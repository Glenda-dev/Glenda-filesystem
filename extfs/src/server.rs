use crate::fs::ExtFs;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::system::SystemService;
use glenda::ipc::server::handle_call;
use glenda::ipc::{Badge, MsgTag, UTCB};
use glenda::protocol::fs::OpenFlags;
use glenda::protocol::process;
use glenda::protocol::{FS_PROTO, PROCESS_PROTO};
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

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

    pub res_client: &'a mut ResourceClient,
    pub cspace: &'a mut CSpaceManager,
    pub vspace: &'a mut VSpaceManager,
}

const RECV_SLOT: CapPtr = CapPtr::from(0x100);
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
            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(RECV_SLOT);

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
        glenda::ipc_dispatch! {
            self, utcb,
            (FS_PROTO, glenda::protocol::fs::OPEN) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let flags = OpenFlags::from_bits_truncate(u_inner.get_mr(0));
                    let mode = u_inner.get_mr(1) as u32;
                    let path = unsafe { u_inner.read_str()? };
                    let handle_id = Self::handle_id_from_badge(badge);
                    let file_handle = fs.open_handle(badge, &path, flags, mode)?;
                    s.handles.insert(handle_id, file_handle);
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::MKDIR) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let mode = u_inner.get_mr(0) as u32;
                    let path = unsafe { u_inner.read_str()? };
                    fs.mkdir(badge, &path, mode)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::UNLINK) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    fs.unlink(badge, &path)?;
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::STAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.stat_path(badge, &path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::LSTAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let stat = fs.lstat_path(badge, &path)?;
                    unsafe { u_inner.write_obj(&stat)? };
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::READLINK_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = unsafe { u_inner.read_str()? };
                    let target = fs.readlink_path(badge, &path)?;
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
                        handle.read(badge, offset, &mut buf[..len])?
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
                    let written = handle.write(badge, offset, u_inner.buffer())?;
                    Ok(written)
                })
            },
            (FS_PROTO, glenda::protocol::fs::CLOSE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let handle_id = Self::handle_id_from_badge(badge);
                    s.handles.remove(&handle_id);
                    Ok(0usize)
                })
            },
            (FS_PROTO, glenda::protocol::fs::SETUP_IOURING) => |_s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    Err::<usize, Error>(Error::NotSupported)
                })
            },
            (FS_PROTO, glenda::protocol::fs::PROCESS_IOURING) => |_s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    Err::<usize, Error>(Error::NotSupported)
                })
            },
            (PROCESS_PROTO, process::EXIT) => |s: &mut Self, _u: &mut UTCB| {
                s.running = false;
                Ok(())
            },
            (_, _) => |_s: &mut Self, _u: &mut UTCB| {
                Err(Error::InvalidMethod)
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
