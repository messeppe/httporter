//! S3 request signing (AWS Signature Version 4), service presets and endpoint
//! addressing for HTTPorter. Everything here is pure: no network, no clock.

pub mod presets;
pub mod sigv4;

pub use presets::{
    Endpoint, REGIONS_AWS, REGIONS_B2, REGIONS_SPACES, REGIONS_WASABI, bucket_url, endpoint,
    object_url,
};
pub use sigv4::{
    Credentials, EMPTY_PAYLOAD_SHA256, SignInput, UNSIGNED_PAYLOAD, canonical_uri, sha256_hex, sign,
};
/// Re-exported so callers of [`sign`] and [`endpoint`] need no `url` dependency of their own.
pub use url::Url;
