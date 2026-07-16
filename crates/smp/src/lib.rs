#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod acpi;
pub mod ap_entry;
pub mod cpuinfo;
pub mod trampoline;

pub use acpi::{CpuEntry, MAX_CPUS, parse_madt};
pub use cpuinfo::{
    CpuInfo, apic_id_for_cpu, apic_id_to_cpu_id, cpu_count, online_count, register_cpu,
};
pub use trampoline::{TRAMPOLINE_PHYS, TRAMPOLINE_SIZE, start_aps};
