//! virtio-net: the network device on top of the virtio transport.
//!
//! Queue 0 is receive, queue 1 is transmit. Every packet buffer (both
//! directions) is prefixed by a 12-byte [`VirtioNetHdr`]; with no offload
//! features negotiated the header is simply zeroed on transmit and ignored
//! (past the length) on receive.

use crate::device::VirtioTransport;
use crate::queue::{VIRTQ_DESC_F_WRITE, Virtqueue};
use kernel_memory::alloc_frame;

const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// Receive queue index.
pub const RX_QUEUE: u16 = 0;
/// Transmit queue index.
pub const TX_QUEUE: u16 = 1;

/// How many receive buffers to keep posted.
pub const RX_BUFFER_COUNT: usize = 16;

/// Largest standard Ethernet frame (no jumbo, no VLAN growth): 1514 bytes.
pub const ETH_FRAME_MAX: usize = 1514;

/// virtio-net header size with `VIRTIO_F_VERSION_1` (12 bytes).
pub const NET_HDR_LEN: usize = 12;

/// Bytes reserved per buffer: header + a full Ethernet frame (≤ one 4 KiB frame).
const BUF_LEN: u32 = (NET_HDR_LEN + ETH_FRAME_MAX) as u32;

/// virtio-net packet header. With VERSION_1 negotiated this is 12 bytes; the
/// trailing `num_buffers` field is present (used only for merged RX buffers,
/// which we do not negotiate, but the field still occupies space).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

const _: () = assert!(
    core::mem::size_of::<VirtioNetHdr>() == 12,
    "VirtioNetHdr must be 12 bytes with VIRTIO_F_VERSION_1"
);

/// The initialized virtio-net device: transport + both queues + buffers.
pub struct NetDevice {
    pub transport: VirtioTransport,
    pub rx: Virtqueue,
    pub tx: Virtqueue,
    pub rx_notify_off: u16,
    pub tx_notify_off: u16,
    /// Physical addresses of the RX buffers, indexed by descriptor id.
    rx_bufs: [u64; RX_BUFFER_COUNT],
    /// The single TX buffer (one in-flight frame at a time for this driver).
    tx_buf: u64,
    pub mac: [u8; 6],
}

impl NetDevice {
    /// Assemble a device from a fully-negotiated transport and its two set-up
    /// queues, allocate the packet buffers, and read the MAC.
    ///
    /// # Safety
    /// `transport` has completed feature negotiation; `rx`/`tx` are set up;
    /// the device-config region is mapped.
    pub unsafe fn new(
        transport: VirtioTransport,
        rx: Virtqueue,
        tx: Virtqueue,
        rx_notify_off: u16,
        tx_notify_off: u16,
    ) -> Self {
        let mut rx_bufs = [0u64; RX_BUFFER_COUNT];
        for b in rx_bufs.iter_mut() {
            *b = alloc_frame().as_u64();
        }
        let tx_buf = alloc_frame().as_u64();
        let mut mac = [0u8; 6];
        // SAFETY: device-config region mapped and ≥ 6 bytes (MAC negotiated).
        unsafe { transport.read_device_config(&mut mac) };
        Self {
            transport,
            rx,
            tx,
            rx_notify_off,
            tx_notify_off,
            rx_bufs,
            tx_buf,
            mac,
        }
    }

    /// Post every receive buffer to the RX queue and notify the device. Each
    /// descriptor is marked WRITE so the device fills it. Call once before
    /// `set_driver_ok`.
    ///
    /// # Safety
    /// queues and buffers are live.
    pub unsafe fn fill_rx(&mut self) {
        for i in 0..RX_BUFFER_COUNT {
            // SAFETY: descriptor i and buffer i are within bounds; RX region
            // mapped. The device WRITES the buffer, hence the WRITE flag.
            unsafe {
                self.rx
                    .write_desc(i, self.rx_bufs[i], BUF_LEN, VIRTQ_DESC_F_WRITE, 0);
                self.rx.push_avail(i as u16);
            }
        }
        // SAFETY: notify region mapped.
        unsafe {
            self.transport
                .notify_queue(RX_QUEUE, self.rx_notify_off);
        }
    }

    /// Transmit one raw Ethernet frame. Zeroes the net header, copies the
    /// frame after it, posts a single read-only descriptor, notifies the
    /// device, and spins (bounded) for the used-ring completion.
    ///
    /// Returns `true` if the descriptor came back in the used ring.
    ///
    /// # Safety
    /// TX queue and buffer are live.
    pub unsafe fn transmit(&mut self, frame: &[u8]) -> bool {
        let len = frame.len().min(ETH_FRAME_MAX);
        let buf_va = (DIRECT_MAP_BASE + self.tx_buf) as *mut u8;
        // SAFETY: tx_buf is a live frame reachable via the direct map; we
        // write the 12-byte zero header then `len` frame bytes (≤ 1514),
        // well within the 4 KiB buffer.
        unsafe {
            core::ptr::write_bytes(buf_va, 0, NET_HDR_LEN);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_va.add(NET_HDR_LEN), len);
            // One read-only descriptor covering header + frame.
            self.tx
                .write_desc(0, self.tx_buf, (NET_HDR_LEN + len) as u32, 0, 0);
            self.tx.push_avail(0);
            self.transport.notify_queue(TX_QUEUE, self.tx_notify_off);
        }
        // Wait (bounded) for the device to return the descriptor.
        for _ in 0..10_000_000u32 {
            // SAFETY: TX used ring mapped.
            if unsafe { self.tx.pop_used() }.is_some() {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Drain completed receive descriptors. For each, invoke `on_frame` with
    /// the frame bytes (past the 12-byte header) and re-post the buffer.
    ///
    /// # Safety
    /// RX queue and buffers are live.
    pub unsafe fn poll_rx(&mut self, mut on_frame: impl FnMut(&[u8])) {
        // SAFETY: RX used ring mapped.
        while let Some((id, total_len)) = unsafe { self.rx.pop_used() } {
            let idx = id as usize;
            if idx < RX_BUFFER_COUNT && total_len as usize >= NET_HDR_LEN {
                let frame_len = (total_len as usize - NET_HDR_LEN).min(ETH_FRAME_MAX);
                let data = (DIRECT_MAP_BASE + self.rx_bufs[idx]) as *const u8;
                // SAFETY: buffer idx is a live frame; the device wrote
                // `total_len` bytes into it, frame data starts after the hdr.
                let frame =
                    unsafe { core::slice::from_raw_parts(data.add(NET_HDR_LEN), frame_len) };
                on_frame(frame);
                // Re-post this buffer for another receive.
                // SAFETY: descriptor idx and buffer idx in bounds; RX mapped.
                unsafe {
                    self.rx
                        .write_desc(idx, self.rx_bufs[idx], BUF_LEN, VIRTQ_DESC_F_WRITE, 0);
                    self.rx.push_avail(id as u16);
                }
            }
        }
        // SAFETY: notify region mapped.
        unsafe { self.transport.notify_queue(RX_QUEUE, self.rx_notify_off) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_header_is_twelve_bytes() {
        assert_eq!(core::mem::size_of::<VirtioNetHdr>(), 12);
        assert_eq!(NET_HDR_LEN, 12);
    }

    #[test]
    fn queue_indices_are_rx0_tx1() {
        assert_eq!(RX_QUEUE, 0);
        assert_eq!(TX_QUEUE, 1);
    }

    #[test]
    fn buffer_length_holds_header_plus_a_full_frame_within_a_page() {
        assert_eq!(BUF_LEN as usize, NET_HDR_LEN + ETH_FRAME_MAX);
        assert!(BUF_LEN as usize <= 4096, "buffer must fit one frame");
    }
}
