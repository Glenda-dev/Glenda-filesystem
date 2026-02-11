use crate::block::BlockReader;
use crate::defs::ext4::*;
use crate::log;
use crate::ops::ExtOps;
use crate::versions::ext2::Ext2Ops;
use crate::versions::ext3::Ext3Ops;
use crate::versions::ext4::Ext4Ops;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::slice;
use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::interface::fs::{FileHandleService, FileSystemJournalService, FileSystemService};
use glenda::protocol::fs::{DEntry, OpenFlags, Stat};

pub struct ExtFs {
    reader: BlockReader,
    sb: SuperBlock,
    block_size: u32,
    group_desc_size: u16,
    inodes_per_group: u32,
    ops: Box<dyn ExtOps>,
}

impl ExtFs {
    pub fn new(block_device: Endpoint) -> Self {
        let reader = BlockReader::new(block_device);

        // ... (existing helper logic in new)
        let mut sb_buf = [0u8; 1024];
        if let Err(_) = reader.read_offset(SUPER_BLOCK_OFFSET, &mut sb_buf) {
            panic!("Failed to read superblock");
        }

        let sb = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const SuperBlock) };
        let magic = sb.s_magic;

        if magic != EXT4_SUPER_MAGIC {
            panic!("Invalid Ext4 Magic: {:x}", magic);
        }

        let block_size = 1024 << sb.s_log_block_size;
        let group_desc_size = if (sb.s_feature_incompat & 0x80) != 0 { sb.s_desc_size } else { 32 };

        // Determine OPS based on features
        let ops: Box<dyn ExtOps> = if (sb.s_feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0 {
            log!("Detected Ext4 with Extents");
            Box::new(Ext4Ops)
        } else if (sb.s_feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0 {
            log!("Detected Ext3 (Journaled)");
            Box::new(Ext3Ops)
        } else {
            log!("Detected Ext2");
            Box::new(Ext2Ops)
        };

        Self {
            reader,
            sb,
            block_size,
            group_desc_size,
            inodes_per_group: sb.s_inodes_per_group,
            ops,
        }
    }

    fn read_group_desc(&self, group: u32) -> Result<GroupDesc, Error> {
        let first_bg_block = self.sb.s_first_data_block + 1;
        let offset = (first_bg_block as u64 * self.block_size as u64)
            + (group as u64 * self.group_desc_size as u64);

        let mut buf = [0u8; 64];
        self.reader.read_offset(offset, &mut buf)?;

        // Handling packed struct read safely
        let gd = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const GroupDesc) };
        Ok(gd)
    }

    fn read_inode(&self, ino: u32) -> Result<Inode, Error> {
        if ino < 1 {
            return Err(Error::NotFound);
        }
        let group = (ino - 1) / self.inodes_per_group;
        let index = (ino - 1) % self.inodes_per_group;

        let gd = self.read_group_desc(group)?;

        let table_block = gd.bg_inode_table_lo;

        let inode_size = self.sb.s_inode_size as u64;
        let offset = (table_block as u64 * self.block_size as u64) + (index as u64 * inode_size);

        let mut buf = [0u8; 256];
        self.reader.read_offset(offset, &mut buf)?;

        let inode = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Inode) };
        Ok(inode)
    }

    fn get_block_addr(&self, inode: &Inode, lblock: u32) -> Result<u32, Error> {
        self.ops.get_block_addr(&self.reader, inode, lblock, self.block_size)
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
            let read_offset = pblock as u64 * self.block_size as u64;
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
}

impl FileSystemJournalService for ExtFs {
    fn transaction_start(&mut self) -> Result<u64, Error> {
        Ok(1)
    }

    fn transaction_commit(&mut self, _tid: u64) -> Result<(), Error> {
        Ok(())
    }

    fn transaction_abort(&mut self, _tid: u64) -> Result<(), Error> {
        Ok(())
    }

    fn log_block(&mut self, _tid: u64, block_num: u64, data: &[u8]) -> Result<(), Error> {
        let sector = block_num * (self.block_size as u64 / 512);
        self.reader.write_blocks(sector, data)?;
        Ok(())
    }
}

impl FileSystemService for ExtFs {
    fn open(&mut self, path: &str, flags: OpenFlags, _mode: u32) -> Result<usize, Error> {
        if flags.contains(OpenFlags::O_CREAT) {
            // Mock Create:
            // 1. transaction_start
            // 2. allocate inode, link to dir...
            // 3. transaction_commit
            let tid = self.transaction_start()?;
            // logic...
            self.transaction_commit(tid)?;
            return Ok(100); // Mock new inode
        }
        let ino = self.resolve_path(path)?;
        Ok(ino as usize)
    }

    fn mkdir(&mut self, _path: &str, _mode: u32) -> Result<(), Error> {
        let tid = self.transaction_start()?;
        // logic...
        self.transaction_commit(tid)?;
        Ok(())
    }

    fn unlink(&mut self, _path: &str) -> Result<(), Error> {
        let tid = self.transaction_start()?;
        // logic...
        self.transaction_commit(tid)?;
        Ok(())
    }

    fn rename(&mut self, _old_path: &str, _new_path: &str) -> Result<(), Error> {
        let tid = self.transaction_start()?;
        // logic...
        self.transaction_commit(tid)?;
        Ok(())
    }

    fn stat_path(&mut self, path: &str) -> Result<Stat, Error> {
        let ino = self.resolve_path(path)?;
        let inode = self.read_inode(ino)?;
        Ok(Stat {
            ino: ino as u64,
            size: inode.i_size_lo as u64,
            mode: inode.i_mode as u32,
            ..Default::default()
        })
    }
}

// Helper for writing
impl ExtFs {
    pub fn write_file(&mut self, ino: u32, offset: u64, buf: &[u8]) -> Result<usize, Error> {
        let tid = self.transaction_start()?;

        let inode = self.read_inode(ino)?;
        let start_block_idx = (offset / self.block_size as u64) as u32;
        let end_block_idx = ((offset + buf.len() as u64 + self.block_size as u64 - 1)
            / self.block_size as u64) as u32;

        let mut written = 0;
        let mut current_offset = offset;
        let mut buf_ptr = 0;

        for blk_idx in start_block_idx..end_block_idx {
            let pblock = self.get_block_addr(&inode, blk_idx).map_err(|_| Error::IoError)?;

            // Simple Read-Modify-Write
            let mut block_data = alloc::vec![0u8; self.block_size as usize];
            let read_offset = pblock as u64 * self.block_size as u64;
            self.reader.read_offset(read_offset, &mut block_data)?;

            let blk_offset_in_buf = (current_offset % self.block_size as u64) as usize;
            let chuck_len =
                core::cmp::min(buf.len() - buf_ptr, self.block_size as usize - blk_offset_in_buf);

            block_data[blk_offset_in_buf..blk_offset_in_buf + chuck_len]
                .copy_from_slice(&buf[buf_ptr..buf_ptr + chuck_len]);

            self.log_block(tid, pblock as u64, &block_data)?;

            written += chuck_len;
            current_offset += chuck_len as u64;
            buf_ptr += chuck_len;
        }

        self.transaction_commit(tid)?;
        Ok(written)
    }
}

pub struct Ext4FileHandle {}

impl FileHandleService for Ext4FileHandle {
    fn read(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<usize, Error> {
        Err(Error::NotImplemented)
    }
    fn write(&mut self, _offset: u64, _buf: &[u8]) -> Result<usize, Error> {
        Err(Error::NotImplemented)
    }
    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn stat(&self) -> Result<Stat, Error> {
        Ok(Stat::default())
    }
    fn getdents(&mut self, _count: usize) -> Result<Vec<DEntry>, Error> {
        Ok(Vec::new())
    }
    fn seek(&mut self, _offset: i64, _whence: usize) -> Result<u64, Error> {
        Ok(0)
    }
    fn sync(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn truncate(&mut self, _size: u64) -> Result<(), Error> {
        Ok(())
    }
}
