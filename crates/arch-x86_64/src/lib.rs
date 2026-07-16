#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod atomic128;
pub mod cache;
pub mod cpu;
pub mod cycles;
pub mod msr;
pub mod tlb;
pub mod xsave;

pub use cpu::{CpuFeatures, detect_cpu_features};
