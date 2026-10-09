//! Clock and sleep for both targets: tokio natively, the browser's timers on wasm32, where
//! `std::time::Instant` and `SystemTime::now` panic and tokio has no timer driver.

use std::time::Duration;

pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

pub async fn sleep(duration: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(duration).await;
    #[cfg(target_arch = "wasm32")]
    futures_timer::Delay::new(duration).await;
}
