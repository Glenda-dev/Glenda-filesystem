use crate::fs::ExtFs;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use glenda::cap::{CapPtr, Endpoint, Reply, CSPACE_CAP};
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::system::SystemService;
use glenda::interface::{CSpaceService, VSpaceService};
use glenda::io::uring::{IoUringBuffer, IoUringCqe, IOURING_OP_READ};
use glenda::ipc::server::handle_call;
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::mem::Perms;
use glenda::protocol::fs::OpenFlags;
use glenda::protocol::process;
use glenda::protocol::{FS_PROTO, PROCESS_PROTO};
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

struct IoUringCtx {
    ring: IoUringBuffer,
    user_shm_base: usize,
    server_shm_base: usize,
    size: usize,
    frame_slot: CapPtr,
}

pub struct Ext4Service<'a> {
    fs: Option<ExtFs>,
    handles: BTreeMap<usize, Box<dyn FileHandleService + Send>>,
    next_handle_id: u32,
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    running: bool,
    ring_vaddr: usize,
    ring_size: usize,
    next_iouring_vaddr: usize,
    iouring_ctx: BTreeMap<usize, IoUringCtx>,

    pub res_client: &'a mut ResourceClient,
    pub cspace: &'a mut CSpaceManager,
    pub vspace: &'a mut VSpaceManager,
}

impl<'a> Ext4Service<'a> {
    pub fn new(
        ring_vaddr: usize,
        ring_size: usize,
        res_client: &'a mut ResourceClient,
        cspace: &'a mut CSpaceManager,
        vspace: &'a mut VSpaceManager,
    ) -> Self {
        Self {
            fs: None,
            handles: BTreeMap::new(),
            next_handle_id: 1,
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
            ring_vaddr,
            ring_size,
            next_iouring_vaddr: ring_vaddr.saturating_add(ring_size),
            iouring_ctx: BTreeMap::new(),
            res_client,
            cspace,
            vspace,
        }
    }

    pub fn init_fs(&mut self, block_device: Endpoint) -> Result<(), Error> {
        let res_client = &mut *self.res_client;
        let vspace = &mut *self.vspace;
        let cspace = &mut *self.cspace;
        self.fs = Some(ExtFs::new(
            block_device,
            self.ring_vaddr,
            self.ring_size,
            res_client,
            vspace,
            cspace,
        )?);
        Ok(())
    }

    fn handle_id_from_badge(badge: Badge) -> usize {
        if usize::BITS > 32 {
            badge.bits() >> 32
        } else {
            badge.bits()
        }
    }

    fn caller_badge_from_badge(badge: Badge) -> Badge {
        if usize::BITS > 32 {
            Badge::new(badge.bits() & 0xffff_ffffusize)
        } else {
            badge
        }
    }

    fn alloc_handle_badge(&mut self, caller_badge: Badge) -> (usize, Badge) {
        let mut handle_id = self.next_handle_id;
        if handle_id == 0 {
            handle_id = 1;
        }
        self.next_handle_id = handle_id.wrapping_add(1);

        let composed = if usize::BITS > 32 {
            let low = caller_badge.bits() & 0xffff_ffffusize;
            ((handle_id as usize) << 32) | low
        } else {
            handle_id as usize
        };
        (handle_id as usize, Badge::new(composed))
    }

    fn release_iouring_ctx(&mut self, handle_id: usize) {
        if let Some(ctx) = self.iouring_ctx.remove(&handle_id) {
            let pages = ctx.size.div_ceil(glenda::arch::mem::PGSIZE);
            let _ = self.vspace.unmap(ctx.server_shm_base, pages);
            let _ = CSPACE_CAP.delete(ctx.frame_slot);
            self.cspace.free(ctx.frame_slot);
        }
    }
}

impl<'a> SystemService for Ext4Service<'a> {
    fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        self.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.running = true;
        while self.running {
            if !self.recv.is_null() {
                // recv_window 必须为空；否则下一次带 cap 的 IPC 会在内核插入时失败。
                let _ = CSPACE_CAP.delete(self.recv);
            }
            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(self.recv);

            if self.endpoint.recv(&mut utcb).is_ok() {
                if let Err(e) = self.dispatch(&mut utcb) {
                    utcb.set_msg_tag(MsgTag::err());
                    utcb.set_mr(0, e as usize);
                }
                let _ = self.reply(&mut utcb);
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let badge = utcb.get_badge();
        let caller_badge = Self::caller_badge_from_badge(badge);
        glenda::ipc_dispatch! {
            self, utcb,
            (FS_PROTO, glenda::protocol::fs::OPEN) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let flags = OpenFlags::from_bits_truncate(u_inner.get_mr(0));
                    let mode = u_inner.get_mr(1) as u32;
                    let path = unsafe { u_inner.read_str()? };
                    let handle_id = Self::handle_id_from_badge(badge);
                    let file_handle = fs.open_handle(caller_badge, &path, flags, mode)?;
                    s.handles.insert(handle_id, file_handle);
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::MKDIR) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let mode = u_inner.get_mr(0) as u32;
                    let path = unsafe { u_inner.read_str()? };
                    fs.mkdir(caller_badge, &path, mode)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::UNLINK) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    fs.unlink(caller_badge, &path)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::STAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.stat_path(caller_badge, &path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::LSTAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.lstat_path(caller_badge, &path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::READLINK_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let target = fs.readlink_path(caller_badge, &path)?;
                    unsafe { u_inner.write_str(&target)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::READ_SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let len = core::cmp::min(u_inner.get_mr(0), glenda::ipc::IPC_BUFFER_SIZE);
                    let offset = u_inner.get_mr(1) as usize;
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;

                    let read_len = {
                        let buf = u_inner.buffer_mut();
                        handle.read(caller_badge, offset, &mut buf[..len])?
                    };
                    u_inner.set_size(read_len);
                    Ok(read_len)
                })
            },
            (FS_PROTO, glenda::protocol::fs::WRITE_SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let offset = u_inner.get_mr(0) as usize;
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    let written = handle.write(caller_badge, offset, u_inner.buffer())?;
                    Ok(written)
                })
            },
            (FS_PROTO, glenda::protocol::fs::STAT) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get(&handle_id).ok_or(Error::NotFound)?;
                    let stat = handle.stat(caller_badge)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::GETDENTS) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    let count = if u_inner.get_mr(1) != 0 {
                        u_inner.get_mr(1)
                    } else {
                        u_inner.get_mr(0)
                    };
                    let _ = handle.getdents(caller_badge, count)?;
                    Err::<usize, Error>(Error::NotSupported)
                })
            },
            (FS_PROTO, glenda::protocol::fs::SEEK) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    let (offset, whence) = if u_inner.get_mr(2) != 0 || u_inner.get_mr(1) != 0 {
                        (u_inner.get_mr(1) as i64, u_inner.get_mr(2))
                    } else {
                        (u_inner.get_mr(0) as i64, u_inner.get_mr(1))
                    };
                    let new_pos = handle.seek(caller_badge, offset, whence)?;
                    Ok(new_pos)
                })
            },
            (FS_PROTO, glenda::protocol::fs::SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    handle.sync(caller_badge)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::TRUNCATE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    let size = if u_inner.get_mr(1) != 0 {
                        u_inner.get_mr(1)
                    } else {
                        u_inner.get_mr(0)
                    };
                    handle.truncate(caller_badge, size)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::CLOSE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    s.release_iouring_ctx(handle_id);
                    s.handles.remove(&handle_id);
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::SETUP_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    if !s.handles.contains_key(&handle_id) {
                        return Err(Error::NotFound);
                    }

                    let size = u_inner.get_mr(0);
                    let user_vaddr = u_inner.get_mr(1);

                    if size == 0 {
                        return Err(Error::InvalidArgs);
                    }

                    if !u_inner.get_msg_tag().flags().contains(MsgFlags::HAS_CAP) {
                        return Err(Error::InvalidArgs);
                    }

                    s.release_iouring_ctx(handle_id);

                    let frame_slot = s.cspace.alloc(s.res_client)?;
                    CSPACE_CAP.transfer_self(s.recv, frame_slot)?;
                    let frame = glenda::cap::Page::from(frame_slot);

                    let pages = size.div_ceil(glenda::arch::mem::PGSIZE);
                    let server_vaddr = s
                        .next_iouring_vaddr
                        .div_ceil(glenda::arch::mem::PGSIZE)
                        .saturating_mul(glenda::arch::mem::PGSIZE);
                    s.next_iouring_vaddr =
                        server_vaddr.saturating_add(pages.saturating_mul(glenda::arch::mem::PGSIZE));

                    s.vspace.map_page(
                        frame,
                        server_vaddr,
                        Perms::READ | Perms::WRITE,
                        pages,
                        s.res_client,
                        s.cspace,
                    )?;

                    let ring = unsafe { IoUringBuffer::attach(server_vaddr as *mut u8, size) };

                    s.iouring_ctx.insert(
                        handle_id,
                        IoUringCtx {
                            ring,
                            user_shm_base: user_vaddr,
                            server_shm_base: server_vaddr,
                            size,
                            frame_slot,
                        },
                    );

                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::PROCESS_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    let handle = s.handles.get_mut(&handle_id).ok_or(Error::NotFound)?;
                    let ctx = s.iouring_ctx.get_mut(&handle_id).ok_or(Error::NotInitialized)?;

                    while let Some(sqe) = ctx.ring.pop_sqe() {
                        let res = match sqe.opcode {
                            IOURING_OP_READ => {
                                let user_addr = sqe.addr as usize;
                                let len = sqe.len as usize;
                                let file_offset = sqe.off as usize;

                                let end_addr = user_addr.checked_add(len).ok_or(Error::InvalidArgs)?;
                                let shm_end =
                                    ctx.user_shm_base.checked_add(ctx.size).ok_or(Error::InvalidArgs)?;
                                if user_addr < ctx.user_shm_base || end_addr > shm_end {
                                    -(Error::InvalidArgs as i32)
                                } else if len == 0 {
                                    0
                                } else {
                                    let server_addr =
                                        user_addr - ctx.user_shm_base + ctx.server_shm_base;
                                    let slice = unsafe {
                                        core::slice::from_raw_parts_mut(server_addr as *mut u8, len)
                                    };
                                    match handle.read(caller_badge, file_offset, slice) {
                                        Ok(read_len) => read_len as i32,
                                        Err(e) => -(e as i32),
                                    }
                                }
                            }
                            _ => -(Error::NotSupported as i32),
                        };

                        let cqe = IoUringCqe { user_data: sqe.user_data, res, flags: 0 };
                        let _ = ctx.ring.push_cqe(cqe);
                    }

                    Ok(0usize)
                })
            },
            (PROCESS_PROTO, process::EXIT) => |s: &mut Self, _u: &mut UTCB| {
                s.running = false;
                Ok(())
            },
            (_, _) => |_s: &mut Self, _u: &mut UTCB| {
                Err(Error::NotSupported)
            }
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.running = false;
    }
}
