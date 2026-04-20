use crate::block::BlockReader;
use crate::defs::ext4::*;
use crate::ops::ExtOps;
use glenda::error::Error;

pub struct Ext2Ops;

impl Ext2Ops {
    #[inline]
    fn inode_block_ptr(inode: &Inode, index: usize) -> u32 {
        let off = index * 4;
        u32::from_le_bytes([
            inode.i_block[off],
            inode.i_block[off + 1],
            inode.i_block[off + 2],
            inode.i_block[off + 3],
        ])
    }

    #[inline]
    fn set_inode_block_ptr(inode: &mut Inode, index: usize, value: u32) {
        let off = index * 4;
        let bytes = value.to_le_bytes();
        inode.i_block[off..off + 4].copy_from_slice(&bytes);
    }

    #[inline]
    fn inode_add_allocated_blocks(inode: &mut Inode, block_size: u32, blocks: u32) {
        let sectors_per_block = block_size / 512;
        let add = sectors_per_block.saturating_mul(blocks);
        inode.i_blocks_lo = inode.i_blocks_lo.saturating_add(add);
    }

    #[inline]
    fn read_u32_le(slice: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([slice[off], slice[off + 1], slice[off + 2], slice[off + 3]])
    }

    #[inline]
    fn write_u32_le(slice: &mut [u8], off: usize, value: u32) {
        let bytes = value.to_le_bytes();
        slice[off..off + 4].copy_from_slice(&bytes);
    }

    pub fn resolve_indirect(
        reader: &BlockReader,
        block: u32,
        index: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        let offset = block as usize * block_size as usize + index as usize * 4;
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

    fn map_block(
        &self,
        reader: &BlockReader,
        inode: &mut Inode,
        lblock: u32,
        block_size: u32,
        create: bool,
        alloc_block: &mut dyn FnMut() -> Result<u32, Error>,
    ) -> Result<u32, Error> {
        if !create {
            return Self::get_block_addr_map(reader, inode, lblock, block_size);
        }

        // Direct blocks 0..11
        if lblock < 12 {
            let idx = lblock as usize;
            let current = Self::inode_block_ptr(inode, idx);
            if current != 0 {
                return Ok(current);
            }

            let new_block = alloc_block()?;
            Self::set_inode_block_ptr(inode, idx, new_block);
            Self::inode_add_allocated_blocks(inode, block_size, 1);
            return Ok(new_block);
        }

        // Single indirect only for write-path in this stage.
        let ptrs_per_block = block_size / 4;
        let remaining = lblock - 12;
        if remaining < ptrs_per_block {
            let mut indirect_block = Self::inode_block_ptr(inode, 12);

            if indirect_block == 0 {
                indirect_block = alloc_block()?;
                Self::set_inode_block_ptr(inode, 12, indirect_block);
                Self::inode_add_allocated_blocks(inode, block_size, 1);

                let zero = alloc::vec![0u8; block_size as usize];
                reader.write_offset(indirect_block as usize * block_size as usize, &zero)?;
            }

            let mut buf = alloc::vec![0u8; block_size as usize];
            let table_off = indirect_block as usize * block_size as usize;
            reader.read_offset(table_off, &mut buf)?;

            let ptr_off = remaining as usize * 4;
            let current = Self::read_u32_le(&buf, ptr_off);
            if current != 0 {
                return Ok(current);
            }

            let new_block = alloc_block()?;
            Self::write_u32_le(&mut buf, ptr_off, new_block);
            reader.write_offset(table_off, &buf)?;
            Self::inode_add_allocated_blocks(inode, block_size, 1);
            return Ok(new_block);
        }

        Err(Error::NotSupported)
    }
}
