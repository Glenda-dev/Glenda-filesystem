use crate::fs::FatFs;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::system::SystemService;
use glenda::ipc::server::handle_call;
use glenda::ipc::{MsgTag, UTCB};
use glenda::protocol;
use glenda::protocol::fs::OpenFlags;
use glenda::protocol::{FS_PROTO, PROCESS_PROTO};

pub struct FatFsService {
    fs: Option<FatFs>,
    handles: BTreeMap<usize, Box<dyn FileHandleService + Send>>,
    next_handle_id: usize,
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    running: bool,
    ring_vaddr: usize,
    ring_size: usize,
}

const RECV_SLOT: CapPtr = CapPtr::from(0x100);

impl FatFsService {
    pub fn new(ring_vaddr: usize, ring_size: usize) -> Self {
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
        }
    }

    pub fn init_fs(
        &mut self,
        block_device: Endpoint,
        res_client: &mut ResourceClient,
    ) -> Result<(), Error> {
        // Initialize FatFs with the block device
        self.fs = Some(FatFs::new(block_device, self.ring_vaddr, self.ring_size, res_client)?);
        Ok(())
    }
}

impl SystemService for FatFsService {
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
        glenda::ipc_dispatch! {
            self, utcb,
            (FS_PROTO, protocol::fs::OPEN) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let flags = OpenFlags::from_bits_truncate(u_inner.get_mr(0));
                    let mode = u_inner.get_mr(1) as u32;
                    let path = "mock_path"; // TODO

                    let handle = fs.open_handle(path, flags, mode)?;
                    let id = s.next_handle_id;
                    s.next_handle_id += 1;
                    s.handles.insert(id, handle);
                    u_inner.set_mr(0, id);
                    Ok(())
                })
            },
            (FS_PROTO, protocol::fs::MKDIR) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let mode = u_inner.get_mr(0) as u32;
                    let path = "mock_path";
                    fs.mkdir(path, mode)?;
                    Ok(())
                })
            },
            (FS_PROTO, protocol::fs::UNLINK) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = "mock_path";
                    fs.unlink(path)?;
                    Ok(())
                })
            },
            (FS_PROTO, protocol::fs::STAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let fs = s.fs.as_mut().ok_or(Error::NotInitialized)?;
                    let path = "mock_path";
                    let stat = fs.stat_path(path)?;
                    u_inner.set_mr(0, stat.size as usize);
                    u_inner.set_mr(1, stat.mode as usize);
                    Ok(())
                })
            },
            (FS_PROTO, protocol::fs::READ_SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let id = u_inner.get_mr(0);
                    let offset = u_inner.get_mr(1) as u64;
                    let len = u_inner.get_mr(2);
                    let handle = s.handles.get_mut(&id).ok_or(Error::NotFound)?;

                    let mut buf = alloc::vec![0u8; len];
                    let read_len = handle.read(offset, &mut buf)?;
                    u_inner.set_mr(0, read_len);
                    // TODO: copy buffer to UTCB or shared memory
                    Ok(())
                })
            },
            (PROCESS_PROTO, protocol::process::EXIT) => |s: &mut Self, _u: &mut UTCB| {
                s.running = false;
                Ok(())
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
