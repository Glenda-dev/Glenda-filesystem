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

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => ({
        glenda::println!("{}ExtFS: {}{}", glenda::console::ANSI_BLUE, format_args!($($arg)*), glenda::console::ANSI_RESET);
    })
}

#[unsafe(no_mangle)]
fn main() -> usize {
    let mut service = Ext4Service::new();
    service.run().expect("Ext4 service crashed");
    0
}
