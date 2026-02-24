use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::io::uring::IoUringClient;
use glenda::mem::shm::SharedMemory;
use glenda_drivers::client::block::BlockClient;
use glenda_drivers::interface::BlockDriver;
extern crate alloc;

pub struct BlockReader {
    client: BlockClient,
}

impl BlockReader {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { client: BlockClient::new(endpoint) }
    }

    pub fn init(&mut self) -> Result<(), Error> {
        self.client.init()
    }

    pub fn setup_ring(
        &mut self,
        sq_entries: u32,
        cq_entries: u32,
        notify_ep: Endpoint,
        recv: glenda::cap::CapPtr,
    ) -> Result<glenda::cap::Frame, Error> {
        self.client.setup_ring(sq_entries, cq_entries, notify_ep, recv)
    }

    pub fn request_shm(
        &self,
        recv: glenda::cap::CapPtr,
    ) -> Result<(glenda::cap::Frame, usize, usize, usize), Error> {
        self.client.request_shm(recv)
    }

    pub fn set_shm(&mut self, shm: SharedMemory) {
        self.client.set_shm(shm);
    }

    pub fn set_ring(&mut self, ring: IoUringClient) {
        self.client.set_ring(ring);
    }

    pub fn endpoint(&self) -> Endpoint {
        self.client.endpoint()
    }

    pub fn read_offset(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        let block_size: u64 = 4096;
        let start_pos = offset;
        let end_pos = start_pos + buf.len() as u64;

        let start_block = start_pos / block_size;
        let end_block = (end_pos + block_size - 1) / block_size;
        let block_count = end_block - start_block;
        let read_size = block_count * block_size;

        // Perform aligned read using temporary buffer if necessary
        if start_pos % block_size == 0 && buf.len() as u64 == read_size {
            self.client.read_at(offset, buf.len() as u32, buf)?;
        } else {
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(read_size as usize, 0u8);
            self.client.read_at(start_block * block_size, read_size as u32, &mut temp_buf)?;
            let copy_start = (start_pos % block_size) as usize;
            buf.copy_from_slice(&temp_buf[copy_start..copy_start + buf.len()]);
        }
        Ok(buf.len())
    }

    pub fn read_shm(&self, offset: u64, len: u32, shm_vaddr: usize) -> Result<(), Error> {
        self.client.read_shm(offset, len, shm_vaddr)
    }

    pub fn write_blocks(&self, sector: u64, buf: &[u8]) -> Result<(), Error> {
        let block_size: u64 = 4096;
        let start_pos = sector * 512;
        let end_pos = start_pos + buf.len() as u64;

        let start_block = start_pos / block_size;
        let end_block = (end_pos + block_size - 1) / block_size;
        let block_count = end_block - start_block;
        let read_size = block_count * block_size;

        if start_pos % block_size == 0 && buf.len() as u64 == read_size {
            self.client.write_at(start_pos, buf.len() as u32, buf)
        } else {
            // Read-Modify-Write
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(read_size as usize, 0u8);

            // We can ignore read error if we are overwriting everything? likely not.
            // But if specific block is not initialized... For simplicity always read first.
            self.client.read_at(start_block * block_size, read_size as u32, &mut temp_buf)?;

            let copy_start = (start_pos % block_size) as usize;
            temp_buf[copy_start..copy_start + buf.len()].copy_from_slice(buf);

            self.client.write_at(start_block * block_size, read_size as u32, &temp_buf)
        }
    }
}

impl Clone for BlockReader {
    fn clone(&self) -> Self {
        Self { client: BlockClient::new(self.client.endpoint()) }
    }
}
