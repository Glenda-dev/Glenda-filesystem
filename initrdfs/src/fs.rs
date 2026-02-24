use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use glenda::cap::{Endpoint, Frame};
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::io::uring::IoUringBuffer;
use glenda::ipc::Badge;
use glenda::protocol::fs::{DEntry, OpenFlags, Stat};
use glenda_drivers::client::block::BlockClient;

pub const DEFAULT_STAT: u32 = 0o100444;

#[derive(Clone, Debug)]
pub struct InitrdEntry {
    pub _type: u8,
    pub offset: u64,
    pub size: u64,
    pub name: String,
}

// Represents an open file in Initrd
pub struct InitrdFile {
    offset: u64,
    size: u64,
    blk_client: BlockClient,
    uring: Option<IoUringBuffer>,
    user_shm_base: usize,
    server_shm_base: usize,
}

impl FileHandleService for InitrdFile {
    fn read(&mut self, _badge: Badge, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if offset >= self.size {
            return Ok(0);
        }
        let available = self.size - offset;
        let read_len = core::cmp::min(available, buf.len() as u64) as usize;

        let block_size = 4096;
        let start_pos = self.offset + offset;
        let end_pos = start_pos + read_len as u64;

        let start_block = start_pos / block_size;
        let end_block = (end_pos + block_size - 1) / block_size;
        let block_count = end_block - start_block;
        let read_size = block_count * block_size;

        // Allocate temporary buffer for block-aligned read
        // Since we don't have a large buffer, we process block by block or in chunks
        // But BlockClient uses SHM for transfer. We can just read 4KB (or more) into SHM
        // and copy out what we need.
        // However, read_at copies from SHM to OUR buffer. our buffer is `read_len` size.
        // We need a temp buffer of size `read_size`.
        // Allocating large buffer on stack is bad. Heap allocation (Vec) is OK in userspace.

        let mut temp_buf = alloc::vec![0u8; read_size as usize];

        self.blk_client.read_at(start_block * block_size, read_size as u32, &mut temp_buf)?;

        let copy_start = (start_pos % block_size) as usize;
        buf[..read_len].copy_from_slice(&temp_buf[copy_start..copy_start + read_len]);

        Ok(read_len)
    }

    fn write(&mut self, _badge: Badge, _offset: u64, _buf: &[u8]) -> Result<usize, Error> {
        Err(Error::PermissionDenied)
    }

    fn stat(&self, _badge: Badge) -> Result<Stat, Error> {
        Ok(Stat { size: self.size, mode: DEFAULT_STAT, ..Default::default() })
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
        Err(Error::PermissionDenied)
    }

    fn close(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn setup_iouring(
        &mut self,
        _badge: Badge,
        server_vaddr: usize,
        user_vaddr: usize,
        size: usize,
        frame: Option<Frame>,
    ) -> Result<(), Error> {
        self.server_shm_base = server_vaddr;
        self.user_shm_base = user_vaddr;
        self.uring = Some(unsafe { IoUringBuffer::attach(server_vaddr as *mut u8, size) });
        if let Some(f) = frame {
            let shm = glenda::mem::shm::SharedMemory::new(f, server_vaddr, size);
            self.blk_client.set_shm(shm);
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
                            match self.blk_client.read_shm(self.offset + offset, len, server_addr) {
                                Ok(_) => len as i32,
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

pub struct InitrdFS {
    blk_ep: Endpoint,
    entries: Vec<InitrdEntry>,
    ring_vaddr: usize,
    ring_size: usize,
}

impl InitrdFS {
    pub fn new(
        blk_ep: Endpoint,
        entries: Vec<InitrdEntry>,
        ring_vaddr: usize,
        ring_size: usize,
    ) -> Self {
        Self { blk_ep, entries, ring_vaddr, ring_size }
    }

    pub fn open_handle(
        &mut self,
        path: &str,
        _flags: OpenFlags,
        _mode: u32,
    ) -> Result<Box<dyn FileHandleService + Send>, Error> {
        let entry = self.entries.iter().find(|e| e.name == path).ok_or(Error::NotFound)?;

        let mut blk_client = BlockClient::new(self.blk_ep);
        blk_client.init()?;

        if self.ring_vaddr != 0 {
            let ring_buf =
                unsafe { IoUringBuffer::attach(self.ring_vaddr as *mut u8, self.ring_size) };
            blk_client.set_ring(glenda::io::uring::IoUringClient::new(ring_buf));
        }

        Ok(Box::new(InitrdFile {
            offset: entry.offset,
            size: entry.size,
            blk_client,
            uring: None,
            user_shm_base: 0,
            server_shm_base: 0,
        }))
    }

    pub fn stat(&mut self, path: &str) -> Result<Stat, Error> {
        if let Some(entry) = self.entries.iter().find(|e| e.name == path) {
            Ok(Stat { size: entry.size, mode: DEFAULT_STAT, ..Default::default() })
        } else {
            Err(Error::NotFound)
        }
    }
}
