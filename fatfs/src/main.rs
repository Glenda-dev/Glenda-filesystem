#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

use glenda::cap::{CapType, ENDPOINT_CAP, ENDPOINT_SLOT, RECV_SLOT, REPLY_SLOT};
use glenda::interface::system::SystemService;
use glenda::interface::ResourceService;
use glenda::interface::VolumeService;
use glenda::ipc::Badge;
use glenda::protocol::init::ServiceState;
use glenda::utils::manager::{CSpaceManager, VSpaceManager};
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
    let mut cspace = CSpaceManager::new(glenda::cap::CSPACE_CAP, 16);
    let mut vspace = VSpaceManager::new(glenda::cap::VSPACE_CAP, 0x7000_0000, 0x8000_0000);

    res_client
        .alloc(Badge::null(), CapType::Endpoint, 0, ENDPOINT_SLOT)
        .expect("FatFS: Failed to alloc endpoint");
    res_client
        .get_cap(
            Badge::null(),
            glenda::protocol::resource::ResourceType::Endpoint,
            glenda::protocol::resource::VOLUME_ENDPOINT,
            VOLUME_SLOT,
        )
        .expect("FatFS: Failed to get volume endpoint");

    let mut vol_client = glenda::client::VolumeClient::new_simple(VOLUME_CAP, &res_client);
    let block_device = vol_client
        .get_device(Badge::null(), DEVICE_SLOT)
        .expect("FatFS: Failed to get block device");

    let mut service = FatFsService::new(RING_VADDR, RING_SIZE, &mut cspace, &mut vspace);
    service.init_fs(block_device, &mut res_client).expect("Failed to init FatFS");

    service
        .listen(ENDPOINT_CAP, REPLY_SLOT, RECV_SLOT)
        .expect("FatFS: Failed to listen on endpoint");
    if let Err(e) = service.init() {
        let _ = vol_client.report_state(Badge::null(), ServiceState::Failed, None);
        panic!("FatFS: Failed to init system service: {:?}", e);
    }

    if let Err(e) =
        vol_client.report_state(Badge::null(), ServiceState::Running, Some(ENDPOINT_SLOT))
    {
        panic!("FatFS: Failed to report running state: {:?}", e);
    }

    service.run().expect("FatFs service crashed");
    0
}
