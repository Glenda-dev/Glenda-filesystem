#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use glenda::interface::system::SystemService;
use glenda::interface::ResourceService;
use glenda::ipc::Badge;
use layout::{DEVICE_SLOT, RING_SIZE, RING_VADDR, VOLUME_CAP, VOLUME_SLOT};

mod block;
mod defs;
mod fs;
mod layout;
mod ops;
mod server;
mod versions;

pub use server::FatFsService;

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("FatFS");

    let mut res_client = glenda::client::ResourceClient::new(glenda::cap::MONITOR_CAP);
    res_client
        .get_cap(
            glenda::ipc::Badge::null(),
            glenda::protocol::resource::ResourceType::Endpoint,
            glenda::protocol::resource::VOLUME_ENDPOINT,
            VOLUME_SLOT,
        )
        .expect("FatFS: Failed to get volume endpoint");

    let vol_client = glenda::client::VolumeClient::new_simple(VOLUME_CAP, &res_client);
    let block_device = vol_client
        .get_device(Badge::null(), DEVICE_SLOT)
        .expect("FatFS: Failed to get block device");

    let mut service = FatFsService::new(RING_VADDR, RING_SIZE);
    service.init_fs(block_device, &mut res_client).expect("Failed to init FatFS");

    service.run().expect("FatFs service crashed");
    0
}
