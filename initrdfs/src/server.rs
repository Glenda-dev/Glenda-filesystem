use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use glenda::cap::{CapPtr, Endpoint, Frame, Reply, CSPACE_CAP, RECV_SLOT};
use glenda::client::{FsClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::system::SystemService;
use glenda::interface::{MemoryService, VirtualFileSystemService};
use glenda::io::uring::{IoUringBuffer, IoUringClient};
use glenda::ipc::server::handle_call;
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::mem::shm::SharedMemory;
use glenda::protocol;
use glenda::protocol::fs::OpenFlags;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::client::block::BlockClient;
use glenda_drivers::interface::BlockDriver;

use crate::fs::{InitrdEntry, InitrdFS};
use crate::layout::{RING_SLOT, SHM_SLOT};

pub struct InitrdServer<'a> {
    blk_client: &'a mut BlockClient,
    res_client: &'a mut ResourceClient,
    vfs_client: &'a mut FsClient,
    fs: Option<InitrdFS>,
    open_files: BTreeMap<usize, Box<dyn FileHandleService + Send>>,
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
        blk_client: &'a mut BlockClient,
        res_client: &'a mut ResourceClient,
        vfs_client: &'a mut FsClient,
    ) -> Self {
        Self {
            blk_client,
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
        self.blk_client.init()?;

        // Use request_shm to let Fossil allocate and manage the buffer.
        // This ensures the buffer is correctly registered with Fossil/Drivers for zero-copy.
        // We use RECV_SLOT to receive the frame cap.
        let (shm, vaddr_fossil, size, paddr) = self.blk_client.request_shm(SHM_SLOT)?;

        // We still need to map it into our address space to access it
        let shm_vaddr = self.next_vaddr;
        self.next_vaddr += size; // Use actual size returned by Fossil
        self.res_client.mmap(Badge::null(), shm, shm_vaddr, size)?;
        // Setup local objects
        // Create SharedMemory with paddr aware
        let mut shm = SharedMemory::new(shm, shm_vaddr, size);
        shm.set_paddr(paddr as u64); // Important!
        shm.set_client_vaddr(vaddr_fossil); // Set the address Fossil expects (client_vaddr from its perspective)

        log!("Mapped SHM into our address space at {:#x}", shm_vaddr);

        // Now setup the ring.
        // This makes Fossil/Driver allocate ring memory and return it in RECV_SLOT.
        // Use 16 entries to ensure it fits within 1 page (4096 bytes).
        let ring = self.blk_client.setup_ring(16, 16, self.endpoint, RING_SLOT)?;
        let ring_size = 4096; // 1 page is enough for 16 entries
        let ring_vaddr = self.next_vaddr;
        self.next_vaddr += ring_size;
        self.res_client.mmap(Badge::null(), ring, ring_vaddr, ring_size)?;

        let ring_buf = unsafe { IoUringBuffer::attach(ring_vaddr as *mut u8, ring_size) };
        let ring = IoUringClient::new(ring_buf);

        self.blk_client.set_shm(shm);
        self.blk_client.set_ring(ring);
        log!("Mapped ring buffer into our address space at {:#x}", ring_vaddr);

        // Read the Initrd header (sector 0)
        let mut header_buf = [0u8; 4096];
        self.blk_client.read_at(0, 4096, &mut header_buf)?;
        log!("Header read complete");

        let magic =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        log!("Magic = {:08x}", magic);
        if magic != 0x99999999 {
            error!("Invalid initrd header magic: {:08x}", magic);
            return Err(Error::InvalidArgs);
        }

        let count = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]])
            as usize;
        log!("File count = {}", count);
        let mut entries = Vec::with_capacity(count);

        let entry_base = 16;
        let entry_size = 48;
        for i in 0..count {
            let offset = entry_base + i * entry_size;
            let type_byte = header_buf[offset];
            let file_offset = u32::from_le_bytes([
                header_buf[offset + 1],
                header_buf[offset + 2],
                header_buf[offset + 3],
                header_buf[offset + 4],
            ]) as u64;
            let file_size = u32::from_le_bytes([
                header_buf[offset + 5],
                header_buf[offset + 6],
                header_buf[offset + 7],
                header_buf[offset + 8],
            ]) as u64;

            let name_bytes = &header_buf[offset + 9..offset + 9 + 32];
            let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
            let name = core::str::from_utf8(&name_bytes[..name_len]).unwrap_or("unknown");

            entries.push(InitrdEntry {
                _type: type_byte,
                name: alloc::string::String::from(name),
                offset: file_offset,
                size: file_size,
            });
        }

        self.fs = Some(InitrdFS::new(self.blk_client.endpoint(), entries, ring_vaddr, ring_size));
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
                    if let Some(mut handle) = s.open_files.remove(&badge_bits) {
                        handle.close(badge)?;
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
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    let len = u_inner.get_mr(0);
                    let offset = u_inner.get_mr(1) as u64;
                    let buf = u_inner.buffer_mut();
                    if len > buf.len() {
                        return Err(Error::InvalidArgs);
                    }
                    let read_len = handle.read(badge, offset, &mut buf[..len])?;
                    Ok(read_len)
                })
            },
            (protocol::FS_PROTO, protocol::fs::SETUP_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
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

                    handle.setup_iouring(badge, addr_server, addr_user, size, frame)?;
                    Ok(())
                })
            },
            (protocol::FS_PROTO, protocol::fs::PROCESS_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_u_inner| {
                    let handle = s.open_files.get_mut(&badge_bits).ok_or(Error::InvalidArgs)?;
                    handle.process_iouring(badge)?;
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
