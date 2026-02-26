#![no_std]
#![no_main]

#[macro_use]
extern crate glenda;
extern crate alloc;

// Import common types
use glenda::cap::{CapPtr, CapType, Endpoint, ENDPOINT_CAP, ENDPOINT_SLOT, MONITOR_CAP, REPLY_CAP};
use glenda::client::{FsClient, ResourceClient, VolumeClient};
use glenda::interface::system::SystemService;
use glenda::interface::{ResourceService};
use glenda::ipc::Badge;
use glenda::protocol::resource::{FS_ENDPOINT, VOLUME_ENDPOINT};

mod fs;
mod layout;
mod server;

use layout::{DEVICE_SLOT, VFS_SLOT};

use crate::layout::VOLUME_SLOT;

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("InitrdFS");
    log!("Service starting...");

    let mut res_client = ResourceClient::new(MONITOR_CAP);

    let vol_cap = res_client
        .get_cap(
            Badge::null(),
            glenda::protocol::resource::ResourceType::Endpoint,
            VOLUME_ENDPOINT,
            VOLUME_SLOT,
        )
        .expect("Failed to get volume endpoint");
    let vol_client = VolumeClient::new_simple(Endpoint::from(vol_cap), &mut res_client);

    // Retrieve the specific block device endpoint badged for this driver
    let dev_cap =
        vol_client.get_device(Badge::null(), DEVICE_SLOT).expect("Failed to get block device");

    if let Err(e) = res_client.alloc(Badge::null(), CapType::Endpoint, 0, ENDPOINT_SLOT) {
        log!("Failed to allocate endpoint: {:?}", e);
        return 1;
    }

    let vfs_cap = res_client
        .get_cap(
            Badge::null(),
            glenda::protocol::resource::ResourceType::Endpoint,
            FS_ENDPOINT,
            VFS_SLOT,
        )
        .expect("Failed to get VFS endpoint");
    let mut vfs_client = FsClient::new(Endpoint::from(vfs_cap));

    let mut server = server::InitrdServer::new(dev_cap, &mut res_client, &mut vfs_client);

    if let Err(e) = server.listen(ENDPOINT_CAP, REPLY_CAP.cap(), CapPtr::null()) {
        log!("Failed to listen: {:?}", e);
        return 1;
    }

    if let Err(e) = server.init() {
        log!("Failed to init: {:?}", e);
        return 1;
    }

    if let Err(e) = server.run() {
        log!("InitrdFS exited with error: {:?}", e);
        return 1;
    }
    0
}
