//! The transfer engine: runs uploads, downloads, deletes and new folders on Bunny Storage and
//! S3 sites, with per-site lanes, adaptive concurrency, retries, a global bandwidth limit and a
//! journal that survives a crash. And the planner that turns what the user picked into jobs,
//! with the conflicts to resolve first.

mod bucket;
mod compare;
mod engine;
mod hash;
mod lane;
mod limiter;
mod local;
pub mod messages;
mod names;
mod offline;
mod plan;
mod publish;
mod rate;

pub use bucket::TokenBucket;
pub use compare::{
    CompareCounts, CompareError, CompareItem, CompareMode, CompareResult, HashCache, ScanProgress,
    compare_trees,
};
pub use engine::{
    Batch, BatchOutcome, CHECK_AFTER, Engine, MAX_CHECKS, SiteInfo, SiteLookup, Unfinished, WINDOW,
};
pub use hash::{FileHashes, hash_file};
pub use limiter::AdaptiveLimiter;
pub use local::{DiskFiles, LocalFiles};
pub use names::{keep_both_name, safe_local_name};
pub use plan::{
    DeletePlan, DownloadPlan, UploadPlan, plan_delete, plan_download, plan_upload,
    policy_to_resolution, reason, resolve_downloads, resolve_uploads,
};
pub use publish::{BatchTarget, PlanItem, PublishOutcome, PublishPhase, PublishPlan, run_publish};
pub use rate::{RATE_WINDOW, RateWindow, eta};
