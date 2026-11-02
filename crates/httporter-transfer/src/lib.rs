//! The transfer engine: runs uploads, downloads, deletes and new folders on Bunny Storage and
//! S3 sites, with per-site lanes, adaptive concurrency, retries, a global bandwidth limit and a
//! journal that survives a crash.

mod bucket;
mod engine;
mod hash;
mod lane;
mod limiter;
mod rate;

pub use bucket::TokenBucket;
pub use engine::{Batch, Engine, WINDOW};
pub use hash::{FileHashes, hash_file};
pub use limiter::AdaptiveLimiter;
pub use rate::{RATE_WINDOW, RateWindow, eta};
