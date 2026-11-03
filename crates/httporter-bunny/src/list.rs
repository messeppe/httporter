//! Directory listings: at most five at once, each try with its own time limit
//! ([`crate::BunnyConfig::list_timeout`]) and retries ([`crate::BunnyConfig::list_retry`]).
//!
//! What the live probe (Task 7) showed about them:
//! - a listing is one JSON array with every child, however many: a folder of 1,050 files came
//!   back whole (about 530 KB). There are no pages (`page=` belongs to the account API);
//! - an empty folder is `200 []`, and so is a folder that is not there, a file listed as a
//!   folder, or a folder under a missing parent. Bunny never answers 404 to a listing, so an
//!   empty answer is checked against the parent (see [`BunnyStorage::list_dir`]).

use httporter_core::{Entry, Listing, ProviderError, RemotePath, RequestInfo, with_retry};
use reqwest::Version;
use reqwest::header::ACCEPT;
use tokio::time::Instant;

use crate::client::{
    BunnyStorage, ERROR_BODY_MAX, Op, map_status, network_error, parse_retry_after, read_capped,
};
use crate::model::{BunnyObject, to_entry};

impl BunnyStorage {
    /// The direct children of `dir`, or [`ProviderError::NotFound`] when there is no such
    /// folder.
    ///
    /// Bunny answers `[]` both for an empty folder and for one that is not there, so an empty
    /// answer for anything but the root costs one more listing: the parent's, which shows the
    /// folder only if it exists. Empty folders are rare, so this is cheap where it matters.
    pub(crate) async fn list_dir(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        let url = self.dir_url(dir);
        let url = url.as_str();
        let (objects, info) =
            with_retry(&self.list_retry, move |_| self.fetch_listing(url)).await?;
        if objects.is_empty() && !self.has_folder(dir).await? {
            return Err(ProviderError::NotFound);
        }
        Ok(Listing {
            entries: to_entries(objects, dir)?,
            info,
        })
    }

    /// Whether the parent of `dir` lists a folder of exactly that name (names are
    /// case-sensitive on Bunny, and a file of the same name is not the folder). The root is
    /// always there.
    pub(crate) async fn has_folder(&self, dir: &RemotePath) -> Result<bool, ProviderError> {
        let (Some(parent), Some(name)) = (dir.parent(), dir.name()) else {
            return Ok(true);
        };
        let url = self.dir_url(&parent);
        let url = url.as_str();
        let (siblings, _) = with_retry(&self.list_retry, move |_| self.fetch_listing(url)).await?;
        Ok(siblings
            .iter()
            .any(|object| object.is_directory && object.object_name == name))
    }

    /// One attempt at fetching and parsing a directory listing.
    async fn fetch_listing(
        &self,
        url: &str,
    ) -> Result<(Vec<BunnyObject>, RequestInfo), ProviderError> {
        // Held until the body is read, so at most five listings are in flight. Acquired per
        // attempt, so a retry's backoff does not hold a slot.
        let _slot = self
            .list_slots
            .acquire()
            .await
            .map_err(|_| ProviderError::Permanent("the listing limiter was closed".into()))?;

        // Queueing for a slot is not request latency.
        let started = Instant::now();
        let mut response = self
            .with_key(self.client.get(url))
            .header(ACCEPT, "application/json")
            .timeout(self.list_timeout)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            // Read the answer, so the connection can carry the next request (ledger 143).
            let _ = read_capped(&mut response, ERROR_BODY_MAX).await;
            return Err(map_status(status, retry_after, Op::List));
        }
        let http_version = http_version(response.version());
        let body = response.bytes().await.map_err(network_error)?;
        let latency = started.elapsed();

        Ok((
            parse_listing(&body)?,
            RequestInfo {
                latency,
                http_version: http_version.to_owned(),
            },
        ))
    }
}

fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

fn parse_listing(body: &[u8]) -> Result<Vec<BunnyObject>, ProviderError> {
    serde_json::from_slice(body).map_err(|e| {
        if e.is_data() {
            // Valid JSON, but not a list of objects.
            ProviderError::Permanent(format!(
                "unexpected response from server (unrecognised listing: {e})"
            ))
        } else {
            ProviderError::Permanent("unexpected response from server (not JSON)".into())
        }
    })
}

/// One malformed object fails the whole listing, and says which one.
fn to_entries(objects: Vec<BunnyObject>, dir: &RemotePath) -> Result<Vec<Entry>, ProviderError> {
    objects
        .into_iter()
        .map(|object| {
            let name = object.object_name.clone();
            to_entry(object, dir).map_err(|error| {
                let reason = match error {
                    ProviderError::Permanent(message) => message,
                    other => other.to_string(),
                };
                ProviderError::Permanent(format!(
                    "unexpected entry \"{name}\" in listing: {reason}"
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_versions_are_named_for_the_status_bar() {
        assert_eq!(http_version(Version::HTTP_2), "HTTP/2");
        assert_eq!(http_version(Version::HTTP_3), "HTTP/3");
        assert_eq!(http_version(Version::HTTP_10), "HTTP/1.0");
        assert_eq!(http_version(Version::HTTP_11), "HTTP/1.1");
        assert_eq!(http_version(Version::HTTP_09), "HTTP/1.1");
    }

    #[test]
    fn an_empty_listing_is_no_entries() {
        assert!(parse_listing(b"[]").unwrap().is_empty());
        assert!(
            to_entries(Vec::new(), &RemotePath::root())
                .unwrap()
                .is_empty()
        );
    }
}
