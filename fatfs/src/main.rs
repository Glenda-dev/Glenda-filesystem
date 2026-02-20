#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use glenda::interface::system::SystemService;

mod block;
mod defs;
mod fs;
mod ops;
mod server;
mod versions;

pub use server::FatFsService;

use glenda::{error, log, warn};

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("FatFS");
    // In a real scenario, we would get the block device capability from the root task or device manager.
    // For now, we assume it's passed or well-known.
    let mut service = FatFsService::new();

    // Standard service setup would go here
    // service.listen(...);

    service.run().expect("FatFs service crashed");
    0
}
