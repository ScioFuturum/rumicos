use crate::gdt::{DescriptorTablePointer, KERNEL_CS};
use core::arch::asm;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

const IDT_ENTRIES: usize = 256;
const GPR_SAVE_BYTES: usize = 15 * 8;
const INTERRUPT_GATE: u16 = 0x0e;
const PRESENT: u16 = 1 << 15;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
global_asm!(include_str!(concat!(env!("OUT_DIR"), "/idt_stubs.S")));

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    options: u16,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            options: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    pub const fn interrupt_gate(handler: u64, selector: u16, ist: u8, dpl: u8) -> Self {
        let options =
            ((ist as u16) & 0x07) | (INTERRUPT_GATE << 8) | (((dpl as u16) & 0x03) << 13) | PRESENT;
        Self {
            offset_low: handler as u16,
            selector,
            options,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    pub const fn options(&self) -> u16 {
        self.options
    }
}

#[repr(C, align(16))]
pub struct Idt {
    entries: [IdtEntry; IDT_ENTRIES],
}

impl Idt {
    pub const fn new() -> Self {
        Self {
            entries: [IdtEntry::missing(); IDT_ENTRIES],
        }
    }

    fn pointer(&self) -> DescriptorTablePointer {
        DescriptorTablePointer {
            limit: (mem::size_of::<Self>() - 1) as u16,
            base: ptr::from_ref(self) as u64,
        }
    }
}

impl Default for Idt {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
pub struct InterruptFrame {
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector_slot: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

type Handler = fn(&mut InterruptFrame, u8);

struct IdtCell(UnsafeCell<Idt>);

unsafe impl Sync for IdtCell {}

impl IdtCell {
    const fn new() -> Self {
        Self(UnsafeCell::new(Idt::new()))
    }

    fn get(&self) -> *mut Idt {
        self.0.get()
    }
}

static IDT: IdtCell = IdtCell::new();
static HANDLERS: [AtomicUsize; IDT_ENTRIES] = [const { AtomicUsize::new(0) }; IDT_ENTRIES];

/// Hook run at the end of interrupt dispatch when returning to ring 3, so
/// pending signals get delivered on the IRETQ path (timer/device IRQ,
/// resolved #PF), not only on syscall returns. kernel-proc registers
/// `signal::check_and_deliver_iret_hook`. Kept behind the cheap
/// `frame.cs & 3 == 3` ring check below so kernel→kernel interrupts (the
/// common case) pay only a compare + not-taken branch.
static INTERRUPT_RETURN_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the ring-3 interrupt-return hook (see [`InterruptReturnHook`]).
/// Called once by kernel-proc's init.
pub fn set_interrupt_return_hook(hook: InterruptReturnHook) {
    INTERRUPT_RETURN_HOOK.store(hook as usize, Ordering::Release);
}

/// A hook run just before an interrupt returns to ring 3, given the
/// (mutable) interrupt frame. It may edit `frame.rip`/`rsp`/`rflags`/GPRs
/// in place to redirect the IRETQ return (e.g. into a signal handler).
pub type InterruptReturnHook = fn(&mut InterruptFrame);

pub fn init_idt() {
    // SAFETY: IDT bootstrap is serialized and runs before interrupts are
    // enabled; the static table remains resident forever.
    let idt = unsafe { &mut *IDT.get() };
    for vector in 0..IDT_ENTRIES {
        let ist = match vector {
            2 | 8 | 18 => 1,
            _ => 0,
        };
        let dpl = if vector == 0x80 { 3 } else { 0 };
        idt.entries[vector] = IdtEntry::interrupt_gate(stub_address(vector), KERNEL_CS, ist, dpl);
    }
    let pointer = idt.pointer();
    // SAFETY: `pointer` describes the fully initialized static IDT.
    unsafe { lidt(&pointer) };
}

pub fn register_handler(vector: u8, handler: Handler) {
    HANDLERS[vector as usize].store(handler as usize, Ordering::Release);
}

#[unsafe(no_mangle)]
/// Dispatch an interrupt from assembly stubs into a registered Rust handler.
///
/// # Safety
/// `frame` must point to the normalized interrupt frame built by the assembly
/// stubs and remain valid for the duration of the call.
pub unsafe extern "C" fn rust_interrupt_dispatch(frame: *mut InterruptFrame, vector: u64) {
    let vector = vector as u8;
    let raw = HANDLERS[vector as usize].load(Ordering::Acquire);
    let handler = if raw == 0 {
        default_handler as Handler
    } else {
        // SAFETY: `register_handler` stores only function pointers with the
        // `Handler` signature, represented losslessly as usize on x86_64.
        unsafe { mem::transmute::<usize, Handler>(raw) }
    };
    // SAFETY: assembly stubs pass a pointer to the normalized stack frame they
    // constructed immediately before the call.
    let frame = unsafe { &mut *frame };
    handler(frame, vector);

    // Signal delivery on return to user mode: only when the interrupt hit
    // ring-3 code (CS.RPL == 3). Kernel→kernel interrupts skip this after a
    // single compare + not-taken branch, so the hot path is unaffected.
    if frame.cs & 3 == 3 {
        let hook = INTERRUPT_RETURN_HOOK.load(Ordering::Acquire);
        if hook != 0 {
            // SAFETY: set_interrupt_return_hook stores only valid
            // InterruptReturnHook function pointers.
            let hook: InterruptReturnHook = unsafe { mem::transmute(hook) };
            hook(frame);
        }
    }
}

fn default_handler(_frame: &mut InterruptFrame, _vector: u8) {}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn stub_address(vector: usize) -> u64 {
    unsafe extern "C" {
        static __interrupt_stub_table: [usize; IDT_ENTRIES];
    }

    // SAFETY: generated assembly defines a 256-entry table of valid ISR labels.
    unsafe { __interrupt_stub_table[vector] as u64 }
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn stub_address(_vector: usize) -> u64 {
    0
}

#[inline(always)]
unsafe fn lidt(pointer: &DescriptorTablePointer) {
    // SAFETY: caller guarantees `pointer` describes a valid IDT descriptor.
    unsafe {
        asm!(
            "lidt [{pointer}]",
            pointer = in(reg) pointer,
            options(readonly, nostack, preserves_flags),
        )
    };
}

const _: () = assert!(GPR_SAVE_BYTES == 120);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idt_descriptor_bit_layout() {
        let entry = IdtEntry::interrupt_gate(0x1234_5678_9abc_def0, KERNEL_CS, 1, 3);
        let options = entry.options();
        assert_eq!(options & 0x07, 1);
        assert_eq!((options >> 8) & 0x0f, 0x0e);
        assert_eq!((options >> 13) & 0x03, 3);
        assert_ne!(options & PRESENT, 0);
    }
}
