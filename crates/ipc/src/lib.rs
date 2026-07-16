#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod descriptor;
pub mod mpsc;
pub mod spsc;
pub mod wait;

pub use descriptor::IpcDescriptor;
pub use mpsc::MpscRing;
pub use spsc::SpscRing;
