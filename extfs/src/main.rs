#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use glenda::interface::system::SystemService;
use glenda::interface::{ResourceService, VolumeService};
use glenda::ipc::Badge;

mod block;
mod defs;
mod fs;
mod ops;
mod server;
mod versions;

pub use server::Ext4Service;

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("ExtFS");

    let mut res_client = glenda::client::ResourceClient::new(glenda::cap::MONITOR_CAP);
    let vol_slot = glenda::cap::CapPtr::from(12);
    res_client
        .get_cap(
            glenda::ipc::Badge::null(),
            glenda::protocol::resource::ResourceType::Endpoint,
            glenda::protocol::resource::VOLUME_ENDPOINT,
            vol_slot,
        )
        .expect("ExtFS: Failed to get volume endpoint");

    let mut vol_client = glenda::client::VolumeClient::new(glenda::cap::Endpoint::from(vol_slot));
    let block_device = vol_client
        .get_device(Badge::null(), glenda::cap::CapPtr::from(13))
        .expect("ExtFS: Failed to get block device");

    let ring_vaddr = 0x6000_0000;
    let ring_size = 4096;

    let mut service = Ext4Service::new(ring_vaddr, ring_size);
    service.init_fs(block_device, &mut res_client).expect("Failed to init ExtFS");

    service.run().expect("Ext4 service crashed");
    0
}
