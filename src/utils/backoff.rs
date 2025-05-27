pub(crate) struct Backoff {
    initial: u32,
    threshold: u32,
    current: u32,
}

impl Backoff {
    pub(crate) const fn new() -> Self {
        Self::with_params(1, 7)
    }

    pub(crate) const fn with_params(initial: u32, threshold_exponent: u32) -> Self {
        assert!(initial > 0, "backoff: initial value must be positive number");
        assert!(threshold_exponent > 0, "backoff: threshold_exponent must be positive number");
        assert!(threshold_exponent < 32, "backoff: threshold_exponent must be less than 32 to avoid shift overflow");

        let threshold = 1 << threshold_exponent;
        assert!(initial < threshold, "backoff: initial value must be less than the calculated threshold");

        Self {
            initial,
            threshold,
            current: initial,
        }
    }

    pub(crate) fn spin(&mut self) {
        for _ in 0..self.current {
            std::hint::spin_loop();
        }
        if self.current < self.threshold {
            self.current <<= 1;
        }
    }

    pub(crate) fn reset(&mut self) {
        self.current = self.initial;
    }

}
