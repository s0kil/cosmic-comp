use std::time::Duration;

/// A transient error gets one immediate retry; persistent failures must yield.
#[derive(Default)]
pub(super) struct RenderRetry {
    failures: u32,
}

impl RenderRetry {
    pub fn failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn minimum_delay(&self) -> Duration {
        if self.failures < 2 {
            Duration::ZERO
        } else {
            Duration::from_millis((1u64 << (self.failures - 2).min(6)).min(50))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_rendering_and_first_retry_have_no_delay() {
        let mut retry = RenderRetry::default();
        assert_eq!(retry.minimum_delay(), Duration::ZERO);
        retry.failed();
        assert_eq!(retry.minimum_delay(), Duration::ZERO);
    }

    #[test]
    fn persistent_failures_are_bounded_and_success_rearms_immediate_retry() {
        let mut retry = RenderRetry::default();
        for milliseconds in [0, 1, 2, 4, 8, 16, 32, 50, 50] {
            retry.failed();
            assert_eq!(retry.minimum_delay(), Duration::from_millis(milliseconds));
        }
        retry.failures = u32::MAX;
        retry.failed();
        assert_eq!(retry.minimum_delay(), Duration::from_millis(50));
        retry.reset();
        assert_eq!(retry.minimum_delay(), Duration::ZERO);
        retry.failed();
        assert_eq!(retry.minimum_delay(), Duration::ZERO);
    }
}
