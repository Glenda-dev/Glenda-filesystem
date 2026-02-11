use glenda::cap::Endpoint;
use glenda::error::Error;

// Re-using simplified BlockReader concept from Ext4
pub struct BlockReader {
    #[allow(dead_code)]
    endpoint: Endpoint,
    #[allow(dead_code)]
    sector_size: u32,
    #[allow(dead_code)]
    block_size: u32,
}

impl BlockReader {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint, sector_size: 512, block_size: 4096 }
    }

    pub fn read_offset(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        // Mock implementation
        // Fill fake boot sector if reading 0 for test stability
        if _offset == 0 && buf.len() >= 512 {
            buf.fill(0);
            // Valid signature 0x55AA at offset 510
            buf[510] = 0x55;
            buf[511] = 0xAA;
            // Fake params
            buf[11] = 0x00;
            buf[12] = 0x02; // 512 bytes per sector
            buf[13] = 4; // 4 sectors per cluster
            buf[14] = 1;
            buf[15] = 0; // 1 reserved sector
                         // ...
        }
        Ok(buf.len())
    }

    pub fn write_blocks(&self, _sector: u64, _buf: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}
