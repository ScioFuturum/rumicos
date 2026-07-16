use crate::constants::MAX_PCID;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_arch_x86_64::cycles::pause;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Pcid {
    pub id: u16,
    pub generation: u64,
}

#[repr(C, align(64))]
pub struct PcidAllocator<const WORDS: usize> {
    generation: AtomicU64,
    bitmap: [AtomicU64; WORDS],
}

impl<const WORDS: usize> PcidAllocator<WORDS> {
    #[inline(always)]
    pub const fn new() -> Self {
        assert!(WORDS * 64 >= MAX_PCID);
        Self {
            generation: AtomicU64::new(1),
            bitmap: [const { AtomicU64::new(0) }; WORDS],
        }
    }

    pub fn alloc(&self) -> Option<Pcid> {
        for word_index in 0..WORDS {
            let base = word_index * 64;
            if base >= MAX_PCID {
                break;
            }

            let mut word = self.bitmap[word_index].load(Ordering::Relaxed);
            loop {
                let mut free = !word;
                if word_index == 0 {
                    free &= !1u64;
                }
                if free == 0 {
                    break;
                }

                let bit = free.trailing_zeros() as usize;
                let id = base + bit;
                if id >= MAX_PCID {
                    break;
                }

                let mask = 1u64 << bit;
                match self.bitmap[word_index].compare_exchange_weak(
                    word,
                    word | mask,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        return Some(Pcid {
                            id: id as u16,
                            generation: self.generation.load(Ordering::Acquire),
                        });
                    }
                    Err(observed) => {
                        word = observed;
                        pause();
                    }
                }
            }
        }

        None
    }

    pub fn free(&self, pcid: Pcid) {
        let id = pcid.id as usize;
        if id == 0 || id >= MAX_PCID {
            return;
        }

        let word = id / 64;
        let bit = id % 64;
        self.bitmap[word].fetch_and(!(1u64 << bit), Ordering::AcqRel);
    }

    #[inline(always)]
    pub fn bump_generation(&self) -> u64 {
        self.generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }
}

impl<const WORDS: usize> Default for PcidAllocator<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::PcidAllocator;

    #[test]
    fn allocator_skips_reserved_zero_but_uses_first_word() {
        let allocator = PcidAllocator::<64>::new();
        assert_eq!(allocator.alloc().unwrap().id, 1);
        assert_eq!(allocator.alloc().unwrap().id, 2);
    }
}
