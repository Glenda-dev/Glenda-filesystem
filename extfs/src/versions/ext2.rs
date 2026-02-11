use crate::block::BlockReader;
use crate::defs::ext4::*;
use crate::ops::ExtOps;
use glenda::error::Error;

pub struct Ext2Ops;

impl Ext2Ops {
    pub fn resolve_indirect(
        reader: &BlockReader,
        block: u32,
        index: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        let offset = block as u64 * block_size as u64 + index as u64 * 4;
        let mut buf = [0u8; 4];
        reader.read_offset(offset, &mut buf)?;
        let data = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const u32) };
        Ok(data)
    }

    pub fn get_block_addr_map(
        reader: &BlockReader,
        inode: &Inode,
        lblock: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        // Cast i_block to [u32; 15]
        let blocks =
            unsafe { core::slice::from_raw_parts(inode.i_block.as_ptr() as *const u32, 15) };

        // Direct blocks 0-11
        if lblock < 12 {
            return Ok(unsafe { core::ptr::read_unaligned(&blocks[lblock as usize]) });
        }

        let ptrs_per_block = block_size / 4;
        let mut remaining = lblock - 12;

        // Indirect block 12
        if remaining < ptrs_per_block {
            let indirect_block = unsafe { core::ptr::read_unaligned(&blocks[12]) };
            if indirect_block == 0 {
                return Ok(0);
            }
            return Self::resolve_indirect(reader, indirect_block, remaining, block_size);
        }
        remaining -= ptrs_per_block;

        // Double indirect block 13
        if remaining < ptrs_per_block * ptrs_per_block {
            let double_indirect = unsafe { core::ptr::read_unaligned(&blocks[13]) };
            if double_indirect == 0 {
                return Ok(0);
            }

            let first_idx = remaining / ptrs_per_block;
            let second_idx = remaining % ptrs_per_block;

            let indirect_block =
                Self::resolve_indirect(reader, double_indirect, first_idx, block_size)?;
            if indirect_block == 0 {
                return Ok(0);
            }

            return Self::resolve_indirect(reader, indirect_block, second_idx, block_size);
        }
        remaining -= ptrs_per_block * ptrs_per_block;

        // Triple indirect block 14
        let triple_indirect = unsafe { core::ptr::read_unaligned(&blocks[14]) };
        if triple_indirect == 0 {
            return Ok(0);
        }

        let first_idx = remaining / (ptrs_per_block * ptrs_per_block);
        remaining %= ptrs_per_block * ptrs_per_block;

        let second_idx = remaining / ptrs_per_block;
        let third_idx = remaining % ptrs_per_block;

        let double_indirect =
            Self::resolve_indirect(reader, triple_indirect, first_idx, block_size)?;
        if double_indirect == 0 {
            return Ok(0);
        }

        let indirect_block =
            Self::resolve_indirect(reader, double_indirect, second_idx, block_size)?;
        if indirect_block == 0 {
            return Ok(0);
        }

        Self::resolve_indirect(reader, indirect_block, third_idx, block_size)
    }
}

impl ExtOps for Ext2Ops {
    fn get_block_addr(
        &self,
        reader: &BlockReader,
        inode: &Inode,
        lblock: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        Self::get_block_addr_map(reader, inode, lblock, block_size)
    }
}
