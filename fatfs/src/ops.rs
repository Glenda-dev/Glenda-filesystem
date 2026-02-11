use crate::block::BlockReader;
use glenda::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootLocation {
    Cluster(u32),
    // sector, size_sectors
    Sector(u64, u32),
}

pub trait FatOps: Send + Sync {
    fn get_next_cluster(&self, reader: &BlockReader, cluster: u32) -> Result<u32, Error>;
    fn cluster_to_sector(&self, cluster: u32) -> u64;
    fn get_root_location(&self) -> RootLocation;
    fn bytes_per_sector(&self) -> u32;
    fn sectors_per_cluster(&self) -> u32;
}
