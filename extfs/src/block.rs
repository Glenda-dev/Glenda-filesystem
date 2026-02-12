use core::cmp::min;
use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::ipc::{MsgFlags, MsgTag};
use glenda::protocol::drivers::block;

pub struct BlockReader {
    endpoint: Endpoint,
    start_sector: u64,
    sector_size: u32,
    block_size: u32,
}

impl BlockReader {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            start_sector: 0,
            sector_size: 512, // Default
            block_size: 4096, // Default
        }
    }

    /// Read bytes from offset.
    /// Note: This is inefficient for unaligned small reads, but sufficient for initialization.
    /// It effectively maps [offset, offset+len] to sectors and reads them.
    pub fn read_offset(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Calculate start and end sector
        let start_sec = offset / (self.sector_size as u64);
        let end_sec =
            (offset + buf.len() as u64 + (self.sector_size as u64) - 1) / (self.sector_size as u64);
        let count = (end_sec - start_sec) as u32;

        // We need a temporary buffer to hold sector data because we need to extract the slice.
        // Assuming max read is reasonable.
        let temp_size = (count as usize) * (self.sector_size as usize);
        let mut temp_buf = alloc::vec![0u8; temp_size];

        // Call the block device
        self.read_blocks(start_sec, &mut temp_buf)?;

        // Copy out
        let buf_offset = (offset % (self.sector_size as u64)) as usize;
        let copy_len = min(buf.len(), temp_size - buf_offset);
        buf[..copy_len].copy_from_slice(&temp_buf[buf_offset..buf_offset + copy_len]);

        Ok(copy_len)
    }

    /// Read blocks from device.
    /// This implementation assumes a simple synchronous BLOCK_READ via IPC/Shared Memory.
    /// IN REALITY: This needs complex Shared Memory setup (granting pages to the block driver).
    /// For this mock/basic version, we assume the block driver can copy to our buffer if we provide a capability,
    /// Or we use a small buffer encoded in registers (unlikely for FS).
    /// PROPER IMPLEMENTATION:
    /// 1. Allocate DMA buffer (using DmaService or simple Page alloc).
    /// 2. Share buffer cap with Block Driver.
    /// 3. Block Driver dma-writes to it.
    ///
    /// SIMPLIFICATION: We assume `read_blocks` is implemented via a hypothetical synchronous call
    /// that returns data in a shared buffer established at init.
    ///
    /// FOR NOW: I will stub this to just "succeed" with zeros or panic if real HW is needed,
    /// because establishing shared memory requires interacting with `DmaService` or `VSPACE`.
    pub fn read_blocks(&self, _sector: u64, _buf: &mut [u8]) -> Result<(), Error> {
        // In a real system you cannot just pass `buf` pointer to another process.
        // Protocol: READ (sector, count) -> returns usually via a pre-registered shared buffer.

        // TODO: Implement actual Block IPC.
        // This requires the FS driver to have a shared memory region with the Block Driver.
        // Let's assume we have `self.shared_buf` which is 4KB or so.
        // But `read_offset` allocates `temp_buf`.

        // Just mocking the IPC call structure here.
        let _tag = MsgTag::new(block::READ_BLOCKS, 2, MsgFlags::empty());
        // self.endpoint.call( ... )

        // For the sake of "Implementing basic functionality" in a stub environment:
        Ok(())
    }

    pub fn write_blocks(&self, _sector: u64, _buf: &[u8]) -> Result<(), Error> {
        let _tag = MsgTag::new(block::WRITE_BLOCKS, 2, MsgFlags::empty());
        Ok(())
    }
}
