use crate::block::BlockReader;
use crate::defs::ext4::*;
use glenda::error::Error;

pub trait ExtOps: Send + Sync {
    fn get_block_addr(
        &self,
        reader: &BlockReader,
        inode: &Inode,
        lblock: u32,
        block_size: u32,
    ) -> Result<u32, Error>;
}
