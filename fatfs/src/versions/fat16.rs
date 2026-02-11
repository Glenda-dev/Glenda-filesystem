use crate::block::BlockReader;
use crate::ops::{FatOps, RootLocation};
use glenda::error::Error;

pub struct Fat16Ops {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub fat_start_sector: u64,
    pub root_start_sector: u64,
    pub root_entries: u16,
    pub data_start_sector: u64,
}

impl FatOps for Fat16Ops {
    fn get_next_cluster(&self, reader: &BlockReader, cluster: u32) -> Result<u32, Error> {
        let fat_offset = cluster as u64 * 2;
        let fat_sector_offset = fat_offset / self.bytes_per_sector as u64;
        let entry_offset = (fat_offset % self.bytes_per_sector as u64) as usize;

        let sector = self.fat_start_sector + fat_sector_offset;

        let mut buf = alloc::vec![0u8; self.bytes_per_sector as usize];
        let read_pos = sector * self.bytes_per_sector as u64;
        reader.read_offset(read_pos, &mut buf).map_err(|_| Error::IoError)?;

        // Read u16
        let val = unsafe {
            let ptr = buf.as_ptr().add(entry_offset);
            core::ptr::read_unaligned(ptr as *const u16)
        };

        // FAT16 end of chain is >= 0xFFF8
        if val >= 0xFFF8 {
            Ok(0x0FFFFFFF) // Normalize to FAT32 EOF convention for internal logic
        } else {
            Ok(val as u32)
        }
    }

    fn cluster_to_sector(&self, cluster: u32) -> u64 {
        let rel_cluster = if cluster >= 2 { cluster - 2 } else { 0 };
        self.data_start_sector + (rel_cluster as u64 * self.sectors_per_cluster as u64)
    }

    fn get_root_location(&self) -> RootLocation {
        // Size of root dir in sectors
        let root_dir_size = (self.root_entries as u64 * 32 + self.bytes_per_sector as u64 - 1)
            / self.bytes_per_sector as u64;
        RootLocation::Sector(self.root_start_sector, root_dir_size as u32)
    }

    fn bytes_per_sector(&self) -> u32 {
        self.bytes_per_sector as u32
    }
    fn sectors_per_cluster(&self) -> u32 {
        self.sectors_per_cluster as u32
    }
}
