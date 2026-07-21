//! Split virtqueues (virtio 1.0).
//!
//! A queue is three device-visible regions — the descriptor table, the
//! available ring (driver→device), and the used ring (device→driver). Each
//! is placed in its own physical frame allocated from the kernel frame
//! allocator and accessed by the driver through the direct map; the device
//! is handed the three physical addresses.
//!
//! ## Queue-size approach
//!
//! `queue_size` is capped at [`MAX_QUEUE_SIZE`] (256). At that cap the
//! descriptor table is exactly one 4 KiB frame (16 B × 256) and both rings
//! fit comfortably in a frame each, so every region is one page — no
//! multi-frame contiguous allocation is needed. Ring contents are reached by
//! computed offsets and volatile accesses rather than as Rust arrays,
//! because the device reads/writes them concurrently (they are MMIO-like
//! shared memory, and this project does not trust aggregate copies).
//!
//! ## The index-wrap trap
//!
//! `avail.idx` and `used.idx` are free-running u16 counters that wrap at
//! 65536, **not** at `queue_size`. The slot within a ring is `idx %
//! queue_size`. [`ring_slot`] is the single place that arithmetic lives.

use core::sync::atomic::{Ordering, fence};
use kernel_memory::alloc_frame;

/// Direct-map base; must match kernel-paging / kernel-proc.
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// Largest queue this driver will use. Caps the device's advertised size so
/// every ring region fits in a single 4 KiB frame.
pub const MAX_QUEUE_SIZE: u16 = 256;

// ─── descriptor / ring layouts (device ABI) ────────────────────────────────

/// A virtqueue descriptor: points at one buffer segment.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

const _: () = assert!(core::mem::size_of::<VirtqDesc>() == 16, "VirtqDesc must be 16 bytes");

/// One completed descriptor chain, written by the device into the used ring.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

const _: () = assert!(
    core::mem::size_of::<VirtqUsedElem>() == 8,
    "VirtqUsedElem must be 8 bytes"
);

/// Descriptor flag: buffer continues in `next`.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: device writes into this buffer (RX).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

// Available-ring field byte offsets (VirtqAvail { flags:u16, idx:u16, ring:[u16] }).
const AVAIL_FLAGS_OFF: usize = 0;
const AVAIL_IDX_OFF: usize = 2;
const AVAIL_RING_OFF: usize = 4;

// Used-ring field byte offsets (VirtqUsed { flags:u16, idx:u16, ring:[UsedElem] }).
const USED_FLAGS_OFF: usize = 0;
const USED_IDX_OFF: usize = 2;
const USED_RING_OFF: usize = 4;

/// Ring slot for a free-running index and a given queue size.
///
/// The wrap that bites everyone: `idx` wraps at 65536, the ring index is
/// `idx % queue_size`.
#[inline]
pub const fn ring_slot(idx: u16, queue_size: u16) -> usize {
    (idx % queue_size) as usize
}

// ─── the queue ──────────────────────────────────────────────────────────────

/// One split virtqueue's driver-side state.
pub struct Virtqueue {
    pub queue_size: u16,
    pub desc_phys: u64,
    pub avail_phys: u64,
    pub used_phys: u64,
    desc_va: usize,
    avail_va: usize,
    used_va: usize,
    /// Next available-ring index to publish (free-running).
    avail_idx: u16,
    /// Last used-ring index the driver has consumed (free-running).
    last_used_idx: u16,
}

impl Virtqueue {
    /// Allocate and zero the three ring regions (one frame each) for a queue
    /// of `queue_size` entries. Panics if `queue_size` exceeds
    /// [`MAX_QUEUE_SIZE`] (the caller must cap it first).
    ///
    /// # Safety
    /// Called at boot on the BSP; installs DMA memory the device will access.
    pub unsafe fn new(queue_size: u16) -> Self {
        assert!(queue_size <= MAX_QUEUE_SIZE, "queue size exceeds cap");
        let desc_phys = alloc_frame().as_u64();
        let avail_phys = alloc_frame().as_u64();
        let used_phys = alloc_frame().as_u64();
        let desc_va = (DIRECT_MAP_BASE + desc_phys) as usize;
        let avail_va = (DIRECT_MAP_BASE + avail_phys) as usize;
        let used_va = (DIRECT_MAP_BASE + used_phys) as usize;
        // SAFETY: three freshly allocated frames, each visible through the
        // direct map; zero them so unused descriptors/rings read clean.
        unsafe {
            core::ptr::write_bytes(desc_va as *mut u8, 0, 4096);
            core::ptr::write_bytes(avail_va as *mut u8, 0, 4096);
            core::ptr::write_bytes(used_va as *mut u8, 0, 4096);
        }
        Self {
            queue_size,
            desc_phys,
            avail_phys,
            used_phys,
            desc_va,
            avail_va,
            used_va,
            avail_idx: 0,
            last_used_idx: 0,
        }
    }

    /// Write one descriptor slot (volatile, field by field).
    ///
    /// # Safety
    /// `index < queue_size`; the descriptor region is mapped.
    pub unsafe fn write_desc(&self, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let p = (self.desc_va + index * 16) as *mut u8;
        // SAFETY: index bounded by the caller; region is a live mapped frame.
        unsafe {
            core::ptr::write_volatile(p.cast::<u64>(), addr);
            core::ptr::write_volatile(p.add(8).cast::<u32>(), len);
            core::ptr::write_volatile(p.add(12).cast::<u16>(), flags);
            core::ptr::write_volatile(p.add(14).cast::<u16>(), next);
        }
    }

    /// Publish descriptor `desc_index` into the available ring and advance
    /// the driver's index. Does NOT notify the device — the caller batches
    /// notifications. A release fence orders the descriptor/ring writes
    /// before the visible idx bump.
    ///
    /// # Safety
    /// The ring regions are mapped; `desc_index < queue_size`.
    pub unsafe fn push_avail(&mut self, desc_index: u16) {
        let slot = ring_slot(self.avail_idx, self.queue_size);
        let ring_entry = (self.avail_va + AVAIL_RING_OFF + slot * 2) as *mut u16;
        // SAFETY: slot bounded by queue_size; avail region is a live frame.
        unsafe { core::ptr::write_volatile(ring_entry, desc_index) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // Ensure the ring entry (and the descriptor it refers to) are visible
        // before the device sees the new idx.
        fence(Ordering::Release);
        let idx_ptr = (self.avail_va + AVAIL_IDX_OFF) as *mut u16;
        // SAFETY: avail region is a live frame.
        unsafe { core::ptr::write_volatile(idx_ptr, self.avail_idx) };
    }

    /// Read the device's current used-ring index (volatile).
    ///
    /// # Safety: used region is mapped.
    pub unsafe fn used_idx(&self) -> u16 {
        let idx_ptr = (self.used_va + USED_IDX_OFF) as *const u16;
        // SAFETY: used region is a live frame; acquire so subsequent reads of
        // the used ring entries see the device's writes.
        let idx = unsafe { core::ptr::read_volatile(idx_ptr) };
        fence(Ordering::Acquire);
        idx
    }

    /// Pop the next completed used-ring element if the device has advanced
    /// past the driver's cursor. Returns `(descriptor_id, written_len)`.
    ///
    /// # Safety: used region is mapped.
    pub unsafe fn pop_used(&mut self) -> Option<(u32, u32)> {
        // SAFETY: used region mapped.
        let dev_idx = unsafe { self.used_idx() };
        if dev_idx == self.last_used_idx {
            return None;
        }
        let slot = ring_slot(self.last_used_idx, self.queue_size);
        let elem = (self.used_va + USED_RING_OFF + slot * 8) as *const u8;
        // SAFETY: slot bounded; used region is a live frame.
        let (id, len) = unsafe {
            (
                core::ptr::read_volatile(elem.cast::<u32>()),
                core::ptr::read_volatile(elem.add(4).cast::<u32>()),
            )
        };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((id, len))
    }

    /// Clear the available ring's "no interrupt" flag (we want interrupts).
    ///
    /// # Safety: avail region is mapped.
    pub unsafe fn set_avail_flags(&self, flags: u16) {
        let p = (self.avail_va + AVAIL_FLAGS_OFF) as *mut u16;
        // SAFETY: avail region is a live frame.
        unsafe { core::ptr::write_volatile(p, flags) };
    }

    /// Read the used ring's flags (device sets bit 0 to suppress notifies).
    ///
    /// # Safety: used region is mapped.
    pub unsafe fn used_flags(&self) -> u16 {
        let p = (self.used_va + USED_FLAGS_OFF) as *const u16;
        // SAFETY: used region is a live frame.
        unsafe { core::ptr::read_volatile(p) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_and_used_elem_sizes_match_the_spec() {
        assert_eq!(core::mem::size_of::<VirtqDesc>(), 16);
        assert_eq!(core::mem::size_of::<VirtqUsedElem>(), 8);
    }

    #[test]
    fn ring_slot_wraps_at_queue_size_not_at_65536() {
        // Well inside the range.
        assert_eq!(ring_slot(0, 256), 0);
        assert_eq!(ring_slot(5, 256), 5);
        assert_eq!(ring_slot(256, 256), 0);
        assert_eq!(ring_slot(257, 256), 1);
    }

    #[test]
    fn ring_slot_is_correct_across_the_u16_wrap_boundary() {
        // idx just below the u16 wrap, queue_size 256. 65535 % 256 = 255.
        assert_eq!(ring_slot(65535, 256), 255);
        // The next index wraps to 0 (u16), slot 0.
        assert_eq!(ring_slot(0u16.wrapping_sub(1).wrapping_add(1), 256), 0);
        // A non-power-of-two queue size still uses modulo, not masking.
        assert_eq!(ring_slot(65535, 100), (65535 % 100) as usize);
        assert_eq!(ring_slot(200, 100), 0);
    }

    #[test]
    fn desc_flags_have_the_spec_values() {
        assert_eq!(VIRTQ_DESC_F_NEXT, 1);
        assert_eq!(VIRTQ_DESC_F_WRITE, 2);
    }

    #[test]
    fn max_queue_size_keeps_the_descriptor_table_within_one_frame() {
        assert_eq!(MAX_QUEUE_SIZE as usize * 16, 4096);
    }
}
