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

pub use server::Ext4Service;

use glenda::{error, log, warn};

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("ExtFS");
    let mut service = Ext4Service::new();
    service.run().expect("Ext4 service crashed");
    0
}
