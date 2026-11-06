// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bucket tools (§8.8.4–8.8.5): create a bucket, and read and change its CORS rules and its
//! static-website settings, where the service offers them ([`crate::presets::PresetFeatures`]).
//! The XML goes through the crate's own writer and reader (`xml.rs`); every request is signed
//! like the crate's others (`S3Storage::call`), one attempt per call.
//!
//! The requests, from the S3 API reference:
//! - `CreateBucket`: `PUT /` of the bucket, with a `CreateBucketConfiguration` naming the
//!   `LocationConstraint` unless the region is `us-east-1` (where S3 refuses one) or R2's
//!   `auto`. <https://docs.aws.amazon.com/AmazonS3/latest/API/API_CreateBucket.html>
//! - `GetBucketCors` / `PutBucketCors` / `DeleteBucketCors`: `GET` / `PUT` / `DELETE /?cors`;
//!   the `PUT` carries `Content-MD5`, which S3 requires. A bucket without rules answers
//!   `404 NoSuchCORSConfiguration`. <https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutBucketCors.html>
//! - `GetBucketWebsite` / `PutBucketWebsite` / `DeleteBucketWebsite`: the same on `/?website`;
//!   `404 NoSuchWebsiteConfiguration` when the bucket is no website.
//!   <https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutBucketWebsite.html>
//!
//! A service that doesn't implement one of these (`501 NotImplemented`) answers
//! [`ProviderError::Refused`] with [`NOT_OFFERED`], never a server error worth retrying.

use httporter_core::{ProviderError, S3Preset};
use md5::{Digest, Md5};
use reqwest::Method;

use crate::S3Storage;
use crate::body::{read_capped, read_limited};
use crate::errors::{self, ERROR_BODY_MAX};
use crate::presets::bucket_url;
use crate::sigv4::{EMPTY_PAYLOAD_SHA256, sha256_hex};
use crate::storage::{ANSWER_MAX, b64};
use crate::xml;

/// What [`ProviderError::Refused`] says when the service does not offer the request through its
/// S3 API (`NotImplemented`). The app shows it in its own words.
pub const NOT_OFFERED: &str = "the service doesn't offer this through its S3 API";
/// What [`ProviderError::Refused`] says when `CreateBucket` finds the name taken by someone else
/// (`BucketAlreadyExists`): bucket names are unique across a service.
pub const BUCKET_NAME_TAKEN: &str = "that bucket name is taken";
/// What [`ProviderError::Refused`] says when the service refuses the name itself
/// (`InvalidBucketName`).
pub const BUCKET_NAME_INVALID: &str = "the service doesn't accept that bucket name";

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
    /// Response headers a page may read (`ExposeHeader`). Kept so that rewriting the rules
    /// (Settings › Sites changes the first one) loses nothing of the others.
    pub expose_headers: Vec<String>,
    /// The rule's `ID`, if it has one; kept for the same reason.
    pub id: Option<String>,
}

/// A bucket's static-website settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Website {
    /// The index document (`index.html`). Empty for a bucket that only redirects
    /// (`RedirectAllRequestsTo`), which this does not edit.
    pub index: String,
    /// The error document, if any (`404.html`).
    pub error: Option<String>,
}

/// The methods a CORS rule may allow (S3 takes no others).
pub const CORS_METHODS: [&str; 5] = ["GET", "PUT", "POST", "DELETE", "HEAD"];

/// What is wrong with a bucket name, before it is sent ([`bucket_name_problem`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketNameProblem {
    /// Not between `min` and `max` characters.
    Length { min: usize, max: usize },
    /// A character the service doesn't take: lowercase letters, digits and hyphens, and dots
    /// where `dots` says so.
    Characters { dots: bool },
    /// It doesn't start and end with a letter or a digit.
    Edge,
    /// Two dots in a row, or the shape of an IP address.
    Dots,
    /// A prefix or suffix the service keeps for itself (`xn--`, `b2-`, `-s3alias`, …).
    Reserved,
}

/// What is wrong with `name` as a new bucket of `preset`, or `None`. The rules, per service:
/// - Amazon S3, and the services that follow its rules (Wasabi, MinIO, others): 3–63 characters
///   of lowercase letters, digits, dots and hyphens, starting and ending with a letter or digit,
///   no `..`, not an IP address, not `xn--…`, `sthree-…`, `…-s3alias`, `…--ol-s3`.
///   <https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html>
/// - Cloudflare R2: 3–63 lowercase letters, digits and hyphens, not starting or ending with a
///   hyphen. <https://developers.cloudflare.com/r2/buckets/create-buckets/>
/// - Backblaze B2: 6–63 characters; through the S3 API also Amazon's rules. Backblaze advises
///   lowercase letters, digits and hyphens only (no dots), which is what is allowed here, and
///   keeps `b2-…` to itself. <https://www.backblaze.com/docs/cloud-storage-buckets>
/// - DigitalOcean Spaces: 3–63 lowercase letters, digits and dashes, starting with a letter or
///   digit. <https://docs.digitalocean.com/products/spaces/how-to/create/>
pub fn bucket_name_problem(preset: S3Preset, name: &str) -> Option<BucketNameProblem> {
    let (min, max, dots) = match preset {
        // Spaces asks only that a name start with a letter or digit; ending with one too keeps
        // it a valid host name label, which is how Spaces addresses buckets.
        S3Preset::CloudflareR2 | S3Preset::DigitalOceanSpaces => (3, 63, false),
        S3Preset::BackblazeB2 => (6, 63, false),
        S3Preset::AwsS3 | S3Preset::Wasabi | S3Preset::Minio | S3Preset::Other => (3, 63, true),
    };
    let len = name.chars().count();
    if len < min || len > max {
        return Some(BucketNameProblem::Length { min, max });
    }
    let allowed =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || (dots && c == '.');
    if !name.chars().all(allowed) {
        return Some(BucketNameProblem::Characters { dots });
    }
    let alnum = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !alnum(name.chars().next()) || !alnum(name.chars().last()) {
        return Some(BucketNameProblem::Edge);
    }
    if name.contains("..") || looks_like_ip(name) {
        return Some(BucketNameProblem::Dots);
    }
    let reserved = name.starts_with("xn--")
        || name.starts_with("sthree-")
        || name.ends_with("-s3alias")
        || name.ends_with("--ol-s3")
        || (preset == S3Preset::BackblazeB2 && name.starts_with("b2-"));
    reserved.then_some(BucketNameProblem::Reserved)
}

/// `192.168.5.4`: four dot-separated numbers.
fn looks_like_ip(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The region a `CreateBucket` request names, or `None` when it names none: `us-east-1` (S3
/// refuses it there), R2's `auto`, or no region at all.
pub fn location_constraint(region: &str) -> Option<&str> {
    let region = region.trim();
    match region {
        "" | "us-east-1" | "auto" => None,
        other => Some(other),
    }
}

/// How a failed answer to a bucket request ends.
enum Failed {
    /// The service says the setting isn't there (`NoSuchCORSConfiguration`, …).
    Missing,
    /// Anything else.
    Error(ProviderError),
}

/// Reads the failed `response` and decides: a code in `missing` is [`Failed::Missing`];
/// `NotImplemented` (or a 501), `BucketAlreadyExists` and `InvalidBucketName` are
/// [`ProviderError::Refused`]; the rest is what [`errors::from_parts`] makes of it.
async fn failed(
    response: reqwest::Response,
    limit: std::time::Duration,
    missing: &[&str],
) -> Failed {
    let status = response.status().as_u16();
    let retry_after = errors::retry_after(response.headers());
    let body = tokio::time::timeout(limit, read_capped(response, ERROR_BODY_MAX))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let doc = xml::parse_error(&body);
    let code = doc.code.as_deref();
    if code.is_some_and(|code| missing.contains(&code)) {
        return Failed::Missing;
    }
    let refused = |text: &str| Failed::Error(ProviderError::Refused(text.to_owned()));
    match code {
        Some("NotImplemented") => refused(NOT_OFFERED),
        _ if status == 501 => refused(NOT_OFFERED),
        Some("BucketAlreadyExists") => refused(BUCKET_NAME_TAKEN),
        Some("InvalidBucketName") => refused(BUCKET_NAME_INVALID),
        _ => Failed::Error(errors::from_parts(
            status,
            code,
            doc.message.as_deref(),
            retry_after,
        )),
    }
}

impl S3Storage {
    /// `CreateBucket` for `name`, with a `LocationConstraint` unless the region is `us-east-1`
    /// or `auto` ([`location_constraint`]). A name someone else has is
    /// [`ProviderError::Refused`] with [`BUCKET_NAME_TAKEN`]; one these credentials already own
    /// (`BucketAlreadyOwnedByYou`) is `Ok`: the bucket is there, and theirs. The name is sent as
    /// given: check it first with [`bucket_name_problem`].
    pub async fn create_bucket(&self, name: &str) -> Result<(), ProviderError> {
        if name.is_empty() {
            return Err(ProviderError::Permanent("no bucket name was given".into()));
        }
        let url = bucket_url(&self.endpoint, name);
        let (extra, payload, body) = match location_constraint(&self.endpoint.region) {
            Some(region) => {
                let body = xml::create_bucket_request(region).into_bytes();
                let extra = vec![("content-type".to_owned(), "application/xml".to_owned())];
                (extra, sha256_hex(&body), body)
            }
            None => (Vec::new(), EMPTY_PAYLOAD_SHA256.to_owned(), Vec::new()),
        };
        let response = self
            .call(Method::PUT, url, &extra, &payload, Some(body), None)
            .await?;
        if response.status().is_success() {
            let _ = read_capped(response, ERROR_BODY_MAX).await;
            return Ok(());
        }
        match failed(response, self.request_timeout, &["BucketAlreadyOwnedByYou"]).await {
            Failed::Missing => Ok(()),
            Failed::Error(error) => Err(error),
        }
    }

    /// The bucket's CORS rules; none when it has no CORS configuration
    /// (`NoSuchCORSConfiguration`).
    pub async fn cors(&self) -> Result<Vec<CorsRule>, ProviderError> {
        match self.get_setting("cors", "NoSuchCORSConfiguration").await? {
            Some((body, status)) => xml::parse_cors(&body, status),
            None => Ok(Vec::new()),
        }
    }

    /// Replaces the bucket's CORS rules; an empty list deletes them (`DeleteBucketCors`).
    pub async fn set_cors(&self, rules: &[CorsRule]) -> Result<(), ProviderError> {
        if rules.is_empty() {
            return self.delete_setting("cors", "NoSuchCORSConfiguration").await;
        }
        self.put_setting("cors", xml::cors_request(rules)).await
    }

    /// The bucket's static-website settings; `None` when it isn't a website.
    pub async fn website(&self) -> Result<Option<Website>, ProviderError> {
        match self
            .get_setting("website", "NoSuchWebsiteConfiguration")
            .await?
        {
            Some((body, status)) => xml::parse_website(&body, status).map(Some),
            None => Ok(None),
        }
    }

    /// Sets the static-website settings; `None` deletes them (`DeleteBucketWebsite`). An index
    /// document must be given (S3 refuses an empty one).
    pub async fn set_website(&self, website: Option<&Website>) -> Result<(), ProviderError> {
        match website {
            None => {
                self.delete_setting("website", "NoSuchWebsiteConfiguration")
                    .await
            }
            Some(website) if website.index.trim().is_empty() => Err(ProviderError::Permanent(
                "a website needs an index document".into(),
            )),
            Some(website) => {
                self.put_setting("website", xml::website_request(website))
                    .await
            }
        }
    }

    /// `GET /?{what}` of the bucket: the answer's body and status, or `None` when the service
    /// answers with the `missing` code.
    async fn get_setting(
        &self,
        what: &str,
        missing: &str,
    ) -> Result<Option<(Vec<u8>, u16)>, ProviderError> {
        let mut url = bucket_url(&self.endpoint, self.bucket()?);
        url.set_query(Some(what));
        let response = self
            .call(Method::GET, url, &[], EMPTY_PAYLOAD_SHA256, None, None)
            .await?;
        let status = response.status();
        if status.is_success() {
            let body = read_limited(response, ANSWER_MAX).await?;
            return Ok(Some((body, status.as_u16())));
        }
        match failed(response, self.request_timeout, &[missing]).await {
            Failed::Missing => Ok(None),
            Failed::Error(error) => Err(error),
        }
    }

    /// `PUT /?{what}` of the bucket with the document `xml`, and its `Content-MD5`.
    async fn put_setting(&self, what: &str, xml: String) -> Result<(), ProviderError> {
        let body = xml.into_bytes();
        let extra = vec![
            ("content-md5".to_owned(), b64(&Md5::digest(&body))),
            ("content-type".to_owned(), "application/xml".to_owned()),
        ];
        let payload = sha256_hex(&body);
        let mut url = bucket_url(&self.endpoint, self.bucket()?);
        url.set_query(Some(what));
        let response = self
            .call(Method::PUT, url, &extra, &payload, Some(body), None)
            .await?;
        if response.status().is_success() {
            let _ = read_capped(response, ERROR_BODY_MAX).await;
            return Ok(());
        }
        match failed(response, self.request_timeout, &[]).await {
            Failed::Missing => Ok(()),
            Failed::Error(error) => Err(error),
        }
    }

    /// `DELETE /?{what}` of the bucket; a setting that is not there (`missing`) is deleted
    /// already.
    async fn delete_setting(&self, what: &str, missing: &str) -> Result<(), ProviderError> {
        let mut url = bucket_url(&self.endpoint, self.bucket()?);
        url.set_query(Some(what));
        let response = self
            .call(Method::DELETE, url, &[], EMPTY_PAYLOAD_SHA256, None, None)
            .await?;
        if response.status().is_success() {
            let _ = read_capped(response, ERROR_BODY_MAX).await;
            return Ok(());
        }
        match failed(response, self.request_timeout, &[missing]).await {
            Failed::Missing => Ok(()),
            Failed::Error(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_constraints_are_left_out_where_s3_wants_none() {
        for none in ["", " ", "us-east-1", "auto"] {
            assert_eq!(location_constraint(none), None, "{none:?}");
        }
        assert_eq!(location_constraint("eu-west-1"), Some("eu-west-1"));
        assert_eq!(location_constraint(" sgp1 "), Some("sgp1"));
    }

    #[test]
    fn bucket_names_follow_each_services_rules() {
        use BucketNameProblem::*;
        use S3Preset::*;
        for good in ["abc", "my-site", "a1.b2", "x".repeat(63).as_str()] {
            assert_eq!(bucket_name_problem(AwsS3, good), None, "{good}");
        }
        assert_eq!(
            bucket_name_problem(AwsS3, "ab"),
            Some(Length { min: 3, max: 63 })
        );
        assert_eq!(
            bucket_name_problem(AwsS3, &"x".repeat(64)),
            Some(Length { min: 3, max: 63 })
        );
        assert_eq!(
            bucket_name_problem(AwsS3, "My-Site"),
            Some(Characters { dots: true })
        );
        assert_eq!(
            bucket_name_problem(AwsS3, "my_site"),
            Some(Characters { dots: true })
        );
        assert_eq!(bucket_name_problem(AwsS3, "-site"), Some(Edge));
        assert_eq!(bucket_name_problem(AwsS3, "site."), Some(Edge));
        assert_eq!(bucket_name_problem(AwsS3, "a..b"), Some(Dots));
        assert_eq!(bucket_name_problem(AwsS3, "192.168.5.4"), Some(Dots));
        assert_eq!(bucket_name_problem(AwsS3, "xn--site"), Some(Reserved));
        assert_eq!(bucket_name_problem(AwsS3, "site-s3alias"), Some(Reserved));
        // R2 and Spaces take no dots; B2 wants six characters and keeps `b2-` to itself.
        for preset in [CloudflareR2, DigitalOceanSpaces] {
            assert_eq!(
                bucket_name_problem(preset, "a.b"),
                Some(Characters { dots: false })
            );
            assert_eq!(bucket_name_problem(preset, "my-site"), None);
        }
        assert_eq!(
            bucket_name_problem(BackblazeB2, "abcde"),
            Some(Length { min: 6, max: 63 })
        );
        assert_eq!(bucket_name_problem(BackblazeB2, "b2-site"), Some(Reserved));
        assert_eq!(bucket_name_problem(BackblazeB2, "my-site"), None);
    }
}
