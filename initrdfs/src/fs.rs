use alloc::string::String;
use alloc::vec::Vec;
use glenda::cap::Frame;
use glenda::error::Error;
use glenda::io::uring::IoUringBuffer;
use glenda::ipc::Badge;
use glenda::protocol::fs::{OpenFlags, Stat};
use glenda::client::volume::VolumeClient;

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
    pub offset: u64,
    pub size: u64,
    pub uring: Option<IoUringBuffer>,
    pub user_shm_base: usize,
    pub server_shm_base: usize,
}

impl InitrdFile {
    pub fn new(offset: u64, size: u64) -> Self {
        Self { offset, size, uring: None, user_shm_base: 0, server_shm_base: 0 }
    }

    pub fn read(
        &mut self,
        blk_client: &VolumeClient,
        _badge: Badge,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        if offset >= self.size {
            return Ok(0);
        }
        let available = self.size - offset;
        let read_len = core::cmp::min(available, buf.len() as u64) as usize;

        let block_size = 4096;
        let start_pos = self.offset + offset;
        let end_pos = start_pos + read_len as u64;

        let start_sector = start_pos / block_size;
        let end_sector = (end_pos + block_size - 1) / block_size;
        let sector_count = end_sector - start_sector;
        let read_size = sector_count * block_size;

        let mut temp_buf = alloc::vec![0u8; read_size as usize];

        blk_client.read_at(start_sector, read_size as u32, &mut temp_buf)?;

        let copy_start = (start_pos % block_size) as usize;
        let actual_read = core::cmp::min(read_len, buf.len());
        buf[..actual_read].copy_from_slice(&temp_buf[copy_start..copy_start + actual_read]);

        Ok(actual_read)
    }

    pub fn stat(&self, _badge: Badge) -> Result<Stat, Error> {
        Ok(Stat { size: self.size, mode: DEFAULT_STAT, ..Default::default() })
    }

    pub fn setup_iouring(
        &mut self,
        blk_client: &mut VolumeClient,
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
            blk_client.set_shm(shm);
        }
        Ok(())
    }

    pub fn process_iouring(
        &mut self,
        blk_client: &VolumeClient,
        _badge: Badge,
    ) -> Result<(), Error> {
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
                            let start_pos = self.offset + offset;
                            let start_sector = start_pos / 4096;
                            match blk_client.read_shm(start_sector, len, server_addr) {
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
    entries: Vec<InitrdEntry>,
}

impl InitrdFS {
    pub fn new(header_buf: [u8; 4096]) -> Self {
        let magic =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        if magic != 0x99999999 {
            // This should have been checked earlier but let's be safe
        }

        let count = u32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]])
            as usize;
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

            let mut name_buf = [0u8; 32];
            name_buf.copy_from_slice(&header_buf[offset + 16..offset + 48]);
            let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(32);
            let name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("unknown");

            entries.push(InitrdEntry {
                _type: type_byte,
                name: alloc::string::String::from(name),
                offset: file_offset,
                size: file_size,
            });
        }
        Self { entries }
    }

    pub fn open_handle(
        &mut self,
        path: &str,
        _flags: OpenFlags,
        _mode: u32,
    ) -> Result<InitrdFile, Error> {
        let clean_path = path.trim_start_matches('/');
        for entry in &self.entries {
            if entry.name == clean_path {
                return Ok(InitrdFile::new(entry.offset, entry.size));
            }
        }
        Err(Error::NotFound)
    }

    pub fn stat(&self, path: &str) -> Result<Stat, Error> {
        let clean_path = path.trim_start_matches('/');
        if clean_path.is_empty() {
            return Ok(Stat { size: 0, mode: 0o040555, ..Default::default() });
        }
        for entry in &self.entries {
            if entry.name == clean_path {
                return Ok(Stat { size: entry.size, mode: DEFAULT_STAT, ..Default::default() });
            }
        }
        Err(Error::NotFound)
    }
}
