use crate::block::BlockReader;
use crate::defs::*;
use crate::ops::{FatOps, RootLocation};
use crate::versions::Fat16Ops;
use crate::versions::Fat32Ops;
use crate::versions::{ExFatBpb, ExFatOps};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use glenda::cap::Endpoint;
use glenda::client::ResourceClient;
use glenda::error::Error;
use glenda::interface::fs::FileHandleService;
use glenda::interface::{MemoryService, ResourceService};
use glenda::ipc::Badge;
use glenda::mem::shm::SharedMemory;
use glenda::protocol::fs::{DEntry, OpenFlags, Stat};

pub struct FatFs {
    reader: BlockReader,
    ops: Arc<dyn FatOps>,
    ring_vaddr: usize,
    ring_size: usize,
}

impl FatFs {
    pub fn new(
        block_device: Endpoint,
        ring_vaddr: usize,
        ring_size: usize,
        res_client: &mut ResourceClient,
    ) -> Result<Self, Error> {
        let mut reader = BlockReader::new(block_device);
        reader.init()?;

        // Setup IoUring (similar to ExtFS)
        let sq_entries = 4;
        let cq_entries = 4;
        let notify_slot = glenda::cap::CapPtr::from(0x50);
        res_client.alloc(Badge::null(), glenda::cap::CapType::Endpoint, 0, notify_slot)?;
        let notify_ep = glenda::cap::Endpoint::from(notify_slot);

        let recv_ring_slot = glenda::cap::CapPtr::from(0x51);
        let frame = reader.setup_ring(sq_entries, cq_entries, notify_ep, recv_ring_slot)?;
        res_client.mmap(Badge::null(), frame, ring_vaddr, ring_size)?;

        let ring = unsafe {
            glenda::io::uring::IoUringClient::new(glenda::io::uring::IoUringBuffer::new(
                ring_vaddr as *mut u8,
                ring_size,
                sq_entries,
                cq_entries,
            ))
        };
        reader.set_ring(ring);

        // Request Buffer
        let recv_buffer_slot = glenda::cap::CapPtr::from(0x52);
        let (frame, fossil_vaddr, size, paddr) = reader.request_shm(recv_buffer_slot)?;
        res_client.mmap(Badge::null(), frame.clone(), fossil_vaddr, size)?;

        let mut shm = SharedMemory::new(frame, fossil_vaddr, size);
        shm.set_paddr(paddr as u64);
        reader.set_shm(shm);

        // Read BPB
        let mut buf = [0u8; 512];
        reader.read_offset(0, &mut buf)?;

        let oem_name = &buf[3..11];
        let ops: Arc<dyn FatOps> = if oem_name == b"EXFAT   " {
            let bpb = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ExFatBpb) };
            let bytes_per_sector = 1u32 << bpb.bytes_per_sector_shift;
            let sectors_per_cluster = 1u32 << bpb.sectors_per_cluster_shift;

            Arc::new(ExFatOps {
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
                Arc::new(Fat16Ops {
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
                Arc::new(Fat32Ops {
                    bytes_per_sector: bytes_per_sec,
                    sectors_per_cluster: bpb.sec_per_clus,
                    fat_start_sector: bpb.rsvd_sec_cnt as u64,
                    data_start_sector: (bpb.rsvd_sec_cnt as u32 + (bpb.num_fats as u32 * fat_sz))
                        as u64,
                    root_cluster: bpb.root_clus,
                })
            }
        };

        Ok(Self { reader, ops, ring_vaddr, ring_size })
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

impl FatFs {
    pub fn open_handle(
        &mut self,
        path: &str,
        _flags: OpenFlags,
        _mode: u32,
    ) -> Result<Box<dyn FileHandleService + Send>, Error> {
        let entry = self.lookup(path)?;
        if (entry.attr & 0x10) != 0 {
            // Directory opening not fully supported in this simple handle
        }

        let cluster_hi = entry.fst_clus_hi as u32;
        let cluster_lo = entry.fst_clus_lo as u32;

        let first_cluster = (cluster_hi << 16) | cluster_lo;

        Ok(Box::new(FatFileHandle {
            reader: self.reader.clone(),
            ops: self.ops.clone(),
            first_cluster,
            pos: 0,
            size: entry.file_size as u64,
            ring_vaddr: self.ring_vaddr,
            ring_size: self.ring_size,
            uring: None,
            user_shm_base: 0,
            server_shm_base: 0,
        }))
    }

    pub fn mkdir(&mut self, _path: &str, _mode: u32) -> Result<(), Error> {
        Ok(())
    }

    pub fn unlink(&mut self, _path: &str) -> Result<(), Error> {
        Ok(())
    }

    pub fn stat_path(&mut self, path: &str) -> Result<Stat, Error> {
        let entry = self.lookup(path)?;
        let mut stat = Stat::default();
        stat.size = entry.file_size as u64;
        stat.mode = if (entry.attr & 0x10) != 0 { 0o040755 } else { 0o100644 };
        Ok(stat)
    }

    pub fn rename(&mut self, _old_path: &str, _new_path: &str) -> Result<(), Error> {
        Err(Error::NotImplemented)
    }
}

pub struct FatFileHandle {
    reader: BlockReader,
    ops: Arc<dyn FatOps>,
    first_cluster: u32,
    pos: u64,
    size: u64,
    ring_vaddr: usize,
    ring_size: usize,
    uring: Option<glenda::io::uring::IoUringBuffer>,
    user_shm_base: usize,
    server_shm_base: usize,
}

impl FatFileHandle {
    fn get_cluster_by_pos(&self, pos: u64) -> Result<u32, Error> {
        let cluster_size = (self.ops.sectors_per_cluster() * self.ops.bytes_per_sector()) as u64;
        let cluster_index = (pos / cluster_size) as u32;

        // Simple linear scan from start. Optimizations: cache current cluster key.
        let mut curr = self.first_cluster;
        for _ in 0..cluster_index {
            curr = self.ops.get_next_cluster(&self.reader, curr)?;
            if curr >= 0x0FFFFFF8 {
                return Err(Error::IoError); // Unexpected EOF in chain
            }
        }
        Ok(curr)
    }

    fn read_shm_internal(&self, offset: u64, len: u32, shm_vaddr: usize) -> Result<usize, Error> {
        if offset >= self.size {
            return Ok(0);
        }

        let read_len = core::cmp::min(len as u64, self.size - offset) as usize;
        let cluster_size = (self.ops.sectors_per_cluster() * self.ops.bytes_per_sector()) as u64;

        let mut current_pos = offset;
        let mut current_shm_vaddr = shm_vaddr;
        let mut remaining = read_len;

        while remaining > 0 {
            let current_cluster = self.get_cluster_by_pos(current_pos)?;
            let cluster_offset = (current_pos % cluster_size) as usize;
            let bytes_left_in_cluster = cluster_size as usize - cluster_offset;
            let chunk_len = core::cmp::min(remaining, bytes_left_in_cluster);

            let cluster_start_sector = self.ops.cluster_to_sector(current_cluster);
            let abs_offset =
                cluster_start_sector * (self.ops.bytes_per_sector() as u64) + cluster_offset as u64;

            self.reader.read_shm(abs_offset, chunk_len as u32, current_shm_vaddr)?;

            current_pos += chunk_len as u64;
            current_shm_vaddr += chunk_len;
            remaining -= chunk_len;
        }

        Ok(read_len)
    }
}

impl FileHandleService for FatFileHandle {
    fn read(&mut self, _badge: Badge, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if offset >= self.size {
            return Ok(0);
        }

        let read_len = core::cmp::min(buf.len() as u64, self.size - offset) as usize;
        if read_len == 0 {
            return Ok(0);
        }

        let cluster_size = (self.ops.sectors_per_cluster() * self.ops.bytes_per_sector()) as u64;
        let mut buf_offset = 0;
        let mut current_pos = offset;

        while buf_offset < read_len {
            let current_cluster = self.get_cluster_by_pos(current_pos)?;
            let cluster_offset = (current_pos % cluster_size) as usize;
            let bytes_left_in_cluster = cluster_size as usize - cluster_offset;
            let bytes_to_read = core::cmp::min(read_len - buf_offset, bytes_left_in_cluster);

            // Calculate physical sector
            let sector_in_cluster = (cluster_offset as u32) / self.ops.bytes_per_sector();
            let sector_offset = (cluster_offset as u32) % self.ops.bytes_per_sector();

            // For simplicity, we can read the whole cluster or do sector logic.
            // Let's use ops helper to find sector start of cluster.
            let cluster_start_sector = self.ops.cluster_to_sector(current_cluster);
            let target_sector = cluster_start_sector + sector_in_cluster as u64;

            // Read sector
            // Optimization: if bytes_to_read spans multiple sectors, handle it.
            // Here we assume BlockReader works on bytes via read_offset.
            let abs_offset =
                target_sector * (self.ops.bytes_per_sector() as u64) + sector_offset as u64;

            self.reader
                .read_offset(abs_offset, &mut buf[buf_offset..buf_offset + bytes_to_read])?;

            current_pos += bytes_to_read as u64;
            buf_offset += bytes_to_read;
        }

        self.pos = current_pos;
        Ok(read_len)
    }

    fn write(&mut self, _badge: Badge, _offset: u64, _buf: &[u8]) -> Result<usize, Error> {
        // Read-only for now
        Ok(0)
    }

    fn close(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn stat(&self, _badge: Badge) -> Result<Stat, Error> {
        let mut stat = Stat::default();
        stat.size = self.size;
        stat.mode = 0o100644;
        Ok(stat)
    }

    fn getdents(&mut self, _badge: Badge, _count: usize) -> Result<Vec<DEntry>, Error> {
        Err(Error::NotImplemented)
    }

    fn seek(&mut self, _badge: Badge, _offset: i64, _whence: usize) -> Result<u64, Error> {
        Err(Error::NotImplemented)
    }

    fn sync(&mut self, _badge: Badge) -> Result<(), Error> {
        Ok(())
    }

    fn truncate(&mut self, _badge: Badge, _size: u64) -> Result<(), Error> {
        Err(Error::NotImplemented)
    }

    fn setup_iouring(
        &mut self,
        _badge: Badge,
        server_vaddr: usize,
        client_vaddr: usize,
        size: usize,
        frame: Option<glenda::cap::Frame>,
    ) -> Result<(), Error> {
        self.server_shm_base = server_vaddr;
        self.user_shm_base = client_vaddr;
        self.uring = Some(unsafe {
            glenda::io::uring::IoUringBuffer::attach(server_vaddr as *mut u8, size)
        });

        if let Some(f) = frame {
            let shm = glenda::mem::shm::SharedMemory::new(f, server_vaddr, size);
            self.reader.set_shm(shm);
        }
        Ok(())
    }

    fn process_iouring(&mut self, _badge: Badge) -> Result<(), Error> {
        if let Some(ring) = self.uring.take() {
            while let Some(sqe) = ring.pop_sqe() {
                use glenda::io::uring::{IoUringCqe, IOURING_OP_READ};

                let res = match sqe.opcode {
                    IOURING_OP_READ => {
                        let addr = sqe.addr as usize;
                        let len = sqe.len as u32;
                        let offset = sqe.off as u64;

                        if addr < self.user_shm_base {
                            -(Error::InvalidArgs as i32)
                        } else {
                            let server_addr = addr - self.user_shm_base + self.server_shm_base;
                            match self.read_shm_internal(offset, len, server_addr) {
                                Ok(n) => n as i32,
                                Err(e) => -(e as i32),
                            }
                        }
                    }
                    _ => -(Error::NotSupported as i32),
                };

                let cqe = IoUringCqe { user_data: sqe.user_data, res, flags: 0 };
                ring.push_cqe(cqe).ok();
            }
            self.uring = Some(ring);
        }
        Ok(())
    }
}
