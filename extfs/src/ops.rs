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

    fn map_block(
        &self,
        reader: &BlockReader,
        inode: &mut Inode,
        lblock: u32,
        block_size: u32,
        create: bool,
        _alloc_block: &mut dyn FnMut() -> Result<u32, Error>,
    ) -> Result<u32, Error> {
        if create {
            Err(Error::NotSupported)
        } else {
            self.get_block_addr(reader, inode, lblock, block_size)
        }
    }
}
