use crate::block::BlockReader;
use crate::ops::{FatOps, RootLocation};
use glenda::error::Error;

#[repr(C, packed)]
pub struct ExFatBpb {
    pub jmp_boot: [u8; 3],
    pub oem_name: [u8; 8],
    pub padding: [u8; 53],
    pub partition_offset: u64,
    pub vol_length: u64,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub root_dir_cluster: u32,
    pub vol_serial: u32,
    pub fs_revision: u16,
    pub vol_flags: u16,
    pub bytes_per_sector_shift: u8,
    pub sectors_per_cluster_shift: u8,
    pub num_fats: u8,
    pub drive_select: u8,
    pub percent_in_use: u8,
    // ...
}

pub struct ExFatOps {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub fat_start_sector: u64,
    pub data_start_sector: u64,
    pub root_cluster: u32,
}

impl FatOps for ExFatOps {
    fn get_next_cluster(&self, reader: &BlockReader, cluster: u32) -> Result<u32, Error> {
        // exFAT FAT entries are 32-bit
        let fat_offset = cluster as u64 * 4;
        let fat_sector_offset = fat_offset / self.bytes_per_sector as u64;
        let entry_offset = (fat_offset % self.bytes_per_sector as u64) as usize;

        let sector = self.fat_start_sector + fat_sector_offset;

        // TODO: Handle buffer size dynamically if sector > 512
        let mut buf = alloc::vec![0u8; self.bytes_per_sector as usize];
        let read_pos = sector * self.bytes_per_sector as u64;
        reader.read_offset(read_pos, &mut buf).map_err(|_| Error::IoError)?;

        let ptr = unsafe { buf.as_ptr().add(entry_offset) };
        let val = unsafe { core::ptr::read_unaligned(ptr as *const u32) };

        Ok(val) // All 32 bits are valid
    }

    fn cluster_to_sector(&self, cluster: u32) -> u64 {
        // exFAT 1st cluster is cluster 2 usually
        let rel_cluster = if cluster >= 2 { cluster - 2 } else { 0 };
        self.data_start_sector + (rel_cluster as u64 * self.sectors_per_cluster as u64)
    }

    fn get_root_location(&self) -> RootLocation {
        RootLocation::Cluster(self.root_cluster)
    }

    fn bytes_per_sector(&self) -> u32 {
        self.bytes_per_sector
    }
    fn sectors_per_cluster(&self) -> u32 {
        self.sectors_per_cluster
    }
}
