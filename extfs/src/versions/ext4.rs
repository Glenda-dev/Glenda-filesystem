use super::ext2::Ext2Ops; // Reuse block map logic
use crate::block::BlockReader;
use crate::defs::ext4::{
    Extent, ExtentHeader, ExtentIndex, Inode, EXT4_EXTENTS_FL, EXT4_EXT_MAGIC,
};
use crate::ops::ExtOps;
use core::mem::size_of;
use glenda::error::Error;

pub struct Ext4Ops;

impl Ext4Ops {
    const EXTENT_ROOT_SIZE: usize = 60;

    fn write_header_to_inode(inode: &mut Inode, header: &ExtentHeader) {
        let raw = unsafe {
            core::slice::from_raw_parts(
                header as *const ExtentHeader as *const u8,
                core::mem::size_of::<ExtentHeader>(),
            )
        };
        inode.i_block[..raw.len()].copy_from_slice(raw);
    }

    fn read_extent_at(data: &[u8], idx: usize) -> Extent {
        let off = size_of::<ExtentHeader>() + idx * size_of::<Extent>();
        unsafe { core::ptr::read_unaligned(data.as_ptr().add(off) as *const Extent) }
    }

    fn write_extent_to_inode(inode: &mut Inode, idx: usize, ext: &Extent) {
        let off = size_of::<ExtentHeader>() + idx * size_of::<Extent>();
        let raw = unsafe {
            core::slice::from_raw_parts(
                ext as *const Extent as *const u8,
                core::mem::size_of::<Extent>(),
            )
        };
        inode.i_block[off..off + raw.len()].copy_from_slice(raw);
    }

    fn map_block_extent_create(
        &self,
        inode: &mut Inode,
        lblock: u32,
        alloc_block: &mut dyn FnMut() -> Result<u32, Error>,
    ) -> Result<u32, Error> {
        let max_entries =
            ((Self::EXTENT_ROOT_SIZE - size_of::<ExtentHeader>()) / size_of::<Extent>()) as u16;

        let mut header =
            unsafe { core::ptr::read_unaligned(inode.i_block.as_ptr() as *const ExtentHeader) };
        if header.eh_magic != EXT4_EXT_MAGIC {
            header = ExtentHeader {
                eh_magic: EXT4_EXT_MAGIC,
                eh_entries: 0,
                eh_max: max_entries,
                eh_depth: 0,
                eh_generation: 0,
            };
            Self::write_header_to_inode(inode, &header);
        }

        if header.eh_depth != 0 {
            return Err(Error::NotSupported);
        }

        for i in 0..header.eh_entries as usize {
            let ext = Self::read_extent_at(&inode.i_block, i);
            let ee_len = ext.ee_len as u32;
            if lblock >= ext.ee_block && lblock < ext.ee_block.saturating_add(ee_len) {
                let relative = lblock - ext.ee_block;
                let start = ((ext.ee_start_hi as u64) << 32) | ext.ee_start_lo as u64;
                return Ok((start + relative as u64) as u32);
            }
        }

        if header.eh_entries >= header.eh_max {
            return Err(Error::OutOfMemory);
        }

        let pblock = alloc_block()?;
        let new_ext = Extent {
            ee_block: lblock,
            ee_len: 1,
            ee_start_hi: ((pblock as u64) >> 32) as u16,
            ee_start_lo: pblock,
        };
        let idx = header.eh_entries as usize;
        Self::write_extent_to_inode(inode, idx, &new_ext);
        header.eh_entries = header.eh_entries.saturating_add(1);
        Self::write_header_to_inode(inode, &header);

        Ok(pblock)
    }

    // Helper to binary search extents in a block/buffer
    fn search_extent_block(&self, data: &[u8], lblock: u32) -> Result<usize, Error> {
        // data starts with ExtentHeader
        let header = unsafe { core::ptr::read_unaligned(data.as_ptr() as *const ExtentHeader) };
        if header.eh_magic != EXT4_EXT_MAGIC {
            return Err(Error::DeviceError);
        }

        let depth = header.eh_depth;
        let entries = header.eh_entries as usize;
        let entry_size = size_of::<ExtentIndex>(); // 12 bytes. Extent is also 12 bytes.
        let header_size = size_of::<ExtentHeader>(); // 12 bytes

        // Entries start at offset 12
        // We need to find the entry covering lblock.
        // For internal nodes (depth > 0), keys are ExtentIdx.
        // For leaf nodes (depth == 0), keys are Extent.

        if depth == 0 {
            // Leaf node: array of Extent
            for i in 0..entries {
                let offset = header_size + i * entry_size;
                let extent = unsafe {
                    core::ptr::read_unaligned(data.as_ptr().add(offset) as *const Extent)
                };
                if lblock >= extent.ee_block && lblock < extent.ee_block + extent.ee_len as u32 {
                    let relative = lblock - extent.ee_block;
                    let start_hi = (extent.ee_start_hi as u64) << 32;
                    let start_lo = extent.ee_start_lo as u64;
                    return Ok((start_hi | start_lo) as usize + relative as usize);
                }
            }
        } else {
            // Internal node: array of ExtentIdx
            // We need to find the last index where ei_block <= lblock
            for i in 0..entries {
                let offset = header_size + i * entry_size;
                let idx = unsafe {
                    core::ptr::read_unaligned(data.as_ptr().add(offset) as *const ExtentIndex)
                };

                // Check next entry to see if we should go deeper here
                let next_block = if i + 1 < entries {
                    let next_offset = header_size + (i + 1) * entry_size;
                    let next_idx = unsafe {
                        core::ptr::read_unaligned(
                            data.as_ptr().add(next_offset) as *const ExtentIndex
                        )
                    };
                    next_idx.ei_block
                } else {
                    u32::MAX
                };

                if lblock >= idx.ei_block && lblock < next_block {
                    let leaf_block_hi = (idx.ei_leaf_hi as u64) << 32;
                    let leaf_block_lo = idx.ei_leaf_lo as u64;
                    return Ok((leaf_block_hi | leaf_block_lo) as usize);
                }
            }
        }

        Ok(0) // Not found (sparse)
    }
}

impl ExtOps for Ext4Ops {
    fn get_block_addr(
        &self,
        reader: &BlockReader,
        inode: &Inode,
        lblock: u32,
        block_size: u32,
    ) -> Result<u32, Error> {
        if (inode.i_flags & EXT4_EXTENTS_FL) == 0 {
            return Ext2Ops::get_block_addr_map(reader, inode, lblock, block_size);
        }

        // Extents
        // i_block[0..60] contains the root node (Header + entries)
        let root_data = &inode.i_block; // [u8; 60]

        let header =
            unsafe { core::ptr::read_unaligned(root_data.as_ptr() as *const ExtentHeader) };
        if header.eh_magic != EXT4_EXT_MAGIC {
            return Err(Error::DeviceError);
        }

        let mut current_block_data = [0u8; 4096]; // Buffer for tree traversal
                                                  // Need to be careful about block size here.
        if block_size > 4096 {
            return Err(Error::MessageTooLong);
        }

        // Initial check on root
        // We can reuse search_extent_block logic but root is in memory, not block.
        // Let's manually handle root.

        let depth = header.eh_depth;

        // If depth == 0, root is leaf
        if depth == 0 {
            let physical = self.search_extent_block(root_data, lblock)?;
            return Ok(physical as u32);
        }

        // BFS/DFS down
        // Root is internal
        let next_block_phys = self.search_extent_block(root_data, lblock)?;
        if next_block_phys == 0 {
            return Ok(0);
        } // Hole

        let mut curr_phys = next_block_phys;
        let mut curr_depth = depth;

        while curr_depth > 0 {
            reader.read_offset(
                curr_phys * block_size as usize,
                &mut current_block_data[0..block_size as usize],
            )?;

            // Now current_block_data has the node
            // verify magic?
            let next =
                self.search_extent_block(&current_block_data[0..block_size as usize], lblock)?;
            if next == 0 {
                return Ok(0);
            }

            curr_phys = next;
            curr_depth -= 1;
        }

        // Found physical block of data
        Ok(curr_phys as u32)
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
        if (inode.i_flags & EXT4_EXTENTS_FL) != 0 {
            let mapped = self.get_block_addr(reader, inode, lblock, block_size)?;
            if mapped != 0 {
                return Ok(mapped);
            }

            if create {
                return self.map_block_extent_create(inode, lblock, alloc_block);
            }
            return Ok(0);
        }

        <Ext2Ops as ExtOps>::map_block(
            &Ext2Ops,
            reader,
            inode,
            lblock,
            block_size,
            create,
            alloc_block,
        )
    }
}
