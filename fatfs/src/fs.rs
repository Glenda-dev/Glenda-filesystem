use crate::block::BlockReader;
use crate::defs::*;
use crate::ops::{FatOps, RootLocation};
use crate::versions::Fat16Ops;
use crate::versions::Fat32Ops;
use crate::versions::{ExFatBpb, ExFatOps};
use alloc::boxed::Box;
use alloc::vec::Vec;
use glenda::cap::Endpoint;
use glenda::error::Error;
use glenda::interface::fs::{FileHandleService, FileSystemJournalService, FileSystemService};
use glenda::protocol::fs::{DEntry, OpenFlags, Stat};

pub struct FatFs {
    reader: BlockReader,
    ops: Box<dyn FatOps>,
}

impl FatFs {
    pub fn new(block_device: Endpoint) -> Self {
        let reader = BlockReader::new(block_device);

        // Read BPB
        let mut buf = [0u8; 512];
        let _ = reader.read_offset(0, &mut buf);

        let oem_name = &buf[3..11];
        let ops: Box<dyn FatOps> = if oem_name == b"EXFAT   " {
            let bpb = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ExFatBpb) };
            let bytes_per_sector = 1u32 << bpb.bytes_per_sector_shift;
            let sectors_per_cluster = 1u32 << bpb.sectors_per_cluster_shift;

            Box::new(ExFatOps {
                bytes_per_sector,
                sectors_per_cluster,
                fat_start_sector: bpb.partition_offset + bpb.fat_offset as u64,
                data_start_sector: bpb.partition_offset + bpb.cluster_heap_offset as u64,
                root_cluster: bpb.root_dir_cluster,
            })
        } else {
            if buf[510] != 0x55 || buf[511] != 0xAA {
                // Warning: Invalid Signature
            }

            let bpb =
                unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const BiosParameterBlock) };

            let bytes_per_sec = if bpb.byts_per_sec == 0 { 512 } else { bpb.byts_per_sec };
            let root_ent_cnt = bpb.root_ent_cnt;
            let fat_sz = if bpb.fat_sz_16 != 0 { bpb.fat_sz_16 as u32 } else { bpb.fat_sz_32 };
            let tot_sec = if bpb.tot_sec_16 != 0 { bpb.tot_sec_16 as u32 } else { bpb.tot_sec_32 };

            let root_dir_sectors =
                ((root_ent_cnt as u32 * 32) + (bytes_per_sec as u32 - 1)) / bytes_per_sec as u32;

            let data_sec = tot_sec
                - (bpb.rsvd_sec_cnt as u32 + (bpb.num_fats as u32 * fat_sz) + root_dir_sectors);
            let count_of_clusters = data_sec / bpb.sec_per_clus as u32;

            if count_of_clusters < 65525 {
                Box::new(Fat16Ops {
                    bytes_per_sector: bytes_per_sec,
                    sectors_per_cluster: bpb.sec_per_clus,
                    fat_start_sector: bpb.rsvd_sec_cnt as u64,
                    root_start_sector: (bpb.rsvd_sec_cnt as u32 + (bpb.num_fats as u32 * fat_sz))
                        as u64,
                    root_entries: bpb.root_ent_cnt,
                    data_start_sector: (bpb.rsvd_sec_cnt as u32
                        + (bpb.num_fats as u32 * fat_sz)
                        + root_dir_sectors) as u64,
                })
            } else {
                Box::new(Fat32Ops {
                    bytes_per_sector: bytes_per_sec,
                    sectors_per_cluster: bpb.sec_per_clus,
                    fat_start_sector: bpb.rsvd_sec_cnt as u64,
                    data_start_sector: (bpb.rsvd_sec_cnt as u32 + (bpb.num_fats as u32 * fat_sz))
                        as u64,
                    root_cluster: bpb.root_clus,
                })
            }
        };

        Self { reader, ops }
    }

    pub fn get_next_cluster(&self, cluster: u32) -> Result<u32, Error> {
        self.ops.get_next_cluster(&self.reader, cluster)
    }

    pub fn get_cluster_chain(&self, start_cluster: u32) -> Result<Vec<u32>, Error> {
        let mut chain = Vec::new();
        let mut curr = start_cluster;
        loop {
            if curr < 2 {
                break;
            }
            chain.push(curr);
            let next = self.get_next_cluster(curr)?;
            if next >= 0x0FFFFFF8 {
                break;
            }
            if next == 0x0FFFFFF7 {
                return Err(Error::IoError);
            }
            curr = next;
        }
        Ok(chain)
    }

    pub fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), Error> {
        let sector = self.ops.cluster_to_sector(cluster);
        let size = (self.ops.sectors_per_cluster() as u64) * (self.ops.bytes_per_sector() as u64);
        if buf.len() < size as usize {
            return Err(Error::MessageTooLong);
        }
        let offset = sector * (self.ops.bytes_per_sector() as u64);
        self.reader
            .read_offset(offset, &mut buf[..size as usize])
            .map_err(|_| Error::IoError)
            .map(|_| ())
    }

    fn read_sectors(
        &self,
        start_sector: u64,
        num_sectors: u32,
        buf: &mut [u8],
    ) -> Result<(), Error> {
        let bps = self.ops.bytes_per_sector() as u64;
        let size = num_sectors as u64 * bps;
        if buf.len() < size as usize {
            return Err(Error::MessageTooLong);
        }
        let offset = start_sector * bps;
        self.reader
            .read_offset(offset, &mut buf[..size as usize])
            .map_err(|_| Error::IoError)
            .map(|_| ())
    }

    fn matches(fat_name: &[u8; 11], name: &str) -> bool {
        let mut normalized = [0x20u8; 11];
        let mut name_iter = name.bytes();
        let mut i = 0;
        loop {
            match name_iter.next() {
                Some(b'.') => break,
                Some(b) => {
                    if i < 8 {
                        normalized[i] = b.to_ascii_uppercase();
                        i += 1;
                    } else {
                        return false;
                    }
                }
                None => break,
            }
        }

        let mut i = 8;
        while let Some(b) = name_iter.next() {
            if i < 11 {
                normalized[i] = b.to_ascii_uppercase();
                i += 1;
            } else {
                return false;
            }
        }

        &normalized == fat_name
    }

    fn scan_dir_entries(&self, data: &[u8], name: &str) -> Result<DirEntry, Error> {
        for chunk in data.chunks(32) {
            if chunk.len() < 32 {
                break;
            }
            if chunk[0] == 0 {
                return Err(Error::NotFound);
            }
            if chunk[0] == 0xE5 {
                continue;
            }

            let entry = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const DirEntry) };
            if (entry.attr & ATTR_LONG_NAME) == ATTR_LONG_NAME {
                continue;
            }
            if (entry.attr & ATTR_VOLUME_ID) != 0 {
                continue;
            }

            if Self::matches(&entry.name, name) {
                return Ok(entry);
            }
        }
        Err(Error::NotFound)
    }

    pub fn find_entry(&self, location: RootLocation, name: &str) -> Result<DirEntry, Error> {
        match location {
            RootLocation::Cluster(cluster) => {
                let chain = self.get_cluster_chain(cluster)?;
                let cluster_size = (self.ops.sectors_per_cluster() as usize)
                    * (self.ops.bytes_per_sector() as usize);
                let mut buf = alloc::vec![0u8; cluster_size];

                for c in chain {
                    self.read_cluster(c, &mut buf)?;
                    match self.scan_dir_entries(&buf, name) {
                        Ok(entry) => return Ok(entry),
                        Err(Error::NotFound) => continue, // Check next cluster
                        Err(e) => return Err(e),
                    }
                }
                Err(Error::NotFound)
            }
            RootLocation::Sector(start, count) => {
                let bytes_len = (count as u64 * self.ops.bytes_per_sector() as u64) as usize;
                let mut buf = alloc::vec![0u8; bytes_len];
                self.read_sectors(start, count, &mut buf)?;
                self.scan_dir_entries(&buf, name)
            }
        }
    }

    pub fn lookup(&self, path: &str) -> Result<DirEntry, Error> {
        let root_loc = self.ops.get_root_location();

        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if path_parts.is_empty() {
            return Ok(DirEntry {
                name: [0x20; 11],
                attr: ATTR_DIRECTORY,
                nt_res: 0,
                crt_time_tenth: 0,
                crt_time: 0,
                crt_date: 0,
                lst_acc_date: 0,
                fst_clus_hi: 0,
                wrt_time: 0,
                wrt_date: 0,
                fst_clus_lo: 0,
                file_size: 0,
            });
        }

        let mut current_loc = root_loc;
        // Mock entry for initial state is tricky if we don't have it, but we only need it for return if path is empty.
        // If loop runs, current_entry is updated.
        let mut current_entry = DirEntry {
            name: [0x20; 11],
            attr: ATTR_DIRECTORY,
            nt_res: 0,
            crt_time_tenth: 0,
            crt_time: 0,
            crt_date: 0,
            lst_acc_date: 0,
            fst_clus_hi: 0,
            wrt_time: 0,
            wrt_date: 0,
            fst_clus_lo: 0,
            file_size: 0,
        };

        for (i, part) in path_parts.iter().enumerate() {
            let entry = self.find_entry(current_loc, part)?;

            if i < path_parts.len() - 1 {
                if (entry.attr & ATTR_DIRECTORY) == 0 {
                    return Err(Error::NotSupported); // Not a dir
                }
                let cluster_hi = entry.fst_clus_hi as u32;
                let cluster_lo = entry.fst_clus_lo as u32;
                let cluster = (cluster_hi << 16) | cluster_lo;
                current_loc = RootLocation::Cluster(cluster);
            }
            current_entry = entry;
        }

        Ok(current_entry)
    }
}

impl FileSystemService for FatFs {
    fn open(&mut self, path: &str, _flags: OpenFlags, _mode: u32) -> Result<usize, Error> {
        self.lookup(path)?;
        Ok(100)
    }

    fn mkdir(&mut self, _path: &str, _mode: u32) -> Result<(), Error> {
        Ok(())
    }

    fn unlink(&mut self, _path: &str) -> Result<(), Error> {
        Ok(())
    }

    fn rename(&mut self, _old_path: &str, _new_path: &str) -> Result<(), Error> {
        Ok(())
    }

    fn stat_path(&mut self, path: &str) -> Result<Stat, Error> {
        let entry = self.lookup(path)?;
        let mut stat = Stat::default();
        stat.size = entry.file_size as u64;
        // Simple mode mapping
        stat.mode = if (entry.attr & ATTR_DIRECTORY) != 0 { 0o040755 } else { 0o100644 };
        Ok(stat)
    }
}

// Journal Stub
impl FileSystemJournalService for FatFs {
    fn transaction_start(&mut self) -> Result<u64, Error> {
        Ok(1)
    }
    fn transaction_commit(&mut self, _tid: u64) -> Result<(), Error> {
        Ok(())
    }
    fn transaction_abort(&mut self, _tid: u64) -> Result<(), Error> {
        Ok(())
    }
    fn log_block(&mut self, _tid: u64, _block_num: u64, _data: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

pub struct FatFileHandle {}

impl FileHandleService for FatFileHandle {
    fn read(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<usize, Error> {
        Ok(0)
    }
    fn write(&mut self, _offset: u64, _buf: &[u8]) -> Result<usize, Error> {
        Ok(0)
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
