use crate::buildkit::Cancellation;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCancellation;

impl Cancellation for ProcessCancellation {
    fn is_cancelled(&self) -> bool {
        is_cancelled()
    }
}

#[derive(Debug)]
pub struct DeadlineCancellation {
    deadline: Instant,
}

impl DeadlineCancellation {
    pub fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }

    pub const fn at(deadline: Instant) -> Self {
        Self { deadline }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl Cancellation for DeadlineCancellation {
    fn is_cancelled(&self) -> bool {
        is_cancelled() || self.expired()
    }
}

pub fn install() -> Result<(), String> {
    INSTALLED
        .get_or_init(|| {
            ctrlc::set_handler(|| {
                CANCELLED.store(true, Ordering::SeqCst);
            })
            .map_err(|error| error.to_string())
        })
        .clone()
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

#[cfg(test)]
pub fn reset() {
    CANCELLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_cancellation_expires_without_global_signal_state() {
        let cancellation = DeadlineCancellation::new(Duration::ZERO);
        assert!(cancellation.expired());
        assert!(Cancellation::is_cancelled(&cancellation));
        assert_eq!(cancellation.remaining(), Duration::ZERO);
    }
}
