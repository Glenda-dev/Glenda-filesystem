use crate::block::BlockReader;
use crate::defs::ext4::*;
use crate::ops::ExtOps;
use crate::versions::ext2::Ext2Ops;
use crate::versions::ext3::Ext3Ops;
use crate::versions::ext4::Ext4Ops;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::slice;
use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::fs::FileSystemJournalService;
use glenda::protocol::fs::{DEntry, OpenFlags, Stat};

pub struct ExtFs {
    reader: BlockReader,
    sb: SuperBlock,
    block_size: u32,
    group_desc_size: u16,
    inodes_per_group: u32,
    ops: Arc<dyn ExtOps>,
    ring_vaddr: usize,
    ring_size: usize,
}

use glenda::client::ResourceClient;
use glenda::interface::{MemoryService, ResourceService};
use glenda::ipc::Badge;
use glenda::mem::shm::SharedMemory;

impl ExtFs {
    pub fn new(
        block_device: Endpoint,
        ring_vaddr: usize,
        ring_size: usize,
        res_client: &mut ResourceClient,
    ) -> Result<Self, Error> {
        let mut reader = BlockReader::new(block_device);
        reader.init()?;

        // Setup IoUring
        let sq_entries = 4;
        let cq_entries = 4;
        let notify_slot = glenda::cap::CapPtr::from(0x50);
        // Allocate endpoint for notifications
        res_client.alloc(Badge::null(), glenda::cap::CapType::Endpoint, 0, notify_slot)?;
        let notify_ep = glenda::cap::Endpoint::from(notify_slot);

        // We need a receive window for the ring frame
        let recv_ring_slot = glenda::cap::CapPtr::from(0x51);

        let frame = reader.setup_ring(sq_entries, cq_entries, notify_ep, recv_ring_slot)?;
        res_client.mmap(Badge::null(), frame, ring_vaddr, ring_size)?;

        let ring = unsafe {
            glenda::io::uring::IoUringClient::new(glenda::io::uring::IoUringBuffer::new(
                ring_vaddr as *mut u8,
                ring_size,
                sq_entries,
                cq_entries,
            ))
        };
        reader.set_ring(ring);

        // Request Buffer from Fossil
        let recv_buffer_slot = glenda::cap::CapPtr::from(0x52);
        let (frame, fossil_vaddr, size, paddr) = reader.request_shm(recv_buffer_slot)?;

        // Map to our space at the SAME virtual address as Fossil expects
        // This ensures SQE logic matches what driver expects
        res_client.mmap(Badge::null(), frame.clone(), fossil_vaddr, size)?;

        let mut shm = SharedMemory::new(frame, fossil_vaddr, size);
        shm.set_paddr(paddr as u64);

        reader.set_shm(shm);

        // ... (existing helper logic in new)
        let mut sb_buf = [0u8; 1024];
        reader.read_offset(SUPER_BLOCK_OFFSET, &mut sb_buf)?;

        let sb = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) };
        let magic = sb.s_magic;

        if magic != EXT4_SUPER_MAGIC {
            return Err(Error::InvalidArgs);
        }

        let block_size = 1024 << sb.s_log_block_size;
        let group_desc_size = if (sb.s_feature_incompat & 0x80) != 0 { sb.s_desc_size } else { 32 };

        // Determine OPS based on features
        let ops: Arc<dyn ExtOps> = if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
            // log!("Detected Ext4 with Extents");
            Arc::new(Ext4Ops)
        } else if (sb.s_feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
            // log!("Detected Ext3 (Journaled)");
            Arc::new(Ext3Ops)
        } else {
            // log!("Detected Ext2");
            Arc::new(Ext2Ops)
        };

        Ok(Self {
            reader,
            sb,
            block_size,
            group_desc_size,
            inodes_per_group: sb.s_inodes_per_group,
            ops,
            ring_vaddr,
            ring_size,
        })
    }

    fn read_group_desc(&self, group: u32) -> Result<GroupDesc, Error> {
        let first_bg_block = self.sb.s_first_data_block + 1;
        let offset = (first_bg_block as u64 * self.block_size as u64)
            + (group as u64 * self.group_desc_size as u64);

        let mut buf = [0u8; 64];
        self.reader.read_offset(offset, &mut buf)?;

        // Handling packed struct read safely
        let gd = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const GroupDesc) };
        Ok(gd)
    }

    fn read_inode(&self, ino: u32) -> Result<Inode, Error> {
        if ino < 1 {
            return Err(Error::NotFound);
        }
        let group = (ino - 1) / self.inodes_per_group;
        let index = (ino - 1) % self.inodes_per_group;

        let gd = self.read_group_desc(group)?;

        let table_block = gd.bg_inode_table_lo;

        let inode_size = self.sb.s_inode_size as u64;
        let offset = (table_block as u64 * self.block_size as u64) + (index as u64 * inode_size);

        let mut buf = [0u8; 256];
        self.reader.read_offset(offset, &mut buf)?;

        let inode = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Inode) };
        Ok(inode)
    }

    fn get_block_addr(&self, inode: &Inode, lblock: u32) -> Result<u32, Error> {
        self.ops.get_block_addr(&self.reader, inode, lblock, self.block_size)
    }

    fn resolve_path(&self, path: &str) -> Result<u32, Error> {
        let mut current_ino = ROOT_INO;
        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            current_ino = self.find_entry(current_ino, part)?;
        }
        Ok(current_ino)
    }

    fn find_entry(&self, dir_ino: u32, name: &str) -> Result<u32, Error> {
        let inode = self.read_inode(dir_ino)?;
        if (inode.i_mode & 0xF000) != 0x4000 {
            return Err(Error::DeviceError);
        }

        let size = inode.i_size_lo;
        let mut offset = 0;

        while offset < size {
            let lblock = offset / self.block_size;
            let pblock = self.get_block_addr(&inode, lblock)?;

            let mut block_buf = alloc::vec![0u8; self.block_size as usize];
            let read_offset = pblock as u64 * self.block_size as u64;
            self.reader.read_offset(read_offset, &mut block_buf)?;

            let mut block_offset = 0;
            while block_offset < self.block_size {
                let ptr = unsafe { block_buf.as_ptr().add(block_offset as usize) };
                let de = unsafe { core::ptr::read_unaligned(ptr as *const DirEntry2) };

                if de.inode != 0 {
                    let name_len = de.name_len as usize;
                    let name_slice = unsafe { slice::from_raw_parts(ptr.add(8), name_len) };
                    if name.as_bytes() == name_slice {
                        return Ok(de.inode);
                    }
                }

                block_offset += de.rec_len as u32;
                if de.rec_len == 0 {
                    break;
                }
            }
            offset += self.block_size;
        }

        Err(Error::NotFound)
    }
}

impl FileSystemJournalService for ExtFs {
    fn transaction_start(&mut self, _badge: Badge) -> Result<u64, Error> {
        Ok(1)
    }

    fn transaction_commit(&mut self, _badge: Badge, _tid: u64) -> Result<(), Error> {
        Ok(())
    }

    fn transaction_abort(&mut self, _badge: Badge, _tid: u64) -> Result<(), Error> {
        Ok(())
    }

    fn log_block(
        &mut self,
        _badge: Badge,
        _tid: u64,
        block_num: u64,
        data: &[u8],
    ) -> Result<(), Error> {
        let sector = block_num * (self.block_size as u64 / 512);
        self.reader.write_blocks(sector, data)?;
        Ok(())
    }
}

// ExtFs implementation continues...

impl ExtFs {
    pub fn open_handle(
        &mut self,
        _badge: Badge,
        path: &str,
        _flags: OpenFlags,
        _mode: u32,
    ) -> Result<Box<dyn FileHandleService + Send>, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        let handle = ExtFileHandle {
            ops: self.ops.clone(),
            reader: self.reader.clone(),
            inode,
            block_size: self.block_size,
            pos: 0,
            ring_vaddr: self.ring_vaddr,
            ring_size: self.ring_size,
            uring: None,
            user_shm_base: 0,
            server_shm_base: 0,
        };
        Ok(Box::new(handle))
    }

    pub fn mkdir(&mut self, badge: Badge, _path: &str, _mode: u32) -> Result<(), Error> {
        let tid = self.transaction_start(badge)?;
        self.transaction_commit(badge, tid)?;
        Ok(())
    }

    pub fn unlink(&mut self, badge: Badge, _path: &str) -> Result<(), Error> {
        let tid = self.transaction_start(badge)?;
        self.transaction_commit(badge, tid)?;
        Ok(())
    }

    pub fn stat_path(&mut self, _badge: Badge, path: &str) -> Result<Stat, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        Ok(Stat {
            ino: ino as u64,
            size: inode.i_size_lo as u64,
            mode: inode.i_mode as u32,
            ..Default::default()
        })
    }
}

pub struct ExtFileHandle {
    ops: Arc<dyn ExtOps>,
    reader: BlockReader,
    inode: Inode,
    block_size: u32,
    pos: u64,
    ring_vaddr: usize,
    ring_size: usize,
    uring: Option<glenda::io::uring::IoUringBuffer>,
    user_shm_base: usize,
    server_shm_base: usize,
}

impl FileHandleService for ExtFileHandle {
    fn close(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn stat(&self, _badge: Badge) -> Result<Stat, Error> {
        Ok(Stat {
            size: self.inode.i_size_lo as u64,
            mode: self.inode.i_mode as u32,
            ..Default::default()
        })
    }

    fn read(&mut self, _badge: Badge, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let _start_block_idx = (offset / self.block_size as u64) as u32;
        // let end_block_idx = ((offset + buf.len() as u64 + self.block_size as u64 - 1)
        //     / self.block_size as u64) as u32;

        let mut read_len = 0;
        let mut current_offset = offset;
        let mut buf_ptr = 0;

        // Simple loop
        while buf_ptr < buf.len() {
            let lblock = (current_offset / self.block_size as u64) as u32;
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            let blk_offset_in_buf = (current_offset % self.block_size as u64) as usize;
            let chuck_len =
                core::cmp::min(buf.len() - buf_ptr, self.block_size as usize - blk_offset_in_buf);

            let mut block_data = alloc::vec![0u8; self.block_size as usize];
            if pblock != 0 {
                let read_offset = pblock as u64 * self.block_size as u64;
                self.reader.read_offset(read_offset, &mut block_data)?;
            } else {
                // Sparse block, zeroed
            }

            buf[buf_ptr..buf_ptr + chuck_len]
                .copy_from_slice(&block_data[blk_offset_in_buf..blk_offset_in_buf + chuck_len]);

            read_len += chuck_len;
            current_offset += chuck_len as u64;
            buf_ptr += chuck_len;

            if current_offset >= self.inode.i_size_lo as u64 {
                break;
            }
        }
        Ok(read_len)
    }

    fn write(&mut self, _badge: Badge, offset: u64, buf: &[u8]) -> Result<usize, Error> {
        // Simplified write - assumes no allocation needed for existing blocks or implementing minimal allocation is hard here without FS ref.
        // But writes usually go through FS service for allocation?
        // Wait, `FileHandle::write` is called on the handle. The handle needs access to allocator if extending.
        // `ExtFileHandle` only has `read-only` ops access (get_block_addr).
        // `ExtOps` is just for traversing maps.
        // Real write support needs `allocator` etc.
        // The user said: "write logic can be moved from ExtFs::write_file to here."
        // `ExtFs::write_file` did: get_block_addr (failed if not present?), read, modify, write.
        // It used `self.log_block`. `ExtFs` had `FileSystemJournalService`. `ExtFileHandle` does NOT have `FileSystemJournalService`.
        // So `write` might be difficult without `ExtFs` ref.
        // However, `log_block` calls `reader.write_blocks`.
        // `ExtFileHandle` has `reader` so it can write blocks.
        // But `log_block` was part of `transaction`.
        // If I skip transaction overhead for now (as `write_file` seemed to use it just for locking/logging?), I can just write.

        let mut written = 0;
        let mut current_offset = offset;
        let mut buf_ptr = 0;

        while buf_ptr < buf.len() {
            let lblock = (current_offset / self.block_size as u64) as u32;
            // This fails if block not allocated
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            if pblock == 0 {
                return Err(Error::InternalError); // Cannot allocate in this simple handle
            }

            let blk_offset_in_buf = (current_offset % self.block_size as u64) as usize;
            let chuck_len =
                core::cmp::min(buf.len() - buf_ptr, self.block_size as usize - blk_offset_in_buf);

            // Read
            let mut block_data = alloc::vec![0u8; self.block_size as usize];
            let read_offset = pblock as u64 * self.block_size as u64;
            self.reader.read_offset(read_offset, &mut block_data)?;

            // Modify
            block_data[blk_offset_in_buf..blk_offset_in_buf + chuck_len]
                .copy_from_slice(&buf[buf_ptr..buf_ptr + chuck_len]);

            // Write
            self.reader
                .write_blocks(pblock as u64 * (self.block_size / 512) as u64, &block_data)?;

            written += chuck_len;
            current_offset += chuck_len as u64;
            buf_ptr += chuck_len;
        }

        Ok(written)
    }

    fn getdents(&mut self, _badge: Badge, _count: usize) -> Result<Vec<DEntry>, Error> {
        Err(Error::NotImplemented)
    }

    fn seek(&mut self, _badge: Badge, _offset: i64, _whence: usize) -> Result<u64, Error> {
        Err(Error::NotImplemented)
    }

    fn sync(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn truncate(&mut self, _badge: Badge, _size: u64) -> Result<(), Error> {
        Err(Error::NotImplemented)
    }

    fn setup_iouring(
        &mut self,
        _badge: Badge,
        server_vaddr: usize,
        client_vaddr: usize,
        size: usize,
        frame: Option<glenda::cap::Frame>,
    ) -> Result<(), Error> {
        self.server_shm_base = server_vaddr;
        self.user_shm_base = client_vaddr;
        self.uring = Some(unsafe {
            glenda::io::uring::IoUringBuffer::attach(server_vaddr as *mut u8, size)
        });
        if let Some(f) = frame {
            let shm = glenda::mem::shm::SharedMemory::new(f, server_vaddr, size);
            self.reader.set_shm(shm);
        }
        Ok(())
    }

    fn process_iouring(&mut self, _badge: Badge) -> Result<(), Error> {
        if let Some(ring) = self.uring.take() {
            while let Some(sqe) = ring.pop_sqe() {
                use glenda::io::uring::{IoUringCqe, IOURING_OP_READ};

                let res = match sqe.opcode {
                    IOURING_OP_READ => {
                        let addr = sqe.addr as usize;
                        let len = sqe.len as u32;
                        let offset = sqe.off as u64;

                        if addr < self.user_shm_base {
                            -(Error::InvalidArgs as i32)
                        } else {
                            let server_addr = addr - self.user_shm_base + self.server_shm_base;
                            match self.read_shm_internal(offset, len, server_addr) {
                                Ok(n) => n as i32,
                                Err(e) => -(e as i32),
                            }
                        }
                    }
                    _ => -(Error::NotSupported as i32),
                };

                let cqe = IoUringCqe { user_data: sqe.user_data, res, flags: 0 };
                ring.push_cqe(cqe).ok();
            }
            self.uring = Some(ring);
        }
        Ok(())
    }
}

impl ExtFileHandle {
    fn read_shm_internal(&self, offset: u64, len: u32, shm_vaddr: usize) -> Result<usize, Error> {
        let mut read_len = 0;
        let mut current_offset = offset;
        let mut current_shm_vaddr = shm_vaddr;
        let mut remaining = len as usize;

        while remaining > 0 {
            let lblock = (current_offset / self.block_size as u64) as u32;
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            let blk_offset_in_block = (current_offset % self.block_size as u64) as usize;
            let chunk_len =
                core::cmp::min(remaining, self.block_size as usize - blk_offset_in_block);

            if pblock != 0 {
                let read_offset =
                    pblock as u64 * self.block_size as u64 + blk_offset_in_block as u64;
                self.reader.read_shm(read_offset, chunk_len as u32, current_shm_vaddr)?;
            } else {
                unsafe { core::ptr::write_bytes(current_shm_vaddr as *mut u8, 0, chunk_len) };
            }

            read_len += chunk_len;
            current_offset += chunk_len as u64;
            current_shm_vaddr += chunk_len;
            remaining -= chunk_len;

            if current_offset >= self.inode.i_size_lo as u64 {
                break;
            }
        }
        Ok(read_len)
    }
}
