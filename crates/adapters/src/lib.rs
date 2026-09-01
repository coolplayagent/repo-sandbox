//! Infrastructure adapters for repo-sandbox.

pub mod doctor;
pub mod snapshot;

pub mod logging {
    use tracing_subscriber::EnvFilter;

    /// Initialize human-readable logging, respecting `RUST_LOG` when present.
    pub fn init() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

        // Tests or embedders may already have installed a subscriber.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}
