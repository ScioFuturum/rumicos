#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod ioapic;
pub mod ipi;
pub mod lapic;
pub mod pic;
pub mod pit;
pub mod timer;

pub use ioapic::{
    IoApicInfo, IoApicList, IsoInfo, IsoList, KEYBOARD_VECTOR, MAX_IOAPICS, MAX_ISOS,
};
pub use ipi::{IpiDelivery, IpiDest, send_ipi};
pub use pic::disable_pic;
pub use lapic::{
    LVT_MASKED, LVT_TIMER_ONESHOT, LVT_TIMER_PERIODIC, apic_id, eoi, init_ap, init_bsp, send_init,
    send_sipi, timer_current_count, timer_set_divide, timer_set_initial_count, timer_set_lvt,
    write_icr,
};
pub use pit::PitChannel;
pub use timer::{TIMER_VECTOR, calibrate_timer, set_timer_oneshot};
