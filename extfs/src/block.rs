extern crate alloc;

use glenda::cap::Endpoint;
use glenda::client::volume::VolumeClient;
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::io::uring::IoUringClient;
use glenda::io::uring::RingParams;
use glenda::mem::shm::SharedMemory;
use glenda::mem::shm::ShmParams;
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

pub struct BlockReader {
    client: VolumeClient,
}

impl BlockReader {
    fn device_block_size(&self) -> Result<usize, Error> {
        let block_size = self.client.block_size() as usize;
        if block_size == 0 {
            return Err(Error::NotInitialized);
        }
        Ok(block_size)
    }

    pub fn new(
        endpoint: Endpoint,
        res_client: &mut ResourceClient,
        ring_params: RingParams,
        shm_params: ShmParams,
    ) -> Self {
        Self { client: VolumeClient::new(endpoint, res_client, ring_params, shm_params) }
    }

    pub fn init(
        &mut self,
        vspace: &mut VSpaceManager,
        cspace: &mut CSpaceManager,
    ) -> Result<(), Error> {
        self.client.connect(vspace, cspace)
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

    /// Read bytes from offset.
    pub fn read_offset(&self, offset: usize, buf: &mut [u8]) -> Result<usize, Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        let block_size = self.device_block_size()?;
        let start_pos = offset;
        let end_pos = start_pos + buf.len() as usize;

        let start_sector = start_pos / block_size;
        let end_sector = (end_pos + block_size - 1) / block_size;
        let sector_count = end_sector - start_sector;
        let read_size = sector_count * block_size;

        if start_pos % block_size == 0 && buf.len() as usize == read_size {
            self.client.read_at(start_sector, buf.len() as u32, buf)?;
        } else {
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(read_size as usize, 0u8);
            self.client.read_at(start_sector, read_size as u32, &mut temp_buf)?;
            let copy_start = (start_pos % block_size) as usize;
            buf.copy_from_slice(&temp_buf[copy_start..copy_start + buf.len()]);
        }
        Ok(buf.len())
    }

    /// Write bytes at offset.
    pub fn write_offset(&self, offset: usize, buf: &[u8]) -> Result<usize, Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        let block_size = self.device_block_size()?;
        let start_pos = offset;
        let end_pos = start_pos + buf.len();

        let start_sector = start_pos / block_size;
        let end_sector = (end_pos + block_size - 1) / block_size;
        let sector_count = end_sector - start_sector;
        let write_size = sector_count * block_size;

        if start_pos % block_size == 0 && buf.len() == write_size {
            self.client.write_at(start_sector, buf.len() as u32, buf)?;
        } else {
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(write_size, 0u8);
            self.client.read_at(start_sector, write_size as u32, &mut temp_buf)?;
            let copy_start = start_pos % block_size;
            temp_buf[copy_start..copy_start + buf.len()].copy_from_slice(buf);
            self.client.write_at(start_sector, write_size as u32, &temp_buf)?;
        }

        Ok(buf.len())
    }

    pub fn read_shm(&self, offset: usize, len: u32, shm_vaddr: usize) -> Result<(), Error> {
        if len == 0 {
            return Ok(());
        }

        let block_size = self.device_block_size()?;
        let len_usize = len as usize;
        let start_pos = offset;
        let end_pos = start_pos + len_usize;

        let start_sector = start_pos / block_size;
        let end_sector = (end_pos + block_size - 1) / block_size;
        let sector_count = end_sector - start_sector;
        let read_size = sector_count * block_size;

        if start_pos % block_size == 0 && len_usize == read_size {
            self.client.read_shm(start_sector, len, shm_vaddr)
        } else {
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(read_size, 0u8);
            self.client.read_at(start_sector, read_size as u32, &mut temp_buf)?;
            let copy_start = start_pos % block_size;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    temp_buf[copy_start..copy_start + len_usize].as_ptr(),
                    shm_vaddr as *mut u8,
                    len_usize,
                );
            }
            Ok(())
        }
    }

    /// `sector` uses 512-byte logical sectors.
    pub fn write_blocks(&self, sector: usize, buf: &[u8]) -> Result<(), Error> {
        let dev_block_size = self.device_block_size()?;
        let start_pos = sector * 512;
        let end_pos = start_pos + buf.len() as usize;

        let start_sector = start_pos / dev_block_size;
        let end_sector = (end_pos + dev_block_size - 1) / dev_block_size;
        let sector_count = end_sector - start_sector;
        let read_size = sector_count * dev_block_size;

        if start_pos % dev_block_size == 0 && buf.len() as usize == read_size {
            self.client.write_at(start_sector, buf.len() as u32, buf)
        } else {
            let mut temp_buf = alloc::vec::Vec::new();
            temp_buf.resize(read_size as usize, 0u8);
            self.client.read_at(start_sector, read_size as u32, &mut temp_buf)?;
            let copy_start = (start_pos % dev_block_size) as usize;
            temp_buf[copy_start..copy_start + buf.len()].copy_from_slice(buf);
            self.client.write_at(start_sector, read_size as u32, &temp_buf)
        }
    }
}

impl Clone for BlockReader {
    fn clone(&self) -> Self {
        Self { client: self.client.clone() }
    }
}
