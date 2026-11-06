// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bucket tools (§8.8.4–8.8.5): create a bucket, and read and change its CORS rules and its
//! static-website settings, where the service offers them ([`crate::presets::PresetFeatures`]).
//! The XML goes through the crate's own writer and reader (`xml.rs`). T5 fills these in.

use httporter_core::ProviderError;

use crate::S3Storage;

/// One CORS rule of a bucket.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorsRule {
    /// Allowed origins (`https://example.com`, or `*`).
    pub origins: Vec<String>,
    /// Allowed methods (`GET`, `HEAD`, `PUT`, …).
    pub methods: Vec<String>,
    /// Allowed request headers.
    pub headers: Vec<String>,
    /// How long a browser may cache the preflight answer, in seconds.
    pub max_age: Option<u32>,
}

/// A bucket's static-website settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Website {
    /// The index document (`index.html`).
    pub index: String,
    /// The error document, if any (`404.html`).
    pub error: Option<String>,
}

impl S3Storage {
    /// `CreateBucket` for `name`, with a `LocationConstraint` unless the region is `us-east-1`
    /// or `auto`.
    pub async fn create_bucket(&self, _name: &str) -> Result<(), ProviderError> {
        todo!("T5")
    }

    /// The bucket's CORS rules; none when it has no CORS configuration
    /// (`NoSuchCORSConfiguration`).
    pub async fn cors(&self) -> Result<Vec<CorsRule>, ProviderError> {
        todo!("T5")
    }

    /// Replaces the bucket's CORS rules; an empty list deletes them (`DeleteBucketCors`).
    pub async fn set_cors(&self, _rules: &[CorsRule]) -> Result<(), ProviderError> {
        todo!("T5")
    }

    /// The bucket's static-website settings; `None` when it isn't a website.
    pub async fn website(&self) -> Result<Option<Website>, ProviderError> {
        todo!("T5")
    }

    /// Sets the static-website settings; `None` deletes them (`DeleteBucketWebsite`).
    pub async fn set_website(&self, _website: Option<&Website>) -> Result<(), ProviderError> {
        todo!("T5")
    }
}
