use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint};
pub const DEVICE_SLOT: CapPtr = CapPtr::from(10);
pub const VOLUME_SLOT: CapPtr = CapPtr::from(11);
pub const RING_SLOT: CapPtr = CapPtr::from(12);

pub const NOTIFY_SLOT: CapPtr = CapPtr::from(13);
pub const RECV_RING_SLOT: CapPtr = CapPtr::from(14);
pub const RECV_BUFFER_SLOT: CapPtr = CapPtr::from(15);

pub const VOLUME_CAP: Endpoint = Endpoint::from(VOLUME_SLOT);

pub const RING_VADDR: usize = 0x5000_0000;
pub const RING_SIZE: usize = PGSIZE;
