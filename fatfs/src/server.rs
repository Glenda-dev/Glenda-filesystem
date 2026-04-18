use crate::fs::FatFs;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use glenda::cap::{CSPACE_CAP, CapPtr, Endpoint, Reply};
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::system::SystemService;
use glenda::ipc::server::handle_call;
use glenda::ipc::{MsgTag, UTCB};
use glenda::protocol;
use glenda::protocol::fs::OpenFlags;
use glenda::protocol::{FS_PROTO, PROCESS_PROTO};
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

pub struct FatFsService<'a> {
    fs: Option<FatFs>,
    handles: BTreeMap<usize, Box<dyn FileHandleService + Send>>,
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    running: bool,
    ring_vaddr: usize,
    ring_size: usize,

    pub cspace: &'a mut CSpaceManager,
    pub vspace: &'a mut VSpaceManager,
}

impl<'a> FatFsService<'a> {
    pub fn new(
        ring_vaddr: usize,
        ring_size: usize,
        cspace: &'a mut CSpaceManager,
        vspace: &'a mut VSpaceManager,
    ) -> Self {
        Self {
            fs: None,
            handles: BTreeMap::new(),
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
            ring_vaddr,
            ring_size,
            cspace,
            vspace,
        }
    }

    pub fn init_fs(
        &mut self,
        block_device: Endpoint,
        res_client: &mut ResourceClient,
    ) -> Result<(), Error> {
        // Initialize FatFs with the block device
        self.fs = Some(FatFs::new(
            block_device,
            self.ring_vaddr,
            self.ring_size,
            res_client,
            self.vspace,
            self.cspace,
        )?);
        Ok(())
    }

    fn caller_badge_from_badge(badge: glenda::ipc::Badge) -> glenda::ipc::Badge {
        if usize::BITS > 32 {
            let hi = badge.bits() >> 32;
            if hi != 0 {
                glenda::ipc::Badge::new(badge.bits() & 0xffff_ffffusize)
            } else {
                badge
            }
        } else {
            badge
        }
    }
}

impl<'a> SystemService for FatFsService<'a> {
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
            (FS_PROTO, protocol::fs::OPEN) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let flags = OpenFlags::from_bits_truncate(u_inner.get_mr(0));
                    let mode = u_inner.get_mr(1) as u32;
                    let path = unsafe { u_inner.read_str()? };

                    let handle = fs.open_handle(&path, flags, mode)?;
                    s.handles.insert(badge.bits(), handle);
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::MKDIR) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let mode = u_inner.get_mr(0) as u32;
                    let path = unsafe { u_inner.read_str()? };
                    fs.mkdir(&path, mode)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::UNLINK) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    fs.unlink(&path)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::STAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.stat_path(&path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::LSTAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.lstat_path(&path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::READLINK_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let target = fs.readlink_path(&path)?;
                    unsafe { u_inner.write_str(&target)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, protocol::fs::READ_SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let len = u_inner.get_mr(0);
                    let offset = u_inner.get_mr(1) as usize;
                    let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;

                    let read_len = {
                        let cap = core::cmp::min(len, u_inner.buffer_mut().len());
                        let buf = u_inner.buffer_mut();
                        handle.read(caller_badge, offset, &mut buf[..cap])?
                    };
                    u_inner.set_size(read_len);
                    Ok(read_len)
                })
            },
                (FS_PROTO, protocol::fs::WRITE_SYNC) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |u_inner| {
                        let offset = u_inner.get_mr(0) as usize;
                        let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;
                        let written = handle.write(caller_badge, offset, u_inner.buffer())?;
                        Ok(written)
                    })
                },
                (FS_PROTO, protocol::fs::STAT) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |u_inner| {
                        let handle = s.handles.get(&badge.bits()).ok_or(Error::NotFound)?;
                        let stat = handle.stat(caller_badge)?;
                        unsafe { u_inner.write_obj(&stat)? };
                        Ok(0usize)
                    })
                },
                (FS_PROTO, protocol::fs::GETDENTS) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |u_inner| {
                        let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;
                        let count = if u_inner.get_mr(1) != 0 {
                            u_inner.get_mr(1)
                        } else {
                            u_inner.get_mr(0)
                        };
                        let _ = handle.getdents(caller_badge, count)?;
                        Err::<usize, Error>(Error::NotSupported)
                    })
                },
                (FS_PROTO, protocol::fs::SEEK) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |u_inner| {
                        let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;
                        let (offset, whence) = if u_inner.get_mr(2) != 0 || u_inner.get_mr(1) != 0 {
                            (u_inner.get_mr(1) as i64, u_inner.get_mr(2))
                        } else {
                            (u_inner.get_mr(0) as i64, u_inner.get_mr(1))
                        };
                        let pos = handle.seek(caller_badge, offset, whence)?;
                        Ok(pos)
                    })
                },
                (FS_PROTO, protocol::fs::SYNC) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |_u_inner| {
                        let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;
                        handle.sync(caller_badge)?;
                        Ok(0usize)
                    })
                },
                (FS_PROTO, protocol::fs::TRUNCATE) => |s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |u_inner| {
                        let handle = s.handles.get_mut(&badge.bits()).ok_or(Error::NotFound)?;
                        let size = if u_inner.get_mr(1) != 0 {
                            u_inner.get_mr(1)
                        } else {
                            u_inner.get_mr(0)
                        };
                        handle.truncate(caller_badge, size)?;
                        Ok(0usize)
                    })
                },
                (FS_PROTO, protocol::fs::SETUP_IOURING) => |_s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |_u_inner| {
                        let _ = CSPACE_CAP.delete(_s.recv);
                        Err::<usize, Error>(Error::NotSupported)
                    })
                },
                (FS_PROTO, protocol::fs::PROCESS_IOURING) => |_s: &mut Self, u: &mut UTCB| {
                    handle_call(u, |_u_inner| {
                        Err::<usize, Error>(Error::NotSupported)
                    })
                },
            (FS_PROTO, protocol::fs::CLOSE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    s.handles.remove(&badge.bits());
                    Ok(0usize)
                })
            },
            (PROCESS_PROTO, protocol::process::EXIT) => |s: &mut Self, _u: &mut UTCB| {
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
