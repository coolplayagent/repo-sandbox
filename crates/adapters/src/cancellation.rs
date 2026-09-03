use crate::buildkit::Cancellation;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCancellation;

impl Cancellation for ProcessCancellation {
    fn is_cancelled(&self) -> bool {
        is_cancelled()
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
