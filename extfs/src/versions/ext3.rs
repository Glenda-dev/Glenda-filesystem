use super::ext2::Ext2Ops;
use crate::block::BlockReader;
use crate::defs::ext4::Inode;
use crate::ops::ExtOps;
use glenda::error::Error;

pub struct Ext3Ops;

impl ExtOps for Ext3Ops {
    fn get_block_addr(
        &self,
        reader: &BlockReader,
        inode: &Inode,
        lblock: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        // Ext3 uses generic block mapping (same as Ext2)
        // Journaling is handled at FS layer or separate service
        Ext2Ops::get_block_addr_map(reader, inode, lblock, block_size)
    }
}
