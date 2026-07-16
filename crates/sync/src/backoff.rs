use kernel_arch_x86_64::cycles::pause;

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    step: u32,
    max_step: u32,
}

impl Backoff {
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            step: 1,
            max_step: 64,
        }
    }

    #[inline(always)]
    pub const fn with_max(max_step: u32) -> Self {
        Self { step: 1, max_step }
    }

    #[inline(always)]
    pub fn snooze(&mut self) {
        for _ in 0..self.step {
            pause();
        }
        self.step = (self.step << 1).min(self.max_step).max(1);
    }

    #[inline(always)]
    pub fn reset(&mut self) {
        self.step = 1;
    }
}

impl Default for Backoff {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}
