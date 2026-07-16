use core::arch::x86_64::{__cpuid_count, _xgetbv};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CpuFeatures {
    pub sse4_2: bool,
    pub avx2: bool,
    pub avx512f: bool,
    pub rdtscp: bool,
    pub clflushopt: bool,
    pub clwb: bool,
    pub prefetchw: bool,
    pub prefetchwt1: bool,
    pub pcid: bool,
    pub invpcid: bool,
    pub x2apic: bool,
    pub rtm: bool,
    pub umip: bool,
    pub smep: bool,
    pub smap: bool,
    pub pku: bool,
    pub la57: bool,
    pub fsgsbase: bool,
    pub xsave: bool,
    pub osxsave: bool,
    pub rdrand: bool,
    pub rdseed: bool,
    pub ibrs_ibpb: bool,
}

impl CpuFeatures {
    #[inline(always)]
    pub const fn empty() -> Self {
        Self {
            sse4_2: false,
            avx2: false,
            avx512f: false,
            rdtscp: false,
            clflushopt: false,
            clwb: false,
            prefetchw: false,
            prefetchwt1: false,
            pcid: false,
            invpcid: false,
            x2apic: false,
            rtm: false,
            umip: false,
            smep: false,
            smap: false,
            pku: false,
            la57: false,
            fsgsbase: false,
            xsave: false,
            osxsave: false,
            rdrand: false,
            rdseed: false,
            ibrs_ibpb: false,
        }
    }
}

#[inline(always)]
fn bit(value: u32, bit: u32) -> bool {
    (value & (1u32 << bit)) != 0
}

pub fn detect_cpu_features() -> CpuFeatures {
    let leaf0 = __cpuid_count(0, 0);
    let max_basic = leaf0.eax;
    let leaf1 = __cpuid_count(1, 0);
    let max_extended = __cpuid_count(0x8000_0000, 0).eax;

    let mut features = CpuFeatures {
        sse4_2: bit(leaf1.ecx, 20),
        pcid: bit(leaf1.ecx, 17),
        x2apic: bit(leaf1.ecx, 21),
        xsave: bit(leaf1.ecx, 26),
        osxsave: bit(leaf1.ecx, 27),
        rdrand: bit(leaf1.ecx, 30),
        ..CpuFeatures::empty()
    };

    if max_basic >= 7 {
        let leaf7 = __cpuid_count(7, 0);
        features.fsgsbase = bit(leaf7.ebx, 0);
        features.avx2 = bit(leaf7.ebx, 5);
        features.smep = bit(leaf7.ebx, 7);
        features.invpcid = bit(leaf7.ebx, 10);
        features.rtm = bit(leaf7.ebx, 11);
        features.avx512f = bit(leaf7.ebx, 16);
        features.rdseed = bit(leaf7.ebx, 18);
        features.smap = bit(leaf7.ebx, 20);
        features.clflushopt = bit(leaf7.ebx, 23);
        features.clwb = bit(leaf7.ebx, 24);
        features.prefetchwt1 = bit(leaf7.ecx, 0);
        features.umip = bit(leaf7.ecx, 2);
        features.pku = bit(leaf7.ecx, 3);
        features.la57 = bit(leaf7.ecx, 16);
        features.ibrs_ibpb = bit(leaf7.edx, 26);
    }

    if max_extended >= 0x8000_0001 {
        let leaf_ext = __cpuid_count(0x8000_0001, 0);
        features.prefetchw = bit(leaf_ext.ecx, 8);
        features.rdtscp = bit(leaf_ext.edx, 27);
    }

    if features.osxsave {
        let xcr0 = unsafe { _xgetbv(0) };
        let xmm_ymm = (xcr0 & 0b110) == 0b110;
        let opmask_zmm = (xcr0 & 0b1110_0000) == 0b1110_0000;
        features.avx2 &= xmm_ymm;
        features.avx512f &= xmm_ymm && opmask_zmm;
    } else {
        features.avx2 = false;
        features.avx512f = false;
    }

    features
}
