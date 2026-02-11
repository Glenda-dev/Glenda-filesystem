use crate::fs::FatFs;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::error::Error;
use glenda::interface::fs::FileSystemService;
use glenda::interface::system::SystemService;
use glenda::ipc::server::handle_call;
use glenda::ipc::{MsgTag, UTCB};
use glenda::protocol;
use glenda::protocol::fs::OpenFlags;
use glenda::protocol::{FS_PROTO, PROCESS_PROTO};

pub struct FatFsService {
    fs: Option<FatFs>,
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    running: bool,
}

const RECV_SLOT: CapPtr = CapPtr::from(0x100);

impl FatFsService {
    pub fn new() -> Self {
        Self {
            fs: None,
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
        }
    }

    pub fn init_fs(&mut self, block_device: Endpoint) {
        // Initialize FatFs with the block device
        self.fs = Some(FatFs::new(block_device));
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

                    let cap = fs.open(path, flags, mode)?;
                    u_inner.set_mr(0, cap);
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
