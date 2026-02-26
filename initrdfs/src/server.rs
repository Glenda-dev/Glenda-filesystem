use alloc::collections::BTreeMap;
use glenda::cap::{CapPtr, Endpoint, Frame, Reply, CSPACE_CAP, RECV_SLOT};
use glenda::client::volume::VolumeClient;
use glenda::client::{FsClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::memory::MemoryService;
use glenda::interface::system::SystemService;
use glenda::interface::VirtualFileSystemService;
use glenda::io::uring::RingParams;
use glenda::ipc::server::handle_call;
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::mem::shm::ShmParams;
use glenda::protocol;
use glenda::protocol::fs::OpenFlags;
use glenda::utils::manager::{CSpaceManager, CSpaceService};

use crate::fs::InitrdFS;
use crate::layout::{RING_SLOT, SHM_SLOT};

pub struct InitrdServer<'a> {
    blk_client: Option<VolumeClient>,
    dev_ep: Endpoint,
    res_client: &'a mut ResourceClient,
    vfs_client: &'a mut FsClient,
    fs: Option<InitrdFS>,
    open_files: BTreeMap<usize, crate::fs::InitrdFile>,
    next_badge: usize,
    next_vaddr: usize,
    endpoint: Endpoint,
    reply: Reply,
    recv: CapPtr,
    running: bool,
    cspace: CSpaceManager,
}

impl<'a> InitrdServer<'a> {
    pub fn new(
        dev_ep: Endpoint,
        res_client: &'a mut ResourceClient,
        vfs_client: &'a mut FsClient,
    ) -> Self {
        Self {
            blk_client: None,
            dev_ep,
            res_client,
            vfs_client,
            fs: None,
            open_files: BTreeMap::new(),
            next_badge: 1,
            next_vaddr: 0x4000_0000,
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
            cspace: CSpaceManager::new(CSPACE_CAP, 16),
        }
    }
}

impl<'a> SystemService for InitrdServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // We use VolumeClient to let Fossil allocate and manage the buffer.
        // This ensures the buffer is correctly registered with Fossil/Drivers for zero-copy.

        let ring_vaddr = self.next_vaddr;
        self.next_vaddr += 4096;
        let shm_vaddr = self.next_vaddr;

        let ring_params = RingParams {
            sq_entries: 16,
            cq_entries: 16,
            notify_ep: self.endpoint,
            recv_slot: RING_SLOT,
            vaddr: ring_vaddr,
            size: 4096,
        };
        let shm_params = ShmParams {
            frame: Frame::from(CapPtr::null()),
            vaddr: shm_vaddr,
            paddr: 0,
            size: 0,
            recv_slot: SHM_SLOT,
        };

        let mut blk_client =
            VolumeClient::new(self.dev_ep, self.res_client, ring_params, shm_params);
        blk_client.connect()?;

        self.blk_client = Some(blk_client);

        log!(
            "Connected to block device and initialized ring: {:#x}, shm: {:#x}",
            ring_vaddr,
            shm_vaddr
        );

        // Read the Initrd header (sector 0)
        let mut header_buf = [0u8; 4096];
        self.blk_client.as_ref().unwrap().read_at(0, 4096, &mut header_buf)?;
        log!("Header read complete");

        self.fs = Some(InitrdFS::new(header_buf));
        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        self.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.vfs_client.mount(Badge::null(), "/", self.endpoint)?;
        self.running = true;
        while self.running {
            let mut utcb = unsafe { UTCB::new() };
            utcb.set_recv_window(RECV_SLOT);
            utcb.set_reply_window(self.reply.cap());

            if let Err(_) = self.endpoint.recv(&mut utcb) {
                continue;
            }

            if let Err(e) = self.dispatch(&mut utcb) {
                utcb.set_msg_tag(MsgTag::err());
                utcb.set_mr(0, e as usize);
            }

            let _ = self.reply(&mut utcb);
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let badge = utcb.get_badge();
        let badge_bits = badge.bits();
        glenda::ipc_dispatch! {
            self, utcb,
            (protocol::FS_PROTO, protocol::fs::OPEN) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let flags = OpenFlags::from_bits_truncate(u_inner.get_mr(0));
                    let mode = u_inner.get_mr(1) as u32;
                    let path = core::str::from_utf8(u_inner.buffer()).map_err(|_| Error::InvalidArgs)?;

                    if let Some(fs) = &mut s.fs {
                        let handle = fs.open_handle(path, flags, mode)?;
                        let badge = s.next_badge;
                        s.next_badge += 1;
                        s.open_files.insert(badge, handle);
                        Ok(badge)
                    } else {
                        Err(Error::NotInitialized)
                    }
                })
            },
            (protocol::FS_PROTO, protocol::fs::STAT_PATH) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let path = core::str::from_utf8(u_inner.buffer()).map_err(|_| Error::InvalidArgs)?;
                    if let Some(fs) = &mut s.fs {
                        let stat = fs.stat(path)?;
                        unsafe { u_inner.write_obj(&stat) }.map_err(|_| Error::Unknown)?;
                        Ok(())
                    } else {
                        Err(Error::NotInitialized)
                    }
                })
            },
            (protocol::FS_PROTO, protocol::fs::CLOSE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    if let Some(_handle) = s.open_files.remove(&badge_bits) {
                        Ok(())
                    } else {
                        Err(Error::InvalidArgs)
                    }
                })
            },
            (protocol::FS_PROTO, protocol::fs::STAT) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    let stat = handle.stat(badge)?;
                    unsafe { u_inner.write_obj(&stat) }.map_err(|_| Error::Unknown)?;
                    Ok(())
                })
            },
            (protocol::FS_PROTO, protocol::fs::READ_SYNC) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let blk_client = s.blk_client.as_ref().ok_or(Error::NotInitialized)?;
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    let len = u_inner.get_mr(0);
                    let offset = u_inner.get_mr(1) as u64;
                    let buf = u_inner.buffer_mut();
                    if len > buf.len() {
                        return Err(Error::InvalidArgs);
                    }
                    let read_len = handle.read(blk_client, badge, offset, &mut buf[..len])?;
                    Ok(read_len)
                })
            },
            (protocol::FS_PROTO, protocol::fs::SETUP_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let blk_client = s.blk_client.as_mut().ok_or(Error::NotInitialized)?;
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    let addr_user = u_inner.get_mr(1);
                    let size = u_inner.get_mr(2);

                    let frame = if u_inner.get_msg_tag().flags().contains(MsgFlags::HAS_CAP) {
                        let slot = s.cspace.alloc(s.res_client)?;
                        CSPACE_CAP.move_cap(RECV_SLOT, slot)?;
                        Some(Frame::from(slot))
                    } else {
                        None
                    };

                    let addr_server = s.next_vaddr;
                    s.next_vaddr += size;

                    if let Some(f) = frame {
                        s.res_client.mmap(Badge::null(), f, addr_server, size)?;
                    }

                    handle.setup_iouring(blk_client, badge, addr_server, addr_user, size, frame)?;
                    Ok(())
                })
            },
            (protocol::FS_PROTO, protocol::fs::PROCESS_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let blk_client = s.blk_client.as_ref().ok_or(Error::NotInitialized)?;
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    handle.process_iouring(blk_client, badge)?;
                    Ok(())
                })
            }
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let tag = utcb.get_msg_tag();
        let reply_tag = MsgTag::new(tag.proto(), tag.label(), MsgFlags::NONE);
        utcb.set_msg_tag(reply_tag);
        let _ = self.reply.reply(utcb);
        Ok(())
    }

    fn stop(&mut self) {
        self.running = false;
    }
}
