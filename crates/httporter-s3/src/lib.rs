// SPDX-License-Identifier: MIT OR Apache-2.0

//! The S3-compatible storage provider for HTTPorter: request signing (AWS Signature Version 4),
//! service presets and endpoint addressing, and [`S3Storage`], which speaks the S3 REST API.
//! Signing and addressing are pure (no network, no clock); the provider makes one attempt per
//! call, because whoever calls it owns the retries.

mod body;
mod errors;
pub mod multipart;
pub mod presets;
pub mod sigv4;
pub mod storage;
mod xml;

pub use errors::ACCESS_DENIED;
pub use multipart::{MULTIPART_THRESHOLD, PART_SIZE, part_size_for};
pub use presets::{
    Endpoint, REGIONS_AWS, REGIONS_B2, REGIONS_SPACES, REGIONS_WASABI, bucket_url, endpoint,
    object_url,
};
pub use sigv4::{
    Credentials, EMPTY_PAYLOAD_SHA256, PRESIGN_MAX_SECS, SignInput, UNSIGNED_PAYLOAD,
    canonical_uri, presign_get, sha256_hex, sign,
};
pub use storage::{S3Config, S3Storage};
/// Re-exported so callers of [`sign`] and [`endpoint`] need no `url` dependency of their own.
pub use url::Url;
