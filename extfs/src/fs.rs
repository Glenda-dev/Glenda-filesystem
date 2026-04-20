use crate::block::BlockReader;
use crate::defs::ext4::*;
use crate::layout::{NOTIFY_SLOT, RECV_BUFFER_SLOT, RECV_RING_SLOT};
use crate::ops::ExtOps;
use crate::versions::ext2::Ext2Ops;
use crate::versions::ext3::Ext3Ops;
use crate::versions::ext4::Ext4Ops;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::slice;
use glenda::cap::{Endpoint, Page};
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::fs::FileSystemJournalService;
use glenda::io::uring::RingParams;
use glenda::ipc::Badge;
use glenda::mem::shm::ShmParams;
use glenda::protocol::fs::{seek, DEntry, OpenFlags, Stat};
use glenda::utils::manager::{CSpaceManager, VSpaceManager};

pub struct ExtFs {
    reader: BlockReader,
    sb: SuperBlock,
    block_size: u32,
    group_desc_size: u16,
    inodes_per_group: u32,
    ops: Arc<dyn ExtOps>,
    ring_vaddr: usize,
    ring_size: usize,
    writable: bool,
}

use glenda::client::ResourceClient;
use glenda::interface::ResourceService;

impl ExtFs {
    const S_IFMT: u16 = 0xF000;
    const S_IFDIR: u16 = 0x4000;
    const S_IFREG: u16 = 0x8000;
    const S_IFLNK: u16 = 0xA000;

    fn is_ext4(sb: &SuperBlock) -> bool {
        (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0
    }

    fn ext4_writable(sb: &SuperBlock) -> bool {
        let unsupported_incompat =
            sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_WRITE_UNSUPPORTED_MASK;
        let unsupported_ro = sb.s_feature_ro_compat & EXT4_FEATURE_RO_COMPAT_WRITE_UNSUPPORTED_MASK;
        unsupported_incompat == 0 && unsupported_ro == 0
    }

    fn fs_kind(sb: &SuperBlock) -> &'static str {
        let feature_incompat = sb.s_feature_incompat;
        let feature_compat = sb.s_feature_compat;

        if (feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
            "ext4"
        } else if (feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
            "ext3"
        } else {
            "ext2"
        }
    }

    fn decode_cstr(bytes: &[u8]) -> String {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from(String::from_utf8_lossy(&bytes[..end]).trim())
    }

    fn format_uuid(uuid: [u8; 16]) -> String {
        let mut out = String::with_capacity(36);
        for (idx, b) in uuid.iter().enumerate() {
            if matches!(idx, 4 | 6 | 8 | 10) {
                out.push('-');
            }
            let _ = write!(&mut out, "{:02x}", b);
        }
        out
    }

    fn block_count(sb: &SuperBlock) -> u64 {
        let lo = sb.s_blocks_count_lo as u64;
        let hi = sb.s_blocks_count_hi as u64;
        let feature_incompat = sb.s_feature_incompat;
        if (feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            (hi << 32) | lo
        } else {
            lo
        }
    }

    fn free_block_count(sb: &SuperBlock) -> u64 {
        let lo = sb.s_free_blocks_count_lo as u64;
        let hi = sb.s_free_blocks_count_hi as u64;
        let feature_incompat = sb.s_feature_incompat;
        if (feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            (hi << 32) | lo
        } else {
            lo
        }
    }

    fn group_count(sb: &SuperBlock) -> u32 {
        let blocks_per_group = sb.s_blocks_per_group;
        if blocks_per_group == 0 {
            return 0;
        }
        let blocks = Self::block_count(sb);
        blocks.div_ceil(blocks_per_group as u64) as u32
    }

    fn inode_rec_len(sb: &SuperBlock) -> usize {
        core::cmp::max(sb.s_inode_size as usize, core::mem::size_of::<Inode>())
    }

    fn read_superblock_raw(reader: &BlockReader) -> Result<SuperBlock, Error> {
        let mut sb_buf = [0u8; 1024];
        reader.read_offset(SUPER_BLOCK_OFFSET, &mut sb_buf)?;
        Ok(unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) })
    }

    fn write_superblock_raw(reader: &BlockReader, sb: &SuperBlock) -> Result<(), Error> {
        let mut disk = [0u8; 1024];
        reader.read_offset(SUPER_BLOCK_OFFSET, &mut disk)?;

        let raw = unsafe {
            core::slice::from_raw_parts(
                sb as *const SuperBlock as *const u8,
                core::mem::size_of::<SuperBlock>(),
            )
        };
        let n = core::cmp::min(raw.len(), disk.len());
        disk[..n].copy_from_slice(&raw[..n]);
        reader.write_offset(SUPER_BLOCK_OFFSET, &disk)?;
        Ok(())
    }

    fn group_desc_off(block_size: u32, group_desc_size: u16, group: u32) -> usize {
        let first_bg_block = 1usize;
        (first_bg_block * block_size as usize) + (group as usize * group_desc_size as usize)
    }

    fn read_group_desc_raw(
        reader: &BlockReader,
        block_size: u32,
        group_desc_size: u16,
        group: u32,
    ) -> Result<GroupDesc, Error> {
        let offset = Self::group_desc_off(block_size, group_desc_size, group);
        let mut buf = [0u8; 64];
        reader.read_offset(offset, &mut buf)?;
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const GroupDesc) })
    }

    fn write_group_desc_raw(
        reader: &BlockReader,
        block_size: u32,
        group_desc_size: u16,
        group: u32,
        gd: &GroupDesc,
    ) -> Result<(), Error> {
        let offset = Self::group_desc_off(block_size, group_desc_size, group);
        let mut disk = [0u8; 64];
        reader.read_offset(offset, &mut disk)?;
        let raw = unsafe {
            core::slice::from_raw_parts(
                gd as *const GroupDesc as *const u8,
                core::mem::size_of::<GroupDesc>(),
            )
        };
        let n = core::cmp::min(raw.len(), disk.len());
        disk[..n].copy_from_slice(&raw[..n]);
        reader.write_offset(offset, &disk)?;
        Ok(())
    }

    #[inline]
    fn gd_free_blocks(gd: &GroupDesc, sb: &SuperBlock) -> u32 {
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            ((gd.bg_free_blocks_count_hi as u32) << 16) | gd.bg_free_blocks_count_lo as u32
        } else {
            gd.bg_free_blocks_count_lo as u32
        }
    }

    #[inline]
    fn set_gd_free_blocks(gd: &mut GroupDesc, sb: &SuperBlock, value: u32) {
        gd.bg_free_blocks_count_lo = (value & 0xffff) as u16;
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            gd.bg_free_blocks_count_hi = ((value >> 16) & 0xffff) as u16;
        }
    }

    #[inline]
    fn gd_free_inodes(gd: &GroupDesc, sb: &SuperBlock) -> u32 {
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            ((gd.bg_free_inodes_count_hi as u32) << 16) | gd.bg_free_inodes_count_lo as u32
        } else {
            gd.bg_free_inodes_count_lo as u32
        }
    }

    #[inline]
    fn set_gd_free_inodes(gd: &mut GroupDesc, sb: &SuperBlock, value: u32) {
        gd.bg_free_inodes_count_lo = (value & 0xffff) as u16;
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            gd.bg_free_inodes_count_hi = ((value >> 16) & 0xffff) as u16;
        }
    }

    #[inline]
    fn gd_block_bitmap(gd: &GroupDesc, sb: &SuperBlock) -> u64 {
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            ((gd.bg_block_bitmap_hi as u64) << 32) | gd.bg_block_bitmap_lo as u64
        } else {
            gd.bg_block_bitmap_lo as u64
        }
    }

    #[inline]
    fn gd_inode_bitmap(gd: &GroupDesc, sb: &SuperBlock) -> u64 {
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            ((gd.bg_inode_bitmap_hi as u64) << 32) | gd.bg_inode_bitmap_lo as u64
        } else {
            gd.bg_inode_bitmap_lo as u64
        }
    }

    #[inline]
    fn gd_inode_table(gd: &GroupDesc, sb: &SuperBlock) -> u64 {
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            ((gd.bg_inode_table_hi as u64) << 32) | gd.bg_inode_table_lo as u64
        } else {
            gd.bg_inode_table_lo as u64
        }
    }

    fn set_sb_free_blocks(sb: &mut SuperBlock, value: u64) {
        sb.s_free_blocks_count_lo = (value & 0xffff_ffff) as u32;
        if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0 {
            sb.s_free_blocks_count_hi = ((value >> 32) & 0xffff_ffff) as u32;
        }
    }

    fn alloc_block_raw(
        reader: &BlockReader,
        sb: &mut SuperBlock,
        block_size: u32,
        group_desc_size: u16,
    ) -> Result<u32, Error> {
        let groups = Self::group_count(sb);
        let total_blocks = Self::block_count(sb);
        let blocks_per_group = sb.s_blocks_per_group as usize;

        for group in 0..groups {
            let mut gd = Self::read_group_desc_raw(reader, block_size, group_desc_size, group)?;

            let free_blocks = Self::gd_free_blocks(&gd, sb);
            if free_blocks == 0 {
                continue;
            }

            let bitmap_block = Self::gd_block_bitmap(&gd, sb) as usize;
            let mut bitmap = alloc::vec![0u8; block_size as usize];
            reader.read_offset(bitmap_block * block_size as usize, &mut bitmap)?;

            let group_first =
                sb.s_first_data_block as u64 + group as u64 * sb.s_blocks_per_group as u64;
            if group_first >= total_blocks {
                continue;
            }
            let max_bits = core::cmp::min(blocks_per_group, (total_blocks - group_first) as usize);

            for bit in 0..max_bits {
                let byte_idx = bit / 8;
                let bit_mask = 1u8 << (bit % 8);
                if (bitmap[byte_idx] & bit_mask) == 0 {
                    bitmap[byte_idx] |= bit_mask;
                    reader.write_offset(bitmap_block * block_size as usize, &bitmap)?;

                    let new_gd_free = free_blocks.saturating_sub(1);
                    Self::set_gd_free_blocks(&mut gd, sb, new_gd_free);
                    Self::write_group_desc_raw(reader, block_size, group_desc_size, group, &gd)?;

                    let sb_free = Self::free_block_count(sb).saturating_sub(1);
                    Self::set_sb_free_blocks(sb, sb_free);
                    Self::write_superblock_raw(reader, sb)?;

                    return Ok((group_first + bit as u64) as u32);
                }
            }
        }

        Err(Error::OutOfMemory)
    }

    fn alloc_inode_raw(
        reader: &BlockReader,
        sb: &mut SuperBlock,
        block_size: u32,
        group_desc_size: u16,
    ) -> Result<u32, Error> {
        let groups = Self::group_count(sb);
        let inodes_per_group = sb.s_inodes_per_group as usize;
        let inode_size = Self::inode_rec_len(sb);
        let total_inodes = sb.s_inodes_count as usize;
        let first_ino = core::cmp::max(sb.s_first_ino as usize, ROOT_INO as usize);

        for group in 0..groups {
            let mut gd = match Self::read_group_desc_raw(reader, block_size, group_desc_size, group)
            {
                Ok(gd) => gd,
                Err(e) => {
                    warn!("ExtFS: alloc_inode group {} read_group_desc failed: {:?}", group, e);
                    return Err(e);
                }
            };

            let free_inodes = Self::gd_free_inodes(&gd, sb);
            if free_inodes == 0 {
                continue;
            }

            let bitmap_block = Self::gd_inode_bitmap(&gd, sb) as usize;
            let mut bitmap = alloc::vec![0u8; block_size as usize];
            if let Err(e) = reader.read_offset(bitmap_block * block_size as usize, &mut bitmap) {
                warn!(
                    "ExtFS: alloc_inode group {} read inode bitmap block {} failed: {:?}",
                    group, bitmap_block, e
                );
                return Err(e);
            }

            let group_first_ino = group as usize * inodes_per_group + 1;
            if group_first_ino > total_inodes {
                continue;
            }
            let max_bits = core::cmp::min(inodes_per_group, total_inodes - group_first_ino + 1);

            for bit in 0..max_bits {
                let ino = group_first_ino + bit;
                if ino < first_ino {
                    continue;
                }

                let byte_idx = bit / 8;
                let bit_mask = 1u8 << (bit % 8);
                if (bitmap[byte_idx] & bit_mask) == 0 {
                    bitmap[byte_idx] |= bit_mask;
                    if let Err(e) = reader.write_offset(bitmap_block * block_size as usize, &bitmap)
                    {
                        warn!(
                            "ExtFS: alloc_inode group {} write inode bitmap block {} failed: {:?}",
                            group, bitmap_block, e
                        );
                        return Err(e);
                    }

                    let new_gd_free = free_inodes.saturating_sub(1);
                    Self::set_gd_free_inodes(&mut gd, sb, new_gd_free);
                    if let Err(e) =
                        Self::write_group_desc_raw(reader, block_size, group_desc_size, group, &gd)
                    {
                        warn!(
                            "ExtFS: alloc_inode group {} write group desc failed: {:?}",
                            group, e
                        );
                        return Err(e);
                    }

                    sb.s_free_inodes_count = sb.s_free_inodes_count.saturating_sub(1);
                    if let Err(e) = Self::write_superblock_raw(reader, sb) {
                        warn!(
                            "ExtFS: alloc_inode group {} write superblock failed: {:?}",
                            group, e
                        );
                        return Err(e);
                    }

                    let inode_table = Self::gd_inode_table(&gd, sb) as usize;
                    let inode_off = inode_table * block_size as usize + bit * inode_size;
                    let zero = alloc::vec![0u8; inode_size];
                    if let Err(e) = reader.write_offset(inode_off, &zero) {
                        warn!(
                            "ExtFS: alloc_inode group {} zero inode {} at off {} failed: {:?}",
                            group, ino, inode_off, e
                        );
                        return Err(e);
                    }

                    return Ok(ino as u32);
                }
            }
        }

        Err(Error::OutOfMemory)
    }

    pub fn new(
        block_device: Endpoint,
        ring_vaddr: usize,
        ring_size: usize,
        res_client: &mut ResourceClient,
        vspace: &mut VSpaceManager,
        cspace: &mut CSpaceManager,
    ) -> Result<Self, Error> {
        // 1. Setup IoUring Params
        let sq_entries = 4;
        let cq_entries = 4;
        let notify_slot = NOTIFY_SLOT;
        res_client.alloc(Badge::null(), glenda::cap::CapType::Endpoint, 0, notify_slot)?;
        let notify_ep = glenda::cap::Endpoint::from(notify_slot);
        let recv_ring_slot = RECV_RING_SLOT;
        let recv_buffer_slot = RECV_BUFFER_SLOT;

        let ring_params = RingParams {
            sq_entries,
            cq_entries,
            vaddr: ring_vaddr,
            size: ring_size,
            notify_ep,
            recv_slot: recv_ring_slot,
        };

        let shm_params = ShmParams {
            frame: Page::from(glenda::cap::CapPtr::null()),
            vaddr: 0,
            size: 0,
            paddr: 0,
            recv_slot: recv_buffer_slot,
        };

        // 2. Create reader and init (VolumeClient handles handshake)
        let mut reader = BlockReader::new(block_device, res_client, ring_params, shm_params);
        reader.init(vspace, cspace)?;

        // ... (existing helper logic in new)
        let mut sb_buf = [0u8; 1024];
        reader.read_offset(SUPER_BLOCK_OFFSET, &mut sb_buf)?;

        let sb = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) };
        let magic = sb.s_magic;

        if magic != EXT4_SUPER_MAGIC {
            return Err(Error::InvalidArgs);
        }

        let block_size = 1024 << sb.s_log_block_size;
        let group_desc_size = if (sb.s_feature_incompat & 0x80) != 0 { sb.s_desc_size } else { 32 };

        // Determine OPS based on features
        let ops: Arc<dyn ExtOps> = if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
            // log!("Detected Ext4 with Extents");
            Arc::new(Ext4Ops)
        } else if (sb.s_feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
            // log!("Detected Ext3 (Journaled)");
            Arc::new(Ext3Ops)
        } else {
            // log!("Detected Ext2");
            Arc::new(Ext2Ops)
        };

        let writable = if Self::is_ext4(&sb) {
            let can_write = Self::ext4_writable(&sb);
            if !can_write {
                let incompat = sb.s_feature_incompat;
                let ro_compat = sb.s_feature_ro_compat;
                warn!(
                    "ext4 write path features not fully supported; enabling basic write path anyway: incompat=0x{:08x}, ro_compat=0x{:08x}",
                    incompat, ro_compat
                );
            }
            true
        } else {
            true
        };

        let fs_kind = Self::fs_kind(&sb);
        let volume_name_raw = sb.s_volume_name;
        let volume_name = Self::decode_cstr(&volume_name_raw);
        let volume_name = if volume_name.is_empty() { "<unnamed>" } else { volume_name.as_str() };
        let uuid_raw = sb.s_uuid;
        let volume_uuid = Self::format_uuid(uuid_raw);

        let inode_size = sb.s_inode_size;
        let inodes_count = sb.s_inodes_count;
        let free_inodes_count = sb.s_free_inodes_count;
        let blocks_per_group = sb.s_blocks_per_group;
        let inodes_per_group = sb.s_inodes_per_group;
        let feature_compat = sb.s_feature_compat;
        let feature_incompat = sb.s_feature_incompat;
        let feature_ro_compat = sb.s_feature_ro_compat;
        let blocks_count = Self::block_count(&sb);
        let free_blocks_count = Self::free_block_count(&sb);
        let first_data_block = sb.s_first_data_block;
        let group_count =
            if blocks_per_group == 0 { 0 } else { blocks_count.div_ceil(blocks_per_group as u64) };
        let fs_state = sb.s_state;
        let rev_level = sb.s_rev_level;
        let mnt_count = sb.s_mnt_count;
        let max_mnt_count = sb.s_max_mnt_count;
        let mkfs_time = sb.s_mkfs_time;
        let last_mount_time = sb.s_mtime;
        let last_write_time = sb.s_wtime;
        let last_mounted_raw = sb.s_last_mounted;
        let last_mounted = Self::decode_cstr(&last_mounted_raw);
        let last_mounted =
            if last_mounted.is_empty() { "<unknown>" } else { last_mounted.as_str() };

        log!("mounted {} label=\"{}\" uuid={}", fs_kind, volume_name, volume_uuid);
        log!("block_size={} inode_size={} inodes={} blocks={} free_blocks={} free_inodes={} blocks_per_group={} inodes_per_group={} desc_size={} features(c=0x{:08x},i=0x{:08x},ro=0x{:08x})",
            block_size,
            inode_size,
            inodes_count,
            blocks_count,
            free_blocks_count,
            free_inodes_count,
            blocks_per_group,
            inodes_per_group,
            group_desc_size,
            feature_compat,
            feature_incompat,
            feature_ro_compat
        );
        log!("metadata first_data_block={} block_groups={} rev={} state=0x{:04x} mount_count={}/{} mkfs_time={} last_mount_time={} last_write_time={} last_mounted=\"{}\"",
            first_data_block,
            group_count,
            rev_level,
            fs_state,
            mnt_count,
            max_mnt_count,
            mkfs_time,
            last_mount_time,
            last_write_time,
            last_mounted,
        );

        Ok(Self {
            reader,
            sb,
            block_size,
            group_desc_size,
            inodes_per_group: sb.s_inodes_per_group,
            ops,
            ring_vaddr,
            ring_size,
            writable,
        })
    }

    fn read_group_desc(&self, group: u32) -> Result<GroupDesc, Error> {
        Self::read_group_desc_raw(&self.reader, self.block_size, self.group_desc_size, group)
    }

    fn write_group_desc(&self, group: u32, gd: &GroupDesc) -> Result<(), Error> {
        Self::write_group_desc_raw(&self.reader, self.block_size, self.group_desc_size, group, gd)
    }

    fn read_inode(&self, ino: u32) -> Result<Inode, Error> {
        let (inode, _) = self.read_inode_with_offset(ino)?;
        Ok(inode)
    }

    fn read_inode_with_offset(&self, ino: u32) -> Result<(Inode, usize), Error> {
        if ino < 1 {
            return Err(Error::NotFound);
        }
        let group = (ino - 1) / self.inodes_per_group;
        let index = (ino - 1) % self.inodes_per_group;

        let gd = self.read_group_desc(group)?;

        let table_block = gd.bg_inode_table_lo;

        let inode_size = Self::inode_rec_len(&self.sb);
        let offset =
            (table_block as usize * self.block_size as usize) + (index as usize * inode_size);

        let mut buf = alloc::vec![0u8; inode_size];
        self.reader.read_offset(offset, &mut buf[..])?;

        let inode = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Inode) };
        Ok((inode, offset))
    }

    fn write_inode_at_offset(&self, inode: &Inode, inode_offset: usize) -> Result<(), Error> {
        let inode_size = Self::inode_rec_len(&self.sb);
        let mut disk_inode = alloc::vec![0u8; inode_size];
        self.reader.read_offset(inode_offset, &mut disk_inode)?;

        let inode_raw = unsafe {
            core::slice::from_raw_parts(
                inode as *const Inode as *const u8,
                core::mem::size_of::<Inode>(),
            )
        };
        let copy_len = core::cmp::min(inode_raw.len(), disk_inode.len());
        disk_inode[..copy_len].copy_from_slice(&inode_raw[..copy_len]);

        self.reader.write_offset(inode_offset, &disk_inode)?;
        Ok(())
    }

    fn write_inode_by_number(&self, ino: u32, inode: &Inode) -> Result<(), Error> {
        let (_, inode_offset) = self.read_inode_with_offset(ino)?;
        self.write_inode_at_offset(inode, inode_offset)
    }

    fn alloc_block(&mut self) -> Result<u32, Error> {
        let block = Self::alloc_block_raw(
            &self.reader,
            &mut self.sb,
            self.block_size,
            self.group_desc_size,
        )?;
        Ok(block)
    }

    fn alloc_inode(&mut self) -> Result<u32, Error> {
        let ino = Self::alloc_inode_raw(
            &self.reader,
            &mut self.sb,
            self.block_size,
            self.group_desc_size,
        )?;
        Ok(ino)
    }

    fn get_block_addr(&self, inode: &Inode, lblock: u32) -> Result<u32, Error> {
        self.ops.get_block_addr(&self.reader, inode, lblock, self.block_size)
    }

    fn is_symlink_inode(&self, inode: &Inode) -> bool {
        (inode.i_mode & Self::S_IFMT) == Self::S_IFLNK
    }

    fn read_inode_payload(&self, inode: &Inode) -> Result<Vec<u8>, Error> {
        let size = inode.i_size_lo as usize;
        if size == 0 {
            return Ok(Vec::new());
        }

        if self.is_symlink_inode(inode) && size <= inode.i_block.len() && inode.i_blocks_lo == 0 {
            return Ok(inode.i_block[..size].to_vec());
        }

        let mut out = alloc::vec![0u8; size];
        let mut copied = 0usize;
        while copied < size {
            let lblock = (copied / self.block_size as usize) as u32;
            let pblock = self.get_block_addr(inode, lblock)?;
            let block_off = copied % self.block_size as usize;
            let chunk_len = core::cmp::min(size - copied, self.block_size as usize - block_off);

            if pblock != 0 {
                let mut block_data = alloc::vec![0u8; self.block_size as usize];
                let read_offset = pblock as usize * self.block_size as usize;
                self.reader.read_offset(read_offset, &mut block_data)?;
                out[copied..copied + chunk_len]
                    .copy_from_slice(&block_data[block_off..block_off + chunk_len]);
            }

            copied += chunk_len;
        }

        Ok(out)
    }

    fn resolve_path(&self, path: &str) -> Result<u32, Error> {
        let mut current_ino = ROOT_INO;
        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            current_ino = self.find_entry(current_ino, part)?;
        }
        Ok(current_ino)
    }

    fn find_entry(&self, dir_ino: u32, name: &str) -> Result<u32, Error> {
        let inode = self.read_inode(dir_ino)?;
        if (inode.i_mode & 0xF000) != 0x4000 {
            return Err(Error::DeviceError);
        }

        let size = inode.i_size_lo;
        let mut offset = 0;

        while offset < size {
            let lblock = offset / self.block_size;
            let pblock = self.get_block_addr(&inode, lblock)?;

            let mut block_buf = alloc::vec![0u8; self.block_size as usize];
            let read_offset = pblock as usize * self.block_size as usize;
            self.reader.read_offset(read_offset, &mut block_buf)?;

            let mut block_offset = 0;
            while block_offset < self.block_size {
                let ptr = unsafe { block_buf.as_ptr().add(block_offset as usize) };
                let de = unsafe { core::ptr::read_unaligned(ptr as *const DirEntry2) };

                if de.inode != 0 {
                    let name_len = de.name_len as usize;
                    let name_slice = unsafe { slice::from_raw_parts(ptr.add(8), name_len) };
                    if name.as_bytes() == name_slice {
                        return Ok(de.inode);
                    }
                }

                block_offset += de.rec_len as u32;
                if de.rec_len == 0 {
                    break;
                }
            }
            offset += self.block_size;
        }

        Err(Error::NotFound)
    }

    #[inline]
    fn dir_rec_len(name_len: usize) -> usize {
        (8 + name_len + 3) & !3
    }

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
    fn file_type_from_mode(mode: u16) -> u8 {
        match mode & Self::S_IFMT {
            Self::S_IFDIR => EXT4_FT_DIR,
            Self::S_IFREG => EXT4_FT_REG_FILE,
            _ => EXT4_FT_UNKNOWN,
        }
    }

    fn split_parent_name(path: &str) -> Result<(String, String), Error> {
        if path.is_empty() || path == "/" {
            return Err(Error::InvalidArgs);
        }
        let normalized =
            if path.ends_with('/') && path.len() > 1 { &path[..path.len() - 1] } else { path };
        let slash = normalized.rfind('/').ok_or(Error::InvalidArgs)?;
        let parent =
            if slash == 0 { String::from("/") } else { String::from(&normalized[..slash]) };
        let name = String::from(&normalized[slash + 1..]);
        if name.is_empty() || name.len() > 255 {
            return Err(Error::InvalidArgs);
        }
        Ok((parent, name))
    }

    fn map_inode_block(
        &mut self,
        inode: &mut Inode,
        lblock: u32,
        create: bool,
    ) -> Result<u32, Error> {
        let ops = self.ops.clone();
        let reader = self.reader.clone();
        let block_size = self.block_size;
        let group_desc_size = self.group_desc_size;
        let mut sb = self.sb;
        let mut alloc = || Self::alloc_block_raw(&reader, &mut sb, block_size, group_desc_size);

        let pblock = ops.map_block(&reader, inode, lblock, block_size, create, &mut alloc)?;
        self.sb = sb;
        Ok(pblock)
    }

    fn add_dir_entry(
        &mut self,
        dir_ino: u32,
        name: &str,
        child_ino: u32,
        file_type: u8,
    ) -> Result<(), Error> {
        let (mut dir_inode, dir_inode_off) = self.read_inode_with_offset(dir_ino)?;
        if (dir_inode.i_mode & Self::S_IFMT) != Self::S_IFDIR {
            return Err(Error::InvalidType);
        }

        if self.find_entry(dir_ino, name).is_ok() {
            return Err(Error::AlreadyExists);
        }

        let need = Self::dir_rec_len(name.len());
        let mut dir_size = dir_inode.i_size_lo as usize;
        let block_size = self.block_size as usize;

        let mut lblock = 0usize;
        while lblock * block_size < dir_size {
            let pblock = self.map_inode_block(&mut dir_inode, lblock as u32, false)?;
            if pblock == 0 {
                lblock += 1;
                continue;
            }

            let mut block = alloc::vec![0u8; block_size];
            let block_off = pblock as usize * block_size;
            self.reader.read_offset(block_off, &mut block)?;

            let mut off = 0usize;
            while off + 8 <= block_size {
                let de = unsafe {
                    core::ptr::read_unaligned(block.as_ptr().add(off) as *const DirEntry2)
                };
                let rec_len = de.rec_len as usize;
                if rec_len < 8 || off + rec_len > block_size {
                    return Err(Error::IoError);
                }

                if de.inode == 0 && rec_len >= need {
                    let rec_u16 = rec_len as u16;
                    block[off..off + 4].copy_from_slice(&child_ino.to_le_bytes());
                    block[off + 4..off + 6].copy_from_slice(&rec_u16.to_le_bytes());
                    block[off + 6] = name.len() as u8;
                    block[off + 7] = file_type;
                    block[off + 8..off + 8 + name.len()].copy_from_slice(name.as_bytes());
                    self.reader.write_offset(block_off, &block)?;
                    self.write_inode_at_offset(&dir_inode, dir_inode_off)?;
                    return Ok(());
                }

                if de.inode != 0 {
                    let used = Self::dir_rec_len(de.name_len as usize);
                    if rec_len >= used + need {
                        let new_off = off + used;
                        let tail = rec_len - used;

                        let used_u16 = used as u16;
                        block[off + 4..off + 6].copy_from_slice(&used_u16.to_le_bytes());

                        let tail_u16 = tail as u16;
                        block[new_off..new_off + 4].copy_from_slice(&child_ino.to_le_bytes());
                        block[new_off + 4..new_off + 6].copy_from_slice(&tail_u16.to_le_bytes());
                        block[new_off + 6] = name.len() as u8;
                        block[new_off + 7] = file_type;
                        block[new_off + 8..new_off + 8 + name.len()]
                            .copy_from_slice(name.as_bytes());

                        self.reader.write_offset(block_off, &block)?;
                        self.write_inode_at_offset(&dir_inode, dir_inode_off)?;
                        return Ok(());
                    }
                }

                off += rec_len;
            }

            lblock += 1;
        }

        // Need a new directory data block.
        let new_lblock = dir_size.div_ceil(block_size);
        let pblock = self.map_inode_block(&mut dir_inode, new_lblock as u32, true)?;
        if pblock == 0 {
            return Err(Error::OutOfMemory);
        }

        let mut block = alloc::vec![0u8; block_size];
        let rec_u16 = block_size as u16;
        block[0..4].copy_from_slice(&child_ino.to_le_bytes());
        block[4..6].copy_from_slice(&rec_u16.to_le_bytes());
        block[6] = name.len() as u8;
        block[7] = file_type;
        block[8..8 + name.len()].copy_from_slice(name.as_bytes());
        self.reader.write_offset(pblock as usize * block_size, &block)?;

        dir_size = core::cmp::max(dir_size, (new_lblock + 1) * block_size);
        dir_inode.i_size_lo = dir_size as u32;
        self.write_inode_at_offset(&dir_inode, dir_inode_off)?;
        Ok(())
    }

    fn remove_dir_entry(&mut self, dir_ino: u32, name: &str) -> Result<u32, Error> {
        let (dir_inode, dir_inode_off) = self.read_inode_with_offset(dir_ino)?;
        if (dir_inode.i_mode & Self::S_IFMT) != Self::S_IFDIR {
            return Err(Error::InvalidType);
        }

        let dir_size = dir_inode.i_size_lo as usize;
        let block_size = self.block_size as usize;
        let mut cursor = 0usize;

        while cursor < dir_size {
            let lblock = (cursor / block_size) as u32;
            let pblock = self.get_block_addr(&dir_inode, lblock)?;
            if pblock == 0 {
                cursor = ((cursor / block_size) + 1) * block_size;
                continue;
            }

            let mut block = alloc::vec![0u8; block_size];
            let block_off = pblock as usize * block_size;
            self.reader.read_offset(block_off, &mut block)?;

            let mut off = cursor % block_size;
            let mut prev_off: Option<usize> = None;
            while off + 8 <= block_size {
                let de = unsafe {
                    core::ptr::read_unaligned(block.as_ptr().add(off) as *const DirEntry2)
                };
                let rec_len = de.rec_len as usize;
                if rec_len < 8 || off + rec_len > block_size {
                    return Err(Error::IoError);
                }

                if de.inode != 0 {
                    let name_len = de.name_len as usize;
                    if name_len == name.len() {
                        let entry_name = &block[off + 8..off + 8 + name_len];
                        if entry_name == name.as_bytes() {
                            let removed_ino = de.inode;
                            if let Some(poff) = prev_off {
                                let pde = unsafe {
                                    core::ptr::read_unaligned(
                                        block.as_ptr().add(poff) as *const DirEntry2
                                    )
                                };
                                let merged = (pde.rec_len as usize).saturating_add(rec_len) as u16;
                                block[poff + 4..poff + 6].copy_from_slice(&merged.to_le_bytes());
                            } else {
                                block[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                            }

                            self.reader.write_offset(block_off, &block)?;
                            self.write_inode_at_offset(&dir_inode, dir_inode_off)?;
                            return Ok(removed_ino);
                        }
                    }
                    prev_off = Some(off);
                }

                off += rec_len;
            }

            cursor = ((cursor / block_size) + 1) * block_size;
        }

        Err(Error::NotFound)
    }

    fn is_dir_empty(&self, dir_ino: u32) -> Result<bool, Error> {
        let dir_inode = self.read_inode(dir_ino)?;
        if (dir_inode.i_mode & Self::S_IFMT) != Self::S_IFDIR {
            return Err(Error::InvalidType);
        }

        let size = dir_inode.i_size_lo as usize;
        let mut cursor = 0usize;
        let block_size = self.block_size as usize;

        while cursor < size {
            let lblock = (cursor / block_size) as u32;
            let pblock = self.get_block_addr(&dir_inode, lblock)?;
            if pblock == 0 {
                cursor = ((cursor / block_size) + 1) * block_size;
                continue;
            }

            let mut block = alloc::vec![0u8; block_size];
            self.reader.read_offset(pblock as usize * block_size, &mut block)?;

            let mut off = cursor % block_size;
            while off + 8 <= block_size {
                let de = unsafe {
                    core::ptr::read_unaligned(block.as_ptr().add(off) as *const DirEntry2)
                };
                let rec_len = de.rec_len as usize;
                if rec_len < 8 || off + rec_len > block_size {
                    return Err(Error::IoError);
                }

                if de.inode != 0 {
                    let name_len = de.name_len as usize;
                    let is_dot = name_len == 1 && block[off + 8] == b'.';
                    let is_dotdot =
                        name_len == 2 && block[off + 8] == b'.' && block[off + 9] == b'.';
                    if !is_dot && !is_dotdot {
                        return Ok(false);
                    }
                }

                off += rec_len;
            }

            cursor = ((cursor / block_size) + 1) * block_size;
        }

        Ok(true)
    }
}

impl FileSystemJournalService for ExtFs {
    fn transaction_start(&mut self, _badge: Badge) -> Result<usize, Error> {
        Ok(1)
    }

    fn transaction_commit(&mut self, _badge: Badge, _tid: usize) -> Result<(), Error> {
        Ok(())
    }

    fn transaction_abort(&mut self, _badge: Badge, _tid: usize) -> Result<(), Error> {
        Ok(())
    }

    fn log_block(
        &mut self,
        _badge: Badge,
        _tid: usize,
        block_num: usize,
        data: &[u8],
    ) -> Result<(), Error> {
        let sector = block_num * (self.block_size as usize / 512);
        self.reader.write_blocks(sector, data)?;
        Ok(())
    }
}

// ExtFs implementation continues...

impl ExtFs {
    pub fn open_handle(
        &mut self,
        _badge: Badge,
        path: &str,
        flags: OpenFlags,
        mode: u32,
    ) -> Result<Box<dyn FileHandleService + Send>, Error> {
        let access_mode = flags.bits() & 0o3;
        let can_write =
            access_mode == OpenFlags::O_WRONLY.bits() || access_mode == OpenFlags::O_RDWR.bits();
        let can_read = access_mode != OpenFlags::O_WRONLY.bits();

        if can_write && !self.writable {
            return Err(Error::NotSupported);
        }

        let ino = match self.resolve_path(path) {
            Ok(ino) => {
                if flags.contains(OpenFlags::O_CREAT) && flags.contains(OpenFlags::O_EXCL) {
                    return Err(Error::AlreadyExists);
                }
                ino
            }
            Err(Error::NotFound) => {
                if !flags.contains(OpenFlags::O_CREAT) {
                    return Err(Error::NotFound);
                }

                if !self.writable {
                    return Err(Error::NotSupported);
                }

                let (parent_path, file_name) = Self::split_parent_name(path)?;
                warn!(
                    "ExtFS: creating path={}, parent_path={}, file_name={}",
                    path, parent_path, file_name
                );

                let parent_ino = self.resolve_path(&parent_path)?;
                let parent_inode = self.read_inode(parent_ino)?;
                if (parent_inode.i_mode & Self::S_IFMT) != Self::S_IFDIR {
                    return Err(Error::InvalidType);
                }

                let new_ino = match self.alloc_inode() {
                    Ok(ino) => ino,
                    Err(e) => {
                        warn!("ExtFS: alloc_inode failed for {}: {:?}", path, e);
                        return Err(e);
                    }
                };
                warn!("ExtFS: allocated inode {} for {}", new_ino, path);

                let mut new_inode = match self.read_inode(new_ino) {
                    Ok(inode) => inode,
                    Err(e) => {
                        warn!("ExtFS: read_inode({}) failed for {}: {:?}", new_ino, path, e);
                        return Err(e);
                    }
                };
                new_inode.i_mode = Self::S_IFREG | ((mode as u16) & 0o777);
                new_inode.i_size_lo = 0;
                new_inode.i_links_count = 1;
                new_inode.i_blocks_lo = 0;
                if let Err(e) = self.write_inode_by_number(new_ino, &new_inode) {
                    warn!("ExtFS: write_inode_by_number({}) failed for {}: {:?}", new_ino, path, e);
                    return Err(e);
                }

                if let Err(e) = self.add_dir_entry(
                    parent_ino,
                    &file_name,
                    new_ino,
                    Self::file_type_from_mode(new_inode.i_mode),
                ) {
                    warn!(
                        "ExtFS: add_dir_entry(parent={}, name={}, ino={}) failed for {}: {:?}",
                        parent_ino, file_name, new_ino, path, e
                    );
                    return Err(e);
                }

                warn!("ExtFS: created path={} ino={}", path, new_ino);

                new_ino
            }
            Err(e) => return Err(e),
        };
        let (mut inode, inode_offset) = self.read_inode_with_offset(ino)?;
        let inode_size = Self::inode_rec_len(&self.sb);

        if flags.contains(OpenFlags::O_TRUNC) {
            if !can_write {
                return Err(Error::PermissionDenied);
            }
            if (inode.i_mode & Self::S_IFMT) == Self::S_IFDIR {
                return Err(Error::InvalidType);
            }
            inode.i_size_lo = 0;
            self.write_inode_at_offset(&inode, inode_offset)?;
        }

        let append = flags.contains(OpenFlags::O_APPEND);
        let handle = ExtFileHandle {
            ops: self.ops.clone(),
            reader: self.reader.clone(),
            ino,
            inode,
            block_size: self.block_size,
            pos: if append { inode.i_size_lo as usize } else { 0 },
            ring_vaddr: self.ring_vaddr,
            ring_size: self.ring_size,
            uring: None,
            user_shm_base: 0,
            server_shm_base: 0,
            inode_offset,
            inode_size,
            sb: self.sb,
            group_desc_size: self.group_desc_size,
            can_read,
            can_write,
            append,
        };
        Ok(Box::new(handle))
    }

    pub fn mkdir(&mut self, _badge: Badge, path: &str, mode: u32) -> Result<(), Error> {
        if !self.writable {
            return Err(Error::NotSupported);
        }

        if self.resolve_path(path).is_ok() {
            return Err(Error::AlreadyExists);
        }

        let (parent_path, name) = Self::split_parent_name(path)?;
        let parent_ino = self.resolve_path(&parent_path)?;
        let (mut parent_inode, parent_inode_off) = self.read_inode_with_offset(parent_ino)?;
        if (parent_inode.i_mode & Self::S_IFMT) != Self::S_IFDIR {
            return Err(Error::InvalidType);
        }

        let new_ino = self.alloc_inode()?;
        let (mut new_inode, new_inode_off) = self.read_inode_with_offset(new_ino)?;
        new_inode.i_mode = Self::S_IFDIR | ((mode as u16) & 0o777);
        new_inode.i_links_count = 2;
        new_inode.i_size_lo = self.block_size;
        new_inode.i_blocks_lo = self.block_size / 512;

        let data_block = self.alloc_block()?;
        Self::set_inode_block_ptr(&mut new_inode, 0, data_block);

        let mut block = alloc::vec![0u8; self.block_size as usize];
        let dot = Self::dir_rec_len(1);
        block[0..4].copy_from_slice(&new_ino.to_le_bytes());
        block[4..6].copy_from_slice(&(dot as u16).to_le_bytes());
        block[6] = 1;
        block[7] = EXT4_FT_DIR;
        block[8] = b'.';

        let dd_off = dot;
        let dd_rec = self.block_size as usize - dot;
        block[dd_off..dd_off + 4].copy_from_slice(&parent_ino.to_le_bytes());
        block[dd_off + 4..dd_off + 6].copy_from_slice(&(dd_rec as u16).to_le_bytes());
        block[dd_off + 6] = 2;
        block[dd_off + 7] = EXT4_FT_DIR;
        block[dd_off + 8] = b'.';
        block[dd_off + 9] = b'.';

        self.reader.write_offset(data_block as usize * self.block_size as usize, &block)?;
        self.write_inode_at_offset(&new_inode, new_inode_off)?;

        self.add_dir_entry(parent_ino, &name, new_ino, EXT4_FT_DIR)?;
        parent_inode.i_links_count = parent_inode.i_links_count.saturating_add(1);
        self.write_inode_at_offset(&parent_inode, parent_inode_off)?;
        Ok(())
    }

    pub fn unlink(&mut self, _badge: Badge, path: &str) -> Result<(), Error> {
        if !self.writable {
            return Err(Error::NotSupported);
        }

        let (parent_path, name) = Self::split_parent_name(path)?;
        let parent_ino = self.resolve_path(&parent_path)?;
        let (mut parent_inode, parent_inode_off) = self.read_inode_with_offset(parent_ino)?;

        let target_ino = self.resolve_path(path)?;
        let (mut target_inode, target_inode_off) = self.read_inode_with_offset(target_ino)?;

        let is_dir = (target_inode.i_mode & Self::S_IFMT) == Self::S_IFDIR;
        if is_dir {
            if !self.is_dir_empty(target_ino)? {
                return Err(Error::ResourceBusy);
            }
        }

        let removed = self.remove_dir_entry(parent_ino, &name)?;
        if removed != target_ino {
            return Err(Error::IoError);
        }

        if is_dir {
            target_inode.i_size_lo = 0;
            target_inode.i_links_count = 0;
            parent_inode.i_links_count = parent_inode.i_links_count.saturating_sub(1);
            self.write_inode_at_offset(&parent_inode, parent_inode_off)?;
        } else {
            target_inode.i_links_count = target_inode.i_links_count.saturating_sub(1);
        }

        self.write_inode_at_offset(&target_inode, target_inode_off)?;
        Ok(())
    }

    pub fn link(&mut self, _badge: Badge, old_path: &str, new_path: &str) -> Result<(), Error> {
        if !self.writable {
            return Err(Error::NotSupported);
        }

        let old_ino = self.resolve_path(old_path)?;
        let (mut old_inode, old_inode_off) = self.read_inode_with_offset(old_ino)?;
        if (old_inode.i_mode & Self::S_IFMT) == Self::S_IFDIR {
            return Err(Error::NotSupported);
        }

        if self.resolve_path(new_path).is_ok() {
            return Err(Error::AlreadyExists);
        }

        let (new_parent, new_name) = Self::split_parent_name(new_path)?;
        let new_parent_ino = self.resolve_path(&new_parent)?;

        self.add_dir_entry(
            new_parent_ino,
            &new_name,
            old_ino,
            Self::file_type_from_mode(old_inode.i_mode),
        )?;
        old_inode.i_links_count = old_inode.i_links_count.saturating_add(1);
        self.write_inode_at_offset(&old_inode, old_inode_off)?;
        Ok(())
    }

    pub fn stat_path(&mut self, _badge: Badge, path: &str) -> Result<Stat, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        Ok(Stat {
            ino: ino as usize,
            size: inode.i_size_lo as usize,
            mode: inode.i_mode as u32,
            ..Default::default()
        })
    }

    pub fn lstat_path(&mut self, _badge: Badge, path: &str) -> Result<Stat, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        Ok(Stat {
            ino: ino as usize,
            size: inode.i_size_lo as usize,
            mode: inode.i_mode as u32,
            ..Default::default()
        })
    }

    pub fn readlink_path(&mut self, _badge: Badge, path: &str) -> Result<String, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        if !self.is_symlink_inode(&inode) {
            return Err(Error::InvalidType);
        }

        let target = self.read_inode_payload(&inode)?;
        let target = core::str::from_utf8(&target).map_err(|_| Error::InvalidType)?;
        Ok(String::from(target))
    }
}

pub struct ExtFileHandle {
    ops: Arc<dyn ExtOps>,
    reader: BlockReader,
    ino: u32,
    inode: Inode,
    block_size: u32,
    pos: usize,
    ring_vaddr: usize,
    ring_size: usize,
    uring: Option<glenda::io::uring::IoUringBuffer>,
    user_shm_base: usize,
    server_shm_base: usize,
    inode_offset: usize,
    inode_size: usize,
    sb: SuperBlock,
    group_desc_size: u16,
    can_read: bool,
    can_write: bool,
    append: bool,
}

impl FileHandleService for ExtFileHandle {
    fn close(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn stat(&self, _badge: Badge) -> Result<Stat, Error> {
        Ok(Stat {
            ino: self.ino as usize,
            size: self.inode.i_size_lo as usize,
            mode: self.inode.i_mode as u32,
            ..Default::default()
        })
    }

    fn read(&mut self, _badge: Badge, offset: usize, buf: &mut [u8]) -> Result<usize, Error> {
        if !self.can_read {
            return Err(Error::PermissionDenied);
        }

        let file_size = self.inode.i_size_lo as usize;
        if offset >= file_size || buf.is_empty() {
            return Ok(0);
        }

        let to_read = core::cmp::min(buf.len(), file_size - offset);
        let _start_block_idx = (offset / self.block_size as usize) as u32;
        // let end_block_idx = ((offset + buf.len() as usize + self.block_size as usize - 1)
        //     / self.block_size as usize) as u32;

        let mut read_len = 0;
        let mut current_offset = offset;
        let mut buf_ptr = 0;

        // Simple loop
        while buf_ptr < to_read {
            let lblock = (current_offset / self.block_size as usize) as u32;
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            let blk_offset_in_buf = (current_offset % self.block_size as usize) as usize;
            let chuck_len =
                core::cmp::min(to_read - buf_ptr, self.block_size as usize - blk_offset_in_buf);

            if chuck_len == self.block_size as usize {
                if pblock != 0 {
                    let read_offset = pblock as usize * self.block_size as usize;
                    self.reader.read_offset(read_offset, &mut buf[buf_ptr..buf_ptr + chuck_len])?;
                } else {
                    buf[buf_ptr..buf_ptr + chuck_len].fill(0);
                }
            } else {
                let mut block_data = alloc::vec![0u8; self.block_size as usize];
                if pblock != 0 {
                    let read_offset = pblock as usize * self.block_size as usize;
                    self.reader.read_offset(read_offset, &mut block_data)?;
                } else {
                    // Sparse block, zeroed
                }

                buf[buf_ptr..buf_ptr + chuck_len]
                    .copy_from_slice(&block_data[blk_offset_in_buf..blk_offset_in_buf + chuck_len]);
            }

            read_len += chuck_len;
            current_offset += chuck_len as usize;
            buf_ptr += chuck_len;
        }
        Ok(read_len)
    }

    fn write(&mut self, _badge: Badge, offset: usize, buf: &[u8]) -> Result<usize, Error> {
        if !self.can_write {
            return Err(Error::PermissionDenied);
        }
        if !self.is_regular() {
            return Err(Error::InvalidType);
        }

        if buf.is_empty() {
            return Ok(0);
        }

        let mut current_offset = if self.append { self.inode.i_size_lo as usize } else { offset };
        if current_offset > self.inode.i_size_lo as usize {
            return Err(Error::NotSupported);
        }

        let mut written = 0;
        let mut buf_ptr = 0;
        let mut sb = self.sb;
        let reader = self.reader.clone();
        let block_size = self.block_size;
        let group_desc_size = self.group_desc_size;

        while buf_ptr < buf.len() {
            let lblock = (current_offset / self.block_size as usize) as u32;
            let mut alloc =
                || ExtFs::alloc_block_raw(&reader, &mut sb, block_size, group_desc_size);
            let pblock = match self.ops.map_block(
                &self.reader,
                &mut self.inode,
                lblock,
                self.block_size,
                true,
                &mut alloc,
            ) {
                Ok(pb) => pb,
                Err(e) => {
                    warn!(
                        "ExtFS: write map_block failed ino={} lblock={} off={} err={:?}",
                        self.ino, lblock, current_offset, e
                    );
                    return Err(Error::IoError);
                }
            };

            let blk_offset_in_buf = (current_offset % self.block_size as usize) as usize;
            let chuck_len =
                core::cmp::min(buf.len() - buf_ptr, self.block_size as usize - blk_offset_in_buf);

            let read_offset = pblock as usize * self.block_size as usize;
            let device_block_addr = pblock as usize * (self.block_size / 512) as usize;

            if chuck_len == self.block_size as usize {
                if let Err(e) =
                    self.reader.write_blocks(device_block_addr, &buf[buf_ptr..buf_ptr + chuck_len])
                {
                    warn!(
                        "ExtFS: write write_blocks(full) failed ino={} lblock={} pblock={} dev_sector={} len={} err={:?}",
                        self.ino,
                        lblock,
                        pblock,
                        device_block_addr,
                        chuck_len,
                        e
                    );
                    return Err(e);
                }
            } else {
                // Read
                let mut block_data = alloc::vec![0u8; self.block_size as usize];
                if let Err(e) = self.reader.read_offset(read_offset, &mut block_data) {
                    warn!(
                        "ExtFS: write read_offset(partial) failed ino={} lblock={} pblock={} read_off={} err={:?}",
                        self.ino,
                        lblock,
                        pblock,
                        read_offset,
                        e
                    );
                    return Err(e);
                }

                // Modify
                block_data[blk_offset_in_buf..blk_offset_in_buf + chuck_len]
                    .copy_from_slice(&buf[buf_ptr..buf_ptr + chuck_len]);

                // Write
                if let Err(e) = self.reader.write_blocks(device_block_addr, &block_data) {
                    warn!(
                        "ExtFS: write write_blocks(partial) failed ino={} lblock={} pblock={} dev_sector={} err={:?}",
                        self.ino,
                        lblock,
                        pblock,
                        device_block_addr,
                        e
                    );
                    return Err(e);
                }
            }

            written += chuck_len;
            current_offset += chuck_len as usize;
            buf_ptr += chuck_len;
        }

        if current_offset > u32::MAX as usize {
            return Err(Error::MessageTooLong);
        }
        self.sb = sb;
        if current_offset > self.inode.i_size_lo as usize {
            self.inode.i_size_lo = current_offset as u32;
            self.flush_inode_metadata()?;
        } else if written != 0 {
            self.flush_inode_metadata()?;
        }

        self.pos = current_offset;
        Ok(written)
    }

    fn getdents(&mut self, _badge: Badge, _count: usize) -> Result<Vec<DEntry>, Error> {
        let count = _count;
        if count == 0 {
            return Ok(Vec::new());
        }

        if !self.is_dir() {
            return Err(Error::InvalidType);
        }

        let dir_size = self.inode.i_size_lo as usize;
        if self.pos >= dir_size {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        let mut packed_len = 0usize;
        let mut cursor = self.pos;
        let block_size = self.block_size as usize;
        let max_entries_by_ipc = (glenda::ipc::IPC_BUFFER_SIZE
            .saturating_sub(core::mem::size_of::<usize>()))
            / core::mem::size_of::<DEntry>();
        if max_entries_by_ipc == 0 {
            return Err(Error::MessageTooLong);
        }

        while cursor < dir_size {
            let lblock = (cursor / block_size) as u32;
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            if pblock == 0 {
                cursor = ((cursor / block_size) + 1) * block_size;
                continue;
            }

            let mut block_buf = alloc::vec![0u8; block_size];
            let read_offset = pblock as usize * block_size;
            self.reader.read_offset(read_offset, &mut block_buf)?;

            let block_off = cursor % block_size;
            if block_off + core::mem::size_of::<DirEntry2>() > block_size {
                cursor = ((cursor / block_size) + 1) * block_size;
                continue;
            }

            let ptr = unsafe { block_buf.as_ptr().add(block_off) };
            let de = unsafe { core::ptr::read_unaligned(ptr as *const DirEntry2) };

            let rec_len = de.rec_len as usize;
            if rec_len < 8 || block_off + rec_len > block_size {
                return Err(Error::IoError);
            }

            let next_off = cursor.saturating_add(rec_len);

            if de.inode != 0 {
                if packed_len.saturating_add(rec_len) > count {
                    break;
                }
                if entries.len() >= max_entries_by_ipc {
                    break;
                }

                let name_len = core::cmp::min(
                    de.name_len as usize,
                    core::cmp::min(rec_len.saturating_sub(8), 255),
                );

                let mut name = [0u8; 256];
                let name_ptr = unsafe { ptr.add(8) };
                let name_slice = unsafe { core::slice::from_raw_parts(name_ptr, name_len) };
                name[..name_len].copy_from_slice(name_slice);

                entries.push(DEntry {
                    d_ino: de.inode as usize,
                    d_off: next_off as i64,
                    d_reclen: de.rec_len,
                    d_type: Self::ext4_file_type_to_dirent(de.file_type),
                    d_name: name,
                });

                packed_len = packed_len.saturating_add(rec_len);
            }

            cursor = next_off;
        }

        self.pos = core::cmp::min(cursor, dir_size);
        Ok(entries)
    }

    fn seek(&mut self, _badge: Badge, _offset: i64, _whence: usize) -> Result<usize, Error> {
        let base = match _whence {
            seek::SEEK_SET => 0i128,
            seek::SEEK_CUR => self.pos as i128,
            seek::SEEK_END => self.inode.i_size_lo as i128,
            _ => return Err(Error::InvalidArgs),
        };

        let new_pos = base + _offset as i128;
        if new_pos < 0 || new_pos > usize::MAX as i128 {
            return Err(Error::InvalidArgs);
        }

        self.pos = new_pos as usize;
        Ok(self.pos)
    }

    fn sync(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn truncate(&mut self, _badge: Badge, _size: usize) -> Result<(), Error> {
        if !self.can_write {
            return Err(Error::PermissionDenied);
        }
        if !self.is_regular() {
            return Err(Error::InvalidType);
        }

        if _size > u32::MAX as usize {
            return Err(Error::MessageTooLong);
        }

        let old_size = self.inode.i_size_lo as usize;
        if _size > old_size {
            let start_lblock = old_size.div_ceil(self.block_size as usize) as u32;
            let end_lblock = (_size - 1) / self.block_size as usize;
            let mut sb = self.sb;
            let reader = self.reader.clone();
            let block_size = self.block_size;
            let group_desc_size = self.group_desc_size;
            for lblock in start_lblock..=end_lblock as u32 {
                let mut alloc =
                    || ExtFs::alloc_block_raw(&reader, &mut sb, block_size, group_desc_size);
                self.ops
                    .map_block(
                        &self.reader,
                        &mut self.inode,
                        lblock,
                        self.block_size,
                        true,
                        &mut alloc,
                    )
                    .map_err(|_| Error::IoError)?;
            }
            self.sb = sb;
        }

        self.inode.i_size_lo = _size as u32;
        if self.pos > _size {
            self.pos = _size;
        }
        self.flush_inode_metadata()?;
        Ok(())
    }
}

impl ExtFileHandle {
    const S_IFMT: u16 = 0xF000;
    const S_IFDIR: u16 = 0x4000;

    const DT_UNKNOWN: u8 = 0;
    const DT_FIFO: u8 = 1;
    const DT_CHR: u8 = 2;
    const DT_DIR: u8 = 4;
    const DT_BLK: u8 = 6;
    const DT_REG: u8 = 8;
    const DT_LNK: u8 = 10;
    const DT_SOCK: u8 = 12;

    #[inline]
    fn is_dir(&self) -> bool {
        (self.inode.i_mode & Self::S_IFMT) == Self::S_IFDIR
    }

    #[inline]
    fn is_regular(&self) -> bool {
        (self.inode.i_mode & Self::S_IFMT) == 0x8000
    }

    fn write_inode_to_disk(
        reader: &BlockReader,
        inode_offset: usize,
        inode_size: usize,
        inode: &Inode,
    ) -> Result<(), Error> {
        let mut disk_inode = alloc::vec![0u8; inode_size];
        reader.read_offset(inode_offset, &mut disk_inode)?;

        let inode_raw = unsafe {
            core::slice::from_raw_parts(
                inode as *const Inode as *const u8,
                core::mem::size_of::<Inode>(),
            )
        };
        let copy_len = core::cmp::min(inode_raw.len(), disk_inode.len());
        disk_inode[..copy_len].copy_from_slice(&inode_raw[..copy_len]);

        reader.write_offset(inode_offset, &disk_inode)?;
        Ok(())
    }

    fn flush_inode_metadata(&self) -> Result<(), Error> {
        Self::write_inode_to_disk(&self.reader, self.inode_offset, self.inode_size, &self.inode)
    }

    #[inline]
    fn ext4_file_type_to_dirent(file_type: u8) -> u8 {
        match file_type {
            EXT4_FT_UNKNOWN => Self::DT_UNKNOWN,
            EXT4_FT_REG_FILE => Self::DT_REG,
            EXT4_FT_DIR => Self::DT_DIR,
            3 => Self::DT_CHR,
            4 => Self::DT_BLK,
            5 => Self::DT_FIFO,
            6 => Self::DT_SOCK,
            7 => Self::DT_LNK,
            _ => Self::DT_UNKNOWN,
        }
    }

    fn read_shm_internal(&self, offset: usize, len: u32, shm_vaddr: usize) -> Result<usize, Error> {
        let mut read_len = 0;
        let mut current_offset = offset;
        let mut current_shm_vaddr = shm_vaddr;
        let mut remaining = len as usize;

        while remaining > 0 {
            let lblock = (current_offset / self.block_size as usize) as u32;
            let pblock = self
                .ops
                .get_block_addr(&self.reader, &self.inode, lblock, self.block_size)
                .map_err(|_| Error::IoError)?;

            let blk_offset_in_block = (current_offset % self.block_size as usize) as usize;
            let chunk_len =
                core::cmp::min(remaining, self.block_size as usize - blk_offset_in_block);

            if pblock != 0 {
                let read_offset =
                    pblock as usize * self.block_size as usize + blk_offset_in_block as usize;
                self.reader.read_shm(read_offset, chunk_len as u32, current_shm_vaddr)?;
            } else {
                unsafe { core::ptr::write_bytes(current_shm_vaddr as *mut u8, 0, chunk_len) };
            }

            read_len += chunk_len;
            current_offset += chunk_len as usize;
            current_shm_vaddr += chunk_len;
            remaining -= chunk_len;

            if current_offset >= self.inode.i_size_lo as usize {
                break;
            }
        }
        Ok(read_len)
    }
}
