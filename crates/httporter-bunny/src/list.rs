//! Directory listings: at most five at once, each try with its own time limit
//! ([`crate::BunnyConfig::list_timeout`]) and retries ([`crate::BunnyConfig::list_retry`]).

use httporter_core::{Entry, Listing, ProviderError, RemotePath, RequestInfo, with_retry};
use reqwest::Version;
use reqwest::header::ACCEPT;
use tokio::time::Instant;

use crate::client::{
    BunnyStorage, ERROR_BODY_MAX, Op, map_status, network_error, parse_retry_after, read_capped,
};
use crate::model::{BunnyObject, to_entry};

impl BunnyStorage {
    /// The direct children of `dir`.
    pub(crate) async fn list_dir(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        let url = self.dir_url(dir);
        let url = url.as_str();
        let (objects, info) =
            with_retry(&self.list_retry, move |_| self.fetch_listing(url)).await?;
        Ok(Listing {
            entries: to_entries(objects, dir)?,
            info,
        })
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
