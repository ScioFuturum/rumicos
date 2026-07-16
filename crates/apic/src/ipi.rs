//! Inter-processor interrupt (IPI) helpers built on top of [`crate::lapic::write_icr`].
//!
//! Before this checkpoint, `kernel-apic` only exposed `write_icr` (a raw,
//! pre-encoded ICR value) plus the two special-purpose `send_init`/
//! `send_sipi` helpers used for AP bring-up. There was no general
//! "send a vectored IPI to some destination" entry point — `send_ipi`/
//! `IpiDest`/`IpiDelivery` are new in this checkpoint (added for
//! `kernel-proc`'s TLB-shootdown broadcast, see `kernel_proc::shootdown`),
//! not a pre-existing API this checkpoint merely renamed.

use crate::lapic::write_icr;

/// Bits [19:18] of the ICR: destination-shorthand encoding. `0b00` (no
/// shorthand, use the destination field) is intentionally not a named
/// constant here — [`IpiDest::ApicId`] just leaves those bits zero.
const DEST_SHORTHAND_SELF: u64 = 0b01 << 18;
const DEST_SHORTHAND_ALL: u64 = 0b10 << 18;
const DEST_SHORTHAND_OTHERS: u64 = 0b11 << 18;

/// Bits [10:8] of the ICR: delivery-mode encoding.
const DELIVERY_MODE_FIXED: u64 = 0b000 << 8;
const DELIVERY_MODE_SMI: u64 = 0b010 << 8;
const DELIVERY_MODE_NMI: u64 = 0b100 << 8;

/// Bits [63:32] (xAPIC/x2APIC-normalized) destination-field shift, matching
/// `lapic::icr_dest`'s own (private) `ICR_DEST_SHIFT`.
const DEST_FIELD_SHIFT: u32 = 32;

/// Destination selector for an IPI, mirroring the ICR's destination-field /
/// destination-shorthand encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiDest {
    /// A specific target local APIC ID (destination field, no shorthand).
    ApicId(u32),
    /// Self only (shorthand `01`) — no destination field needed.
    Slf,
    /// Every CPU, including self (shorthand `10`).
    All,
    /// Every CPU except self (shorthand `11`) — what TLB shootdown uses.
    Others,
}

/// Delivery mode for an IPI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IpiDelivery {
    /// An ordinary vectored interrupt (delivery mode `000`) at `vector`,
    /// serviced by whatever handler `kernel_cpu::register_handler`
    /// registered for it on the target CPU.
    Fixed(u8),
    /// Non-maskable interrupt (delivery mode `100`); the CPU ignores the
    /// vector field for NMI delivery, so none is carried here.
    Nmi,
    /// System-management interrupt (delivery mode `010`).
    Smi,
}

/// Send an inter-processor interrupt.
///
/// # Safety
/// Caller must ensure the local APIC is initialized (`init_bsp`/`init_ap`
/// already ran on this CPU), `dest` names an online CPU (or CPUs) for the
/// running topology, and — for `IpiDelivery::Fixed` — that the vector is
/// registered with a handler prepared to run in interrupt context on every
/// targeted CPU (see `kernel_cpu::register_handler`).
pub unsafe fn send_ipi(dest: IpiDest, delivery: IpiDelivery) {
    let dest_bits = match dest {
        IpiDest::ApicId(id) => (id as u64) << DEST_FIELD_SHIFT,
        IpiDest::Slf => DEST_SHORTHAND_SELF,
        IpiDest::All => DEST_SHORTHAND_ALL,
        IpiDest::Others => DEST_SHORTHAND_OTHERS,
    };
    let delivery_bits = match delivery {
        IpiDelivery::Fixed(vector) => DELIVERY_MODE_FIXED | vector as u64,
        IpiDelivery::Nmi => DELIVERY_MODE_NMI,
        IpiDelivery::Smi => DELIVERY_MODE_SMI,
    };
    // SAFETY: forwards this function's own preconditions to write_icr.
    unsafe { write_icr(dest_bits | delivery_bits) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipi_dest_apic_id_encodes_destination_field_no_shorthand() {
        let dest_bits = match IpiDest::ApicId(0x0f) {
            IpiDest::ApicId(id) => (id as u64) << DEST_FIELD_SHIFT,
            _ => unreachable!(),
        };
        assert_eq!(dest_bits, 0x0f00_0000_00);
        // No shorthand bits set.
        assert_eq!(dest_bits & (0b11 << 18), 0);
    }

    #[test]
    fn ipi_dest_others_shorthand_matches_icr_encoding() {
        assert_eq!(DEST_SHORTHAND_OTHERS, 0b11 << 18);
    }

    #[test]
    fn ipi_dest_self_and_all_shorthands_are_distinct() {
        assert_ne!(DEST_SHORTHAND_SELF, DEST_SHORTHAND_ALL);
        assert_ne!(DEST_SHORTHAND_ALL, DEST_SHORTHAND_OTHERS);
        assert_ne!(DEST_SHORTHAND_SELF, DEST_SHORTHAND_OTHERS);
    }

    #[test]
    fn ipi_delivery_fixed_encodes_delivery_mode_zero_plus_vector() {
        let bits = match IpiDelivery::Fixed(0xfc) {
            IpiDelivery::Fixed(vector) => DELIVERY_MODE_FIXED | vector as u64,
            _ => unreachable!(),
        };
        assert_eq!(bits, 0xfc);
    }

    #[test]
    fn ipi_delivery_nmi_matches_icr_delivery_mode_100() {
        assert_eq!(DELIVERY_MODE_NMI, 0b100 << 8);
    }
}
