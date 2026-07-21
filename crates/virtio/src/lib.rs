//! Minimal modern virtio-net driver: enough to transmit and receive raw
//! Ethernet frames. No IP/ARP/TCP/sockets — those arrive with the network
//! stack. Scope, deliberately small: one device, MSI-X RX interrupts, TX by
//! polling, the `VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC` feature set only.
//!
//! Layers: [`device`] (virtio PCI transport + init handshake), [`queue`]
//! (split virtqueues), [`net`] (the net device, buffers, TX/RX). This module
//! wires them to a real PCI device found by `kernel_pci` and delivers RX
//! completions through an MSI-X vector straight to a LAPIC.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod device;
pub mod net;
pub mod queue;

use device::{VirtioCaps, VirtioTransport};
use kernel_pci::PciDevice;
use kernel_sync::SpinLock;
use net::{NetDevice, RX_QUEUE, TX_QUEUE};
use queue::{MAX_QUEUE_SIZE, Virtqueue};

/// virtio vendor ID.
pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
/// PCI class for a network controller.
const PCI_CLASS_NETWORK: u8 = 0x02;
/// MSI-X "no vector" sentinel — disables interrupts for a queue/config.
const VIRTIO_MSI_NO_VECTOR: u16 = 0xffff;
/// MSI-X table index the RX queue's interrupts use.
const RX_MSIX_TABLE_INDEX: u16 = 0;

// SAFETY: NetDevice holds device-memory addresses as integers; all access is
// serialised by the SpinLock below, which is the synchronization boundary.
unsafe impl Send for NetDevice {}

/// The one initialized virtio-net device, for the RX interrupt handler.
static VIRTIO_NET: SpinLock<Option<NetDevice>> = SpinLock::new(None);

/// What [`init`] reports back for the boot self-test.
pub struct InitResult {
    pub mac: [u8; 6],
    pub tx_ok: bool,
    /// The device ID actually presented (0x1000 transitional / 0x1041 modern).
    pub device_id: u16,
}

/// Find the first virtio network device (`vendor 0x1AF4`, class 0x02).
fn find_virtio_net() -> Option<PciDevice> {
    let n = kernel_pci::count();
    for i in 0..n {
        if let Some(d) = kernel_pci::get(i)
            && d.vendor_id == VIRTIO_VENDOR_ID
            && d.class == PCI_CLASS_NETWORK
        {
            return Some(d);
        }
    }
    None
}

/// Map every MMIO BAR the device implements into the kernel direct map, so
/// the virtio config structures and the MSI-X table are reachable via
/// `DIRECT_MAP_BASE + bar`.
///
/// # Safety
/// Early boot on the BSP, before any user address space exists (see
/// `kernel_paging::mmio::map_mmio_region`).
unsafe fn map_device_bars(dev: &PciDevice) {
    for i in 0..6 {
        if dev.bar_is_mmio[i] && dev.bar_sizes[i] != 0 {
            // SAFETY: the BAR names real device MMIO; boot-time mapping.
            unsafe { kernel_paging::mmio::map_mmio_region(dev.bars[i], dev.bar_sizes[i]) };
        }
    }
}

/// Build a broadcast ARP request from `mac` as a self-test transmit frame.
/// 42 bytes: 14-byte Ethernet header + 28-byte ARP body, asking who has
/// 0.0.0.0. Well-formed enough to appear in a pcap dump and to elicit no
/// harm on QEMU user networking.
fn build_arp_probe(mac: &[u8; 6]) -> [u8; 42] {
    let mut f = [0u8; 42];
    f[0..6].copy_from_slice(&[0xff; 6]); // dst: broadcast
    f[6..12].copy_from_slice(mac); // src: our MAC
    f[12..14].copy_from_slice(&[0x08, 0x06]); // ethertype: ARP
    f[14..16].copy_from_slice(&[0x00, 0x01]); // htype: Ethernet
    f[16..18].copy_from_slice(&[0x08, 0x00]); // ptype: IPv4
    f[18] = 6; // hlen
    f[19] = 4; // plen
    f[20..22].copy_from_slice(&[0x00, 0x01]); // oper: request
    f[22..28].copy_from_slice(mac); // sender hardware addr
    // sender/target protocol addrs and target hw addr stay zero.
    f
}

/// Initialize the virtio-net device end to end and run the boot self-test:
/// negotiate, set up queues, enable MSI-X RX delivery, post receive buffers,
/// go `DRIVER_OK`, then transmit one ARP probe and confirm its completion.
///
/// `rx_vector` is the CPU interrupt vector RX completions should deliver to
/// (its handler must be registered by the caller); `dest_apic_id` is the BSP.
///
/// Returns `None` if no device is present or negotiation fails.
///
/// # Safety
/// Run once at boot on the BSP, before user address spaces are created and
/// after PCI enumeration.
pub unsafe fn init(dest_apic_id: u32, rx_vector: u8) -> Option<InitResult> {
    let dev = find_virtio_net()?;

    // SAFETY: boot-time BAR mapping for a real device.
    unsafe { map_device_bars(&dev) };

    // SAFETY: ring-0 config-space walk.
    let caps: VirtioCaps = unsafe { device::walk_virtio_caps(&dev) };
    if !caps.common.present || !caps.notify.present || !caps.device.present {
        return None; // not a modern virtio device
    }
    let transport = VirtioTransport::new(&dev, &caps);

    // SAFETY: common region mapped; runs the init handshake.
    if !unsafe { transport.negotiate() } {
        return None; // legacy-only or rejected our feature set
    }

    // Queue sizes: cap the device's advertised size at our fixed maximum.
    // SAFETY: common region mapped.
    let rx_size = unsafe { transport.queue_size(RX_QUEUE) }.min(MAX_QUEUE_SIZE);
    let tx_size = unsafe { transport.queue_size(TX_QUEUE) }.min(MAX_QUEUE_SIZE);
    if rx_size == 0 || tx_size == 0 {
        return None;
    }

    // Allocate the ring memory and program the queues. RX delivers via MSI-X
    // table entry 0; TX is polled, so it uses NO_VECTOR.
    // SAFETY: boot context; installs DMA rings.
    let rx = unsafe { Virtqueue::new(rx_size) };
    let tx = unsafe { Virtqueue::new(tx_size) };
    // SAFETY: common region mapped; ring frames live.
    let rx_notify_off = unsafe {
        transport.setup_queue(
            RX_QUEUE,
            rx.queue_size,
            rx.desc_phys,
            rx.avail_phys,
            rx.used_phys,
            RX_MSIX_TABLE_INDEX,
        )
    };
    // SAFETY: as above.
    let tx_notify_off = unsafe {
        transport.setup_queue(
            TX_QUEUE,
            tx.queue_size,
            tx.desc_phys,
            tx.avail_phys,
            tx.used_phys,
            VIRTIO_MSI_NO_VECTOR,
        )
    };

    // Program MSI-X table entry 0 to deliver `rx_vector` to the BSP and
    // enable MSI-X on the device. The table BAR was mapped above.
    // SAFETY: the device has an MSI-X capability (table size ≥ 1) and its
    // table BAR is mapped.
    unsafe {
        kernel_pci::msix::msix_enable(&dev, RX_MSIX_TABLE_INDEX, rx_vector, dest_apic_id);
    }

    // SAFETY: transport negotiated, queues set up, device-config mapped.
    let mut net_dev = unsafe { NetDevice::new(transport, rx, tx, rx_notify_off, tx_notify_off) };
    let mac = net_dev.mac;

    // Post receive buffers before going live, then DRIVER_OK.
    // SAFETY: queues/buffers live.
    unsafe {
        net_dev.fill_rx();
        net_dev.transport.set_driver_ok();
    }

    // Self-test transmit: one ARP probe, confirmed via the TX used ring.
    let probe = build_arp_probe(&mac);
    // SAFETY: TX queue/buffer live.
    let tx_ok = unsafe { net_dev.transmit(&probe) };

    *VIRTIO_NET.lock() = Some(net_dev);

    Some(InitResult {
        mac,
        tx_ok,
        device_id: dev.device_id,
    })
}

/// Drain completed receive descriptors, invoking `on_frame` for each frame.
/// Called from the RX MSI-X interrupt handler (registered by the kernel)
/// AFTER it sends EOI. Never blocks or schedules.
///
/// # Safety
/// Interrupt context on the BSP; the device was initialized by [`init`].
pub unsafe fn rx_poll(on_frame: impl FnMut(&[u8])) {
    let mut guard = VIRTIO_NET.lock();
    if let Some(dev) = guard.as_mut() {
        // SAFETY: device initialized; RX rings/buffers live.
        unsafe { dev.poll_rx(on_frame) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arp_probe_is_well_formed() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let f = build_arp_probe(&mac);
        assert_eq!(&f[0..6], &[0xff; 6], "broadcast destination");
        assert_eq!(&f[6..12], &mac, "our MAC as source");
        assert_eq!(&f[12..14], &[0x08, 0x06], "ARP ethertype");
        assert_eq!(&f[20..22], &[0x00, 0x01], "ARP request opcode");
        assert_eq!(&f[22..28], &mac, "sender hardware address");
        assert_eq!(f.len(), 42);
    }

    #[test]
    fn virtio_constants() {
        assert_eq!(VIRTIO_VENDOR_ID, 0x1af4);
        assert_eq!(PCI_CLASS_NETWORK, 0x02);
        assert_eq!(VIRTIO_MSI_NO_VECTOR, 0xffff);
    }
}
