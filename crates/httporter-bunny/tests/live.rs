//! Live probes of Bunny Storage (plan Task 7). Every test here is `#[ignore]`d: it talks to the
//! real service with the credentials of a test zone, and runs only when asked for:
//!
//! ```text
//! cargo test --locked -p httporter-bunny --test live -- --ignored --nocapture
//! ```
//!
//! **Credentials.** They come from the git-ignored `.env.test` in the repository root (found by
//! walking up from this crate) and are read by this process alone: `HTTPORTER_TEST_ZONE` (which
//! must start with `httporter-test`), `HTTPORTER_TEST_REGION` and `HTTPORTER_TEST_PASSWORD`. The
//! file is read without touching the environment, and a line it cannot parse is never quoted.
//! Before its first request each test checks the zone name, and panics for any other zone.
//!
//! **Output.** Each test prints what it saw (statuses, the service's own words, the shape of its
//! answers) through [`Live::observe`], never the password: every printed text first goes through
//! [`Live::scrub`], and nothing prints a request, a header value or a configuration.
//!
//! **Cleanup.** Everything a test makes lives in its own `probe-<time>-<name>/` folder, which is
//! deleted at the end, also when the test fails; the zone root is counted before and after. The
//! tests run one at a time, since the busy probe would disturb the others.
//!
//! Requests the client hides (a `PUT` to a folder URL, the raw listing JSON, a `Checksum` the
//! client would never send) go out on a plain reqwest client; everything else uses
//! [`BunnyStorage`]. What was seen is written up in `docs/notes/bunny-behavior.md`.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::future::join_all;
use futures_util::{FutureExt, StreamExt, TryStreamExt};
use httporter_bunny::{BunnyConfig, BunnyStorage, storage_host};
use httporter_core::{
    Listing, ProviderError, RemotePath, RetryPolicy, StorageProvider, UploadOptions, UploadSource,
    encode_segment,
};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// The only zones these tests may touch.
const ZONE_PREFIX: &str = "httporter-test";
/// The production zone, named so that no mistake in the prefix check can reach it.
const PRODUCTION_ZONE: &str = "exampleshop";

/// One probe at a time: a burst of requests from one would look like load to the others.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

// ---------------------------------------------------------------------------------------------
// Setup and cleanup
// ---------------------------------------------------------------------------------------------

/// The `HTTPORTER_TEST_*` values of the nearest `.env.test` above this crate. The file is parsed
/// here, not loaded into the environment, and its errors are reported without the line they
/// are about (which may hold a secret).
fn test_env() -> HashMap<String, String> {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = loop {
        let candidate = dir.join(".env.test");
        if candidate.is_file() {
            break candidate;
        }
        dir = dir
            .parent()
            .unwrap_or_else(|| panic!("no .env.test in any folder above this crate"));
    };
    let entries =
        dotenvy::from_path_iter(&file).unwrap_or_else(|_| panic!(".env.test could not be opened"));
    let mut values = HashMap::new();
    for entry in entries {
        match entry {
            Ok((key, value)) if key.starts_with("HTTPORTER_TEST_") => {
                values.insert(key, value);
            }
            Ok(_) => {}
            Err(_) => panic!("an entry of .env.test could not be read"),
        }
    }
    values
}

fn required(values: &HashMap<String, String>, name: &str) -> String {
    match values.get(name).map(|v| v.trim()) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => panic!("{name} is not set in .env.test"),
    }
}

/// Panics unless `zone` is a test zone. Called before any request.
fn assert_test_zone(zone: &str) {
    assert!(
        zone.starts_with(ZONE_PREFIX),
        "refusing to touch zone {zone:?}: live tests only use zones starting with {ZONE_PREFIX:?}"
    );
    assert_ne!(
        zone, PRODUCTION_ZONE,
        "refusing to touch the production zone"
    );
}

/// A test's view of the zone: the client under test, a patient one for setup and cleanup, and a
/// plain HTTP client for what the client hides.
struct Live {
    zone: String,
    /// `https://{host}`.
    base: String,
    /// The zone password as a header value, marked sensitive.
    access_key: HeaderValue,
    /// Only for [`Self::scrub`].
    password: String,
    http: reqwest::Client,
    /// One attempt per call, as a site uses it: what it answers is what the service said.
    storage: BunnyStorage,
    /// With the default retries, for making and removing what a test needs.
    patient: BunnyStorage,
    /// This test's own folder, `probe-<time>-<name>`; nothing outside it is written.
    folder: RemotePath,
    /// Entries in the zone root when the test began.
    root_before: usize,
}

impl Live {
    async fn start(name: &str) -> Self {
        let values = test_env();
        let zone = required(&values, "HTTPORTER_TEST_ZONE");
        assert_test_zone(&zone);
        let region = required(&values, "HTTPORTER_TEST_REGION").to_ascii_lowercase();
        let password = required(&values, "HTTPORTER_TEST_PASSWORD");
        let host = storage_host(&region)
            .unwrap_or_else(|| panic!("HTTPORTER_TEST_REGION {region:?} is not a Bunny region"));

        let mut access_key = HeaderValue::from_str(&password)
            .unwrap_or_else(|_| panic!("the zone password cannot be sent as a header"));
        access_key.set_sensitive(true);
        // This client comes before the provider's, which installs ring's TLS crypto too.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Long: a recursive folder DELETE answers only when the whole subtree is gone.
            .timeout(Duration::from_secs(600))
            .user_agent("HTTPorter-live-probe")
            .build()
            .unwrap_or_else(|_| panic!("could not build the HTTP client"));

        let mut single = BunnyConfig::new(&zone, &host, &password);
        single.retry = RetryPolicy::immediate(0);
        let storage = BunnyStorage::new(single)
            .unwrap_or_else(|_| panic!("could not build the storage client"));
        let patient = BunnyStorage::new(BunnyConfig::new(&zone, &host, &password))
            .unwrap_or_else(|_| panic!("could not build the storage client"));

        let stamp = Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let folder = RemotePath::parse(&format!("probe-{stamp}-{name}"))
            .unwrap_or_else(|_| panic!("bad probe folder name"));

        let mut live = Self {
            zone,
            base: format!("https://{host}"),
            access_key,
            password,
            http,
            storage,
            patient,
            folder,
            root_before: 0,
        };
        // The first request: if the zone cannot be reached, every probe stops here.
        let root = match live.storage.list(&RemotePath::root()).await {
            Ok(listing) => listing,
            Err(error) => panic!("Bunny zone not reachable: {}", live.kind(&error)),
        };
        live.root_before = root.entries.len();
        live.observe(
            "zone",
            format!(
                "{} on {host}: root has {} entries; listing over {}",
                live.zone,
                root.entries.len(),
                root.info.http_version
            ),
        );
        live
    }

    /// Deletes the test's folder and checks that the zone root is as it was.
    ///
    /// The `DELETE` goes out raw so that its answer can be shown: once, a folder delete left the
    /// folder in place (see the notes). It is tried up to three times, two seconds apart.
    async fn clean_up(&self) {
        let verbose = std::env::var_os("HTTPORTER_PROBE_VERBOSE").is_some();
        let url = self.url(&self.folder, true);
        let mut gone = String::new();
        for attempt in 1..=3 {
            let answer = self.raw(Method::DELETE, &url, None, &[]).await;
            let after = self.storage.list(&self.folder).await;
            let empty = match &after {
                Err(ProviderError::NotFound) => true,
                Ok(listing) => listing.entries.is_empty(),
                Err(_) => false,
            };
            if verbose || !empty || attempt > 1 {
                self.observe(
                    &format!("cleanup DELETE #{attempt}"),
                    format!(
                        "{}; then the folder holds {}",
                        answer.summary(),
                        self.listed(&after)
                    ),
                );
            }
            gone = if empty {
                "lists as []".to_owned()
            } else {
                format!("STILL HAS {}", self.listed(&after))
            };
            if empty {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let root = self.storage.list(&RemotePath::root()).await;
        let (after, still_there) = match &root {
            Ok(listing) => (
                listing.entries.len(),
                listing.entries.iter().any(|e| e.path == self.folder),
            ),
            Err(_) => (usize::MAX, true),
        };
        self.observe(
            "cleanup",
            format!(
                "probe folder {gone}; root entries before {}, after {after}",
                self.root_before
            ),
        );
        // Fewer only after the sweep: a probe writes nothing outside its folder.
        assert!(
            !still_there && after <= self.root_before,
            "the zone root has more in it than before"
        );
    }

    // --- Output -------------------------------------------------------------------------------

    /// `text` with the password taken out.
    fn scrub(&self, text: impl Into<String>) -> String {
        let text = text.into();
        if self.password.is_empty() {
            text
        } else {
            text.replace(&self.password, "<redacted>")
        }
    }

    fn observe(&self, what: &str, text: impl Into<String>) {
        println!("[bunny] {what}: {}", self.scrub(text));
    }

    /// An error's variant and status, scrubbed. A message is kept only where it is the client's
    /// own words or the server's.
    fn kind(&self, error: &ProviderError) -> String {
        self.scrub(format!("{error:?}"))
    }

    fn ok<T>(&self, result: Result<T, ProviderError>, what: &str) -> T {
        result.unwrap_or_else(|error| panic!("{what} failed: {}", self.kind(&error)))
    }

    // --- Paths and URLs -----------------------------------------------------------------------

    /// `rel` (segments separated by `/`) inside the test's folder.
    fn path(&self, rel: &str) -> RemotePath {
        rel.split('/').fold(self.folder.clone(), |path, segment| {
            path.join(segment)
                .unwrap_or_else(|_| panic!("bad segment {segment:?}"))
        })
    }

    /// The URL of `path`, with a trailing slash for a folder, encoded as the client does.
    fn url(&self, path: &RemotePath, folder: bool) -> String {
        format!(
            "{}/{}/{}",
            self.base,
            encode_segment(&self.zone),
            path.to_url_path(folder)
        )
    }

    /// The URL of `path` with its name written as is (`+`, `(` and `)` left alone).
    fn literal_url(&self, rel: &str) -> String {
        format!(
            "{}/{}/{}/{rel}",
            self.base,
            encode_segment(&self.zone),
            self.folder.to_url_path(false)
        )
    }

    // --- Requests -----------------------------------------------------------------------------

    /// One request on the plain client, with the zone password.
    async fn raw(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
    ) -> Answer {
        self.try_raw(method, url, body, headers)
            .await
            .unwrap_or_else(|failure| panic!("{failure}"))
    }

    /// As [`Self::raw`], with a request that got no answer as an error (scrubbed, without its
    /// URL) rather than a panic.
    async fn try_raw(
        &self,
        method: Method,
        url: &str,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
    ) -> Result<Answer, String> {
        let mut request = self
            .http
            .request(method, url)
            .header("AccessKey", self.access_key.clone());
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        if let Some(body) = body {
            request = request.header("Content-Length", body.len()).body(body);
        }
        let response = request.send().await.map_err(|error| {
            format!(
                "request failed: {}",
                self.scrub(error.without_url().to_string())
            )
        })?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(|error| {
            format!(
                "reading failed: {}",
                self.scrub(error.without_url().to_string())
            )
        })?;
        Ok(Answer {
            status,
            headers,
            body,
        })
    }

    /// The listing of `dir` as the service sends it.
    async fn raw_listing(&self, dir: &RemotePath) -> Answer {
        self.raw(
            Method::GET,
            &self.url(dir, true),
            None,
            &[("Accept", "application/json".to_owned())],
        )
        .await
    }

    async fn put_text(&self, rel: &str, text: &str) {
        let result = self
            .patient
            .upload(
                &self.path(rel),
                UploadSource::Bytes(Bytes::from(text.to_owned())),
                &UploadOptions::new("text/plain"),
            )
            .await;
        self.ok(result, &format!("uploading {rel}"));
    }

    async fn fetch(&self, path: &RemotePath) -> Result<Vec<u8>, ProviderError> {
        let stream = self.storage.download(path).await?;
        stream
            .try_fold(Vec::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
    }

    /// What a listing came to, in a few words.
    fn listed(&self, result: &Result<Listing, ProviderError>) -> String {
        match result {
            Ok(listing) => format!(
                "Ok, {} entries [{}]",
                listing.entries.len(),
                listing
                    .entries
                    .iter()
                    .map(|e| format!("{}{}", e.name(), if e.is_folder { "/" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(error) => format!("Err({})", self.kind(error)),
        }
    }
}

/// What the service answered to a raw request.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn text(&self) -> String {
        text(&self.body)
    }

    /// The status, then the body as text.
    fn summary(&self) -> String {
        format!("{} {}", self.status.as_u16(), self.text())
    }

    /// A listing answer as its status and the names in it, never the objects themselves (they
    /// carry `UserId`, which names the account). Anything that is not a JSON array is shown as
    /// [`Self::summary`] does.
    fn listing_summary(&self) -> String {
        match self.json() {
            Some(Value::Array(objects)) => format!(
                "{} array of {} [{}]",
                self.status.as_u16(),
                objects.len(),
                objects
                    .iter()
                    .map(|object| object["ObjectName"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => self.summary(),
        }
    }

    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    fn header_names(&self) -> String {
        let mut names: Vec<&str> = self.headers.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        names.join(", ")
    }
}

/// A body as one line of at most 300 characters, control characters as spaces.
fn text(body: &[u8]) -> String {
    if body.is_empty() {
        return "(empty body)".to_owned();
    }
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut short: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        short.push_str("...");
    }
    format!("{short:?}")
}

/// The JSON type of `value`, and for objects the type of each field: the shape of an answer
/// without its values (some, like `UserId`, name the account).
fn shape(value: &Value) -> String {
    match value {
        Value::Array(items) => match items.first() {
            None => "[] (empty array)".to_owned(),
            Some(first) => format!("array of {}, the first {}", items.len(), shape(first)),
        },
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", json_kind(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => json_kind(other).to_owned(),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The listed object called `name`, if the answer is a listing that has one.
fn listed_object<'a>(listing: &'a Value, name: &str) -> Option<&'a Value> {
    listing
        .as_array()?
        .iter()
        .find(|object| object["ObjectName"] == name)
}

/// The fields of a listed object that say what it is (never `UserId` or `Guid`).
fn describe_object(object: &Value) -> String {
    format!(
        "ObjectName={} IsDirectory={} Length={} ContentType={} Checksum={}",
        object["ObjectName"],
        object["IsDirectory"],
        object["Length"],
        object["ContentType"],
        if object["Checksum"].is_null() {
            "null"
        } else {
            "set"
        }
    )
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn upper_hex(bytes: &[u8]) -> String {
    hex::encode_upper(bytes)
}

/// Runs `body` against a fresh [`Live`] for the test `name`, then cleans up, also when `body`
/// panics (the panic is passed on afterwards).
async fn probe<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<Live>) -> Fut,
    Fut: Future<Output = ()>,
{
    let _turn = ONE_AT_A_TIME.lock().await;
    let live = Arc::new(Live::start(name).await);
    let outcome = AssertUnwindSafe(body(Arc::clone(&live)))
        .catch_unwind()
        .await;
    live.clean_up().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------------

/// Not a probe: removes `probe-*` folders an interrupted run left in the zone root, saying what
/// each `DELETE` answered. Nothing else in the zone is touched.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn sweep_leftover_probe_folders() {
    probe("sweep", |live| async move {
        let root = live.ok(live.storage.list(&RemotePath::root()).await, "root listing");
        for entry in root.entries {
            if !entry.is_folder || !entry.name().starts_with("probe-") {
                continue;
            }
            let inside = live.storage.list(&entry.path).await;
            live.observe(
                &format!("sweep: {}", entry.name()),
                format!("holds {}", live.listed(&inside)),
            );
            let answer = live
                .raw(Method::DELETE, &live.url(&entry.path, true), None, &[])
                .await;
            live.observe("sweep: DELETE folder/", answer.summary());
            let after = live.storage.list(&entry.path).await;
            live.observe("sweep: afterwards it holds", live.listed(&after));
        }
        let root = live.storage.list(&RemotePath::root()).await;
        live.observe("sweep: root afterwards", live.listed(&root));
    })
    .await;
}

/// B1: does `PUT /{zone}/{dir}/` with an empty body make an empty folder? And is that safe on a
/// folder that already has files?
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn b1_put_folder_with_trailing_slash() {
    probe("b1", |live| async move {
        live.put_text("keep.txt", "keep").await;

        let empty = live.path("empty-dir");
        let first = live
            .raw(Method::PUT, &live.url(&empty, true), Some(Vec::new()), &[])
            .await;
        live.observe("B1 PUT dir/ (empty body)", first.summary());
        let again = live
            .raw(Method::PUT, &live.url(&empty, true), Some(Vec::new()), &[])
            .await;
        live.observe("B1 the same PUT again", again.summary());

        let parent = live.raw_listing(&live.folder).await;
        let shown = parent
            .json()
            .as_ref()
            .and_then(|listing| listed_object(listing, "empty-dir").map(describe_object))
            .unwrap_or_else(|| "not in the parent listing".to_owned());
        live.observe("B1 parent listing entry", shown);
        let inside = live.storage.list(&empty).await;
        live.observe("B1 listing the new folder", live.listed(&inside));
        let raw_inside = live.raw_listing(&empty).await;
        live.observe(
            "B1 raw listing of the new folder",
            raw_inside.listing_summary(),
        );

        // Several levels at once.
        let nested = live.path("n1/n2");
        let put = live
            .raw(Method::PUT, &live.url(&nested, true), Some(Vec::new()), &[])
            .await;
        live.observe("B1 PUT n1/n2/ (neither exists)", put.summary());
        let n1 = live.storage.list(&live.path("n1")).await;
        live.observe("B1 listing n1", live.listed(&n1));

        // A folder that has a file keeps it.
        live.put_text("full/x.txt", "x").await;
        let full = live.path("full");
        let put = live
            .raw(Method::PUT, &live.url(&full, true), Some(Vec::new()), &[])
            .await;
        live.observe("B1 PUT full/ (has x.txt)", put.summary());
        let after = live.storage.list(&full).await;
        live.observe("B1 listing full/ afterwards", live.listed(&after));

        // A folder whose name a file already has.
        let put = live
            .raw(
                Method::PUT,
                &live.url(&live.path("keep.txt"), true),
                Some(Vec::new()),
                &[],
            )
            .await;
        live.observe("B1 PUT keep.txt/ (a file)", put.summary());
        let top = live.storage.list(&live.folder).await;
        live.observe("B1 probe folder afterwards", live.listed(&top));
        let kept = live.fetch(&live.path("keep.txt")).await;
        live.observe(
            "B1 keep.txt afterwards",
            match kept {
                Ok(bytes) => format!("{} bytes", bytes.len()),
                Err(error) => live.kind(&error),
            },
        );
    })
    .await;
}

/// B2: are names case-sensitive, for files and for folders?
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn b2_case_sensitivity() {
    probe("b2", |live| async move {
        live.put_text("Case.txt", "upper").await;
        let lower = live
            .raw(
                Method::GET,
                &live.url(&live.path("case.txt"), false),
                None,
                &[],
            )
            .await;
        live.observe("B2 GET case.txt when only Case.txt exists", lower.summary());

        live.put_text("case.txt", "lower").await;
        let listing = live.storage.list(&live.folder).await;
        live.observe("B2 listing after uploading both", live.listed(&listing));
        for name in ["Case.txt", "case.txt"] {
            let body = live.fetch(&live.path(name)).await;
            live.observe(
                &format!("B2 download {name}"),
                match body {
                    Ok(bytes) => text(&bytes),
                    Err(error) => live.kind(&error),
                },
            );
        }

        live.put_text("Dir/x.txt", "x").await;
        let folder = live.storage.list(&live.path("dir")).await;
        live.observe(
            "B2 listing dir/ when only Dir/ exists",
            live.listed(&folder),
        );
    })
    .await;
}

/// What a missing folder lists as, what an empty one lists as, and what deleting something
/// missing answers.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn missing_folder_404_or_empty_array() {
    probe("missing", |live| async move {
        // The probe folder itself does not exist yet.
        let never = live.raw_listing(&live.path("never")).await;
        live.observe(
            "missing: listing a folder whose parent is missing too",
            never.listing_summary(),
        );
        let client = live.storage.list(&live.path("never")).await;
        live.observe("missing: the client says", live.listed(&client));

        live.put_text("f.txt", "f").await;
        let nope = live.raw_listing(&live.path("nope")).await;
        live.observe(
            "missing: listing a missing folder in an existing one",
            format!(
                "{} [headers: {}]",
                nope.listing_summary(),
                nope.header_names()
            ),
        );
        let client = live.storage.list(&live.path("nope")).await;
        live.observe("missing: the client says", live.listed(&client));

        let file_as_folder = live.raw_listing(&live.path("f.txt")).await;
        live.observe(
            "missing: listing a file as a folder",
            file_as_folder.listing_summary(),
        );

        // A folder whose last file goes.
        live.put_text("gone/x.txt", "x").await;
        let deleted = live.storage.delete_file(&live.path("gone/x.txt")).await;
        live.observe(
            "missing: delete the only file in gone/",
            format!("{deleted:?}"),
        );
        let parent = live.storage.list(&live.folder).await;
        live.observe("missing: parent after that", live.listed(&parent));
        let emptied = live.raw_listing(&live.path("gone")).await;
        live.observe(
            "missing: raw listing of the emptied folder",
            emptied.listing_summary(),
        );

        // The shape of a listing that has something in it.
        let full = live.raw_listing(&live.folder).await;
        live.observe(
            "missing: listing JSON shape",
            full.json()
                .map_or_else(|| "not JSON".to_owned(), |listing| shape(&listing)),
        );
        live.observe(
            "missing: listing headers",
            format!(
                "{} [content-type {:?}]",
                full.header_names(),
                full.headers.get("content-type")
            ),
        );

        // Deleting what is not there.
        let file = live
            .raw(
                Method::DELETE,
                &live.url(&live.path("missing.txt"), false),
                None,
                &[],
            )
            .await;
        live.observe("missing: DELETE a missing file", file.summary());
        let folder = live
            .raw(
                Method::DELETE,
                &live.url(&live.path("missing-dir"), true),
                None,
                &[],
            )
            .await;
        live.observe("missing: DELETE a missing folder", folder.summary());
        let client = live.storage.delete_file(&live.path("missing.txt")).await;
        live.observe(
            "missing: the client's delete_file says",
            format!("{client:?}"),
        );

        // A download of a missing file.
        let get = live
            .raw(
                Method::GET,
                &live.url(&live.path("missing.txt"), false),
                None,
                &[],
            )
            .await;
        live.observe("missing: GET a missing file", get.summary());
    })
    .await;
}

/// Bunny's storage listing has no paging parameters (`page=` belongs to the account API, which
/// these tests never call): is a folder of more than a thousand files answered in one piece?
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn listing_comes_in_one_answer() {
    const FILES: usize = 1_050;
    probe("many", |live| async move {
        let failures: Vec<String> = futures_util::stream::iter(0..FILES)
            .map(|n| {
                let live = Arc::clone(&live);
                async move {
                    let path = live.path(&format!("many/f{n:04}.txt"));
                    live.patient
                        .upload(
                            &path,
                            UploadSource::Bytes(Bytes::from(format!("{n}"))),
                            &UploadOptions::new("text/plain"),
                        )
                        .await
                        .err()
                        .map(|error| live.kind(&error))
                }
            })
            .buffer_unordered(16)
            .filter_map(|failure| async move { failure })
            .collect()
            .await;
        live.observe(
            "many: uploads",
            format!(
                "{} of {FILES} failed{}",
                failures.len(),
                failures
                    .first()
                    .map(|f| format!(", the first: {f}"))
                    .unwrap_or_default()
            ),
        );
        let raw = live.raw_listing(&live.path("many")).await;
        let count = raw
            .json()
            .and_then(|listing| listing.as_array().map(Vec::len));
        live.observe(
            "many: raw listing",
            format!(
                "status {}, {} bytes, {count:?} objects, headers [{}]",
                raw.status.as_u16(),
                raw.body.len(),
                raw.header_names()
            ),
        );
        let listing = live.storage.list(&live.path("many")).await;
        let listed = listing.as_ref().map(|l| l.entries.len());
        live.observe("many: the client lists", format!("{listed:?}"));
        assert_eq!(
            listed.ok(),
            Some(FILES - failures.len()),
            "every uploaded file is in one listing"
        );

        // A client that gives up on the recursive DELETE (as one with a request timeout does):
        // does the service go on deleting?
        let impatient = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| panic!("could not build the HTTP client"));
        let cut = impatient
            .delete(live.url(&live.path("many"), true))
            .header("AccessKey", live.access_key.clone())
            .send()
            .await;
        live.observe(
            "many: DELETE many/ with a 10 s timeout",
            match cut {
                Ok(response) => format!("answered {}", response.status().as_u16()),
                Err(error) if error.is_timeout() => "timed out".to_owned(),
                Err(error) => live.scrub(error.without_url().to_string()),
            },
        );
        let mut counts = Vec::new();
        for _ in 0..3 {
            let count = live
                .storage
                .list(&live.path("many"))
                .await
                .map(|l| l.entries.len());
            counts.push(format!("{count:?}"));
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        live.observe(
            "many: files left 0, 10 and 20 s after giving up",
            counts.join(", "),
        );

        // How long the rest takes to delete: the answer comes only once the whole subtree is
        // gone.
        let started = std::time::Instant::now();
        let answer = live
            .raw(
                Method::DELETE,
                &live.url(&live.path("many"), true),
                None,
                &[],
            )
            .await;
        live.observe(
            "many: DELETE many/ (recursive)",
            format!(
                "{} after {:.1} s",
                answer.summary(),
                started.elapsed().as_secs_f64()
            ),
        );
        let after = live.storage.list(&live.path("many")).await;
        let left = after.as_ref().map(|l| l.entries.len());
        live.observe("many: left in many/ afterwards", format!("{left:?}"));
    })
    .await;
}

/// What Bunny says when it is overloaded: a burst of listings past the five the client allows,
/// then of uploads. One burst each, no retries.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn busy_400_body_text() {
    const BURST: usize = 60;
    probe("busy", |live| async move {
        live.put_text("f.txt", "f").await;

        let url = live.url(&live.folder, true);
        let listings = join_all((0..BURST).map(|_| live.raw(Method::GET, &url, None, &[]))).await;
        report_burst(&live, "busy: listings", &listings);

        let me: &Live = &live;
        let uploads = join_all((0..BURST).map(|n| {
            let url = me.url(&me.path(&format!("burst/{n:02}.txt")), false);
            async move { me.raw(Method::PUT, &url, Some(vec![b'x'; 16]), &[]).await }
        }))
        .await;
        report_burst(&live, "busy: uploads", &uploads);
    })
    .await;
}

fn report_burst(live: &Live, what: &str, answers: &[Answer]) {
    let mut statuses: BTreeMap<u16, usize> = BTreeMap::new();
    for answer in answers {
        *statuses.entry(answer.status.as_u16()).or_default() += 1;
    }
    live.observe(
        what,
        format!("{} requests at once: statuses {statuses:?}", answers.len()),
    );
    if let Some(odd) = answers.iter().find(|a| !a.status.is_success()) {
        live.observe(
            &format!("{what} first refusal"),
            format!("{} [headers: {}]", odd.summary(), odd.header_names()),
        );
    }
}

/// What a `Checksum` that does not match the body gets, and whether the file is kept anyway.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn checksum_mismatch_body_text() {
    probe("checksum", |live| async move {
        let data = b"checksum probe".to_vec();
        let bad = live.path("bad.txt");
        let wrong = upper_hex(&sha256(b"something else"));
        let put = live
            .raw(
                Method::PUT,
                &live.url(&bad, false),
                Some(data.clone()),
                &[("Checksum", wrong)],
            )
            .await;
        live.observe(
            "checksum: PUT with a wrong Checksum",
            format!("{} [headers: {}]", put.summary(), put.header_names()),
        );
        let stored = live
            .raw(Method::GET, &live.url(&bad, false), None, &[])
            .await;
        live.observe("checksum: GET it afterwards", stored.summary());

        let lower = live
            .raw(
                Method::PUT,
                &live.url(&live.path("lower.txt"), false),
                Some(data.clone()),
                &[("Checksum", hex::encode(sha256(&data)))],
            )
            .await;
        live.observe(
            "checksum: PUT with a right lowercase Checksum",
            lower.summary(),
        );

        let mut opts = UploadOptions::new("text/plain");
        opts.sha256 = Some(sha256(b"something else"));
        let client = live
            .storage
            .upload(&bad, UploadSource::Bytes(Bytes::from(data.clone())), &opts)
            .await;
        live.observe("checksum: the client's upload says", format!("{client:?}"));

        opts.sha256 = Some(sha256(&data));
        let good = live.path("good.txt");
        let client = live
            .storage
            .upload(&good, UploadSource::Bytes(Bytes::from(data.clone())), &opts)
            .await;
        live.observe(
            "checksum: the client's upload with the right one",
            format!("{client:?}"),
        );
        let listing = live.raw_listing(&live.folder).await;
        let listed = listing
            .json()
            .as_ref()
            .and_then(|l| listed_object(l, "good.txt").map(|o| o["Checksum"].clone()));
        let expected = upper_hex(&sha256(&data));
        live.observe(
            "checksum: listing Checksum of good.txt",
            match listed {
                Some(Value::String(sum)) if sum == expected => "the SHA-256, uppercase".to_owned(),
                Some(Value::String(sum)) if sum.eq_ignore_ascii_case(&expected) => {
                    "the SHA-256, lowercase".to_owned()
                }
                other => format!("something else: {other:?}"),
            },
        );
        assert_eq!(client, Ok(()), "an upload with the right checksum succeeds");
    })
    .await;
}

/// Is `LastChanged` UTC (it has no zone designator)?
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn last_changed_is_utc() {
    probe("time", |live| async move {
        let before = Utc::now();
        live.put_text("time.txt", "t").await;
        let after = Utc::now();

        let listing = live.raw_listing(&live.folder).await;
        let object = listing
            .json()
            .as_ref()
            .and_then(|l| listed_object(l, "time.txt").cloned())
            .unwrap_or_else(|| panic!("time.txt is not listed"));
        // The server's own clock, from the answer's `Date`, against this PC's.
        let server_now = listing
            .headers
            .get("date")
            .and_then(|date| date.to_str().ok())
            .and_then(|date| DateTime::parse_from_rfc2822(date).ok())
            .map(|date| date.with_timezone(&Utc));
        live.observe(
            "time: the answer's Date header minus this PC's clock",
            match server_now {
                Some(date) => format!("{:+} s", (date - Utc::now()).num_seconds()),
                None => "no Date header".to_owned(),
            },
        );
        let changed = object["LastChanged"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let created = object["DateCreated"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        live.observe(
            "time: raw values",
            format!("LastChanged {changed:?}, DateCreated {created:?}"),
        );
        let as_utc = |text: &str| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|naive| naive.and_utc())
        };
        let seconds_off = |time: DateTime<Utc>| {
            if time < before {
                (time - before).num_milliseconds() as f64 / 1000.0
            } else if time > after {
                (time - after).num_milliseconds() as f64 / 1000.0
            } else {
                0.0
            }
        };
        let changed_utc = as_utc(&changed).unwrap_or_else(|| panic!("LastChanged did not parse"));
        let off = seconds_off(changed_utc);
        live.observe(
            "time: LastChanged read as UTC",
            format!(
                "{off:+.1} s from the upload window ({} s long); this PC is at UTC{}",
                (after - before).num_milliseconds() as f64 / 1000.0,
                chrono::Local::now().format("%:z")
            ),
        );
        let entry = live
            .ok(live.storage.list(&live.folder).await, "listing")
            .entries
            .into_iter()
            .find(|e| e.name() == "time.txt")
            .and_then(|e| e.modified);
        live.observe("time: the client's modified", format!("{entry:?}"));
        assert!(
            off.abs() < 300.0,
            "LastChanged is UTC (within five minutes)"
        );
    })
    .await;
}

/// Names with characters URLs treat specially, as the client encodes them, and two written as
/// is.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn encoded_parentheses_and_plus_accepted() {
    const NAMES: [&str; 7] = [
        "a (1)+b.txt",
        "plus+sign.txt",
        "space name.txt",
        "\u{fc}\u{f1} \u{65e5}\u{672c}.txt",
        "hash#percent%.txt",
        "semi;comma,eq=amp&.txt",
        "quote'tilde~[x].txt",
    ];
    probe("names", |live| async move {
        for name in NAMES {
            let result = live
                .storage
                .upload(
                    &live.path(name),
                    UploadSource::Bytes(Bytes::from(name.to_owned())),
                    &UploadOptions::new("text/plain"),
                )
                .await;
            live.observe(&format!("names: upload {name:?}"), format!("{result:?}"));
        }
        let listing = live.ok(live.storage.list(&live.folder).await, "listing");
        let listed: Vec<&str> = listing.entries.iter().map(|e| e.name()).collect();
        let mut bad = Vec::new();
        for name in NAMES {
            let body = live.fetch(&live.path(name)).await;
            let round_trip = listed.contains(&name) && body.as_deref() == Ok(name.as_bytes());
            live.observe(
                &format!("names: {name:?}"),
                format!(
                    "listed under the same name: {}, downloads the same: {}",
                    listed.contains(&name),
                    body.as_deref() == Ok(name.as_bytes())
                ),
            );
            if !round_trip {
                bad.push(name);
            }
        }

        // Written as is: does Bunny read `+` as a space, and keep the parentheses?
        for literal in ["raw+plus.txt", "raw(paren).txt"] {
            let put = live
                .raw(
                    Method::PUT,
                    &live.literal_url(literal),
                    Some(b"raw".to_vec()),
                    &[],
                )
                .await;
            live.observe(&format!("names: PUT {literal} unencoded"), put.summary());
        }
        let after = live.storage.list(&live.folder).await;
        live.observe(
            "names: listing with the unencoded ones",
            live.listed(&after),
        );
        assert!(bad.is_empty(), "names that did not round-trip: {bad:?}");
    })
    .await;
}

/// The client as the findings above left it: `create_folder` (twice, the second time onto a
/// folder that is there), an empty folder, a missing one and a file asked for as a folder.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone"]
async fn client_folders_after_the_findings() {
    probe("client", |live| async move {
        let made = live.path("made/inner");
        let first = live.storage.create_folder(&made).await;
        live.observe("client: create_folder made/inner", format!("{first:?}"));
        let again = live.storage.create_folder(&made).await;
        live.observe(
            "client: create_folder made/inner again",
            format!("{again:?}"),
        );
        let empty = live.storage.list(&made).await;
        live.observe("client: listing the new empty folder", live.listed(&empty));
        let missing = live.storage.list(&live.path("made/nope")).await;
        live.observe("client: listing a missing folder", live.listed(&missing));
        live.put_text("made/f.txt", "f").await;
        let file = live.storage.list(&live.path("made/f.txt")).await;
        live.observe("client: listing a file as a folder", live.listed(&file));

        assert_eq!(first, Ok(()), "create_folder makes the folder");
        assert_eq!(again, Ok(()), "a folder that is there is fine");
        assert!(empty.is_ok_and(|l| l.entries.is_empty()));
        assert_eq!(missing.err(), Some(ProviderError::NotFound));
        assert_eq!(file.err(), Some(ProviderError::NotFound));
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// An upload cut off part way, then sent again in full (acceptance S5, ruling Q55)
// ---------------------------------------------------------------------------------------------

/// How long a trial's file is watched once its full upload was answered.
const WATCH: Duration = Duration::from_secs(180);
/// How often every trial's file is looked at: one listing of their folder, and a direct
/// request for each.
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// The pieces a cut upload's body is handed over in.
const CUT_PIECE: usize = 64 * 1024;

/// How a trial's first upload is cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// Half the body handed over, a second for it to cross, then the connection closed: a
    /// process killed while sending a big file.
    Half,
    /// The whole body handed over, the connection closed before the answer is read: a process
    /// killed while a small file's upload waits for its answer, as most of S5's 32 were.
    AfterBody,
}

/// When a trial's full upload goes, against its cut one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// The cut upload ends, and the full one goes this long after.
    CutFirst(Duration),
    /// The full upload goes while the cut one hangs mid-body, its connection open, as a killed
    /// upload the server has not finished with yet would; the cut comes this long after the
    /// full upload was answered.
    FullWhileHeld(Duration),
    /// As [`Self::FullWhileHeld`], but the first upload is never cut by this side: its
    /// connection stays open, silent, until the server ends it (a PC that lost power or its
    /// network mid-upload sends no close either), or this long has passed.
    FullWhileHeldUntilServerEnds(Duration),
}

impl Order {
    fn describe(self) -> String {
        match self {
            Self::CutFirst(delay) => format!("cut, full {} s later", delay.as_secs()),
            Self::FullWhileHeld(hold) => format!("full while held, cut {} s later", hold.as_secs()),
            Self::FullWhileHeldUntilServerEnds(limit) => format!(
                "full while held, left for the server to end (at most {} s)",
                limit.as_secs()
            ),
        }
    }
}

/// One trial: a file uploaded and cut off, and uploaded again in full, in `order`.
#[derive(Debug, Clone, Copy)]
struct Trial {
    n: usize,
    size: usize,
    cut: Cut,
    /// Both uploads send the file's SHA-256 as `Checksum`, as the app does by default.
    checksum: bool,
    order: Order,
}

impl Trial {
    fn rel(&self) -> String {
        format!("cut/t{:02}.bin", self.n)
    }
}

/// What a listing or a direct request said of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    There(u64),
    Missing,
    /// The request failed, or answered something else.
    Unknown(String),
}

/// One look at a trial's file.
#[derive(Debug, Clone)]
struct Look {
    at: Instant,
    listed: Seen,
    direct: Seen,
}

/// How a trial went, up to its full upload and its cut.
struct TrialRun {
    trial: Trial,
    /// "cut", or what the cut upload got before it could be cut.
    cut: String,
    /// When the cut upload was cut.
    cut_at: Instant,
    /// When the full upload was sent, and what it was answered.
    sent: Instant,
    reupload: String,
    answered: Instant,
}

/// `len` bytes that don't compress, from `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// The body of a cut upload: `data` in pieces up to where `cut` stops it. `taken` hears when
/// the connection asks for more than that; a [`Cut::Half`] body then waits for `go` and fails,
/// which closes the connection mid-body.
fn cut_body(
    data: Bytes,
    cut: Cut,
    taken: tokio::sync::oneshot::Sender<()>,
    go: Arc<tokio::sync::Notify>,
) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let limit = match cut {
        Cut::Half => data.len() / 2,
        Cut::AfterBody => data.len(),
    };
    futures_util::stream::unfold((0, Some(taken)), move |(at, mut taken)| {
        let (data, go) = (data.clone(), go.clone());
        async move {
            if at < limit {
                let end = limit.min(at + CUT_PIECE);
                return Some((Ok(data.slice(at..end)), (end, taken)));
            }
            if let Some(taken) = taken.take() {
                let _ = taken.send(());
            }
            if at >= data.len() {
                return None;
            }
            go.notified().await;
            Some((Err(std::io::Error::other("cut off")), (data.len(), None)))
        }
    })
}

/// An upload on its way to being cut off.
struct CutUpload {
    /// Its own client, dropped with the request.
    client: reqwest::Client,
    task: tokio::task::JoinHandle<Result<u16, reqwest::Error>>,
    /// Fails a [`Cut::Half`] body.
    go: Arc<tokio::sync::Notify>,
    /// Whether the connection took the body up to where it stops.
    taken: bool,
}

/// Starts uploading `data` to `url` on a client of its own, with the length declared, and
/// waits until the connection has taken the body up to where `trial`'s cut stops it.
async fn start_cut_upload(
    live: &Live,
    url: &str,
    data: Bytes,
    trial: Trial,
    sum: &str,
) -> CutUpload {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("HTTPorter-live-probe")
        .build()
        .unwrap_or_else(|_| panic!("could not build the HTTP client"));
    let (taken_tx, taken_rx) = tokio::sync::oneshot::channel();
    let go = Arc::new(tokio::sync::Notify::new());
    let len = data.len();
    let body = cut_body(data, trial.cut, taken_tx, go.clone());
    let mut request = client
        .put(url)
        .header("AccessKey", live.access_key.clone())
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", len)
        .body(reqwest::Body::wrap_stream(body));
    if trial.checksum {
        request = request.header("Checksum", sum);
    }
    let task = tokio::spawn(async move { request.send().await.map(|r| r.status().as_u16()) });
    let taken = tokio::time::timeout(Duration::from_secs(120), taken_rx)
        .await
        .is_ok();
    if trial.cut == Cut::Half {
        // Time for what was handed over to cross.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    CutUpload {
        client,
        task,
        go,
        taken,
    }
}

/// Cuts the upload off: a held body fails, the request is dropped and its client with it.
/// Says how it ended.
async fn cut_off(live: &Live, upload: CutUpload) -> String {
    let CutUpload {
        client,
        task,
        go,
        taken,
    } = upload;
    go.notify_one();
    task.abort();
    let ended = match task.await {
        Err(error) if error.is_cancelled() => "cut".to_owned(),
        Err(_) => "the request's task panicked".to_owned(),
        Ok(Ok(status)) => format!("answered {status} first"),
        Ok(Err(error)) if error.is_body() || error.is_request() => "cut".to_owned(),
        Ok(Err(error)) => live.scrub(format!("cut ({})", error.without_url())),
    };
    drop(client);
    if taken {
        ended
    } else {
        format!("{ended}; the body was never all taken")
    }
}

/// Runs `trial`: its cut upload and its full one (as the client sends it: length declared,
/// `Checksum` when the trial says) in the trial's order. Notes in `settled` when both are
/// over.
async fn run_trial(
    live: &Live,
    trial: Trial,
    settled: &std::sync::Mutex<Vec<Option<Instant>>>,
) -> TrialRun {
    let data = Bytes::from(noise(trial.n as u64 + 1, trial.size));
    let sum = upper_hex(&sha256(&data));
    let url = live.url(&live.path(&trial.rel()), false);
    let mut headers = vec![("Content-Type", "application/octet-stream".to_owned())];
    if trial.checksum {
        headers.push(("Checksum", sum.clone()));
    }
    let first = start_cut_upload(live, &url, data.clone(), trial, &sum).await;
    let answer = |result: Result<Answer, String>| match result {
        Ok(answer) => answer.summary(),
        Err(failure) => failure,
    };
    let run = match trial.order {
        Order::CutFirst(delay) => {
            let cut = cut_off(live, first).await;
            let cut_at = Instant::now();
            tokio::time::sleep(delay).await;
            let sent = Instant::now();
            let reupload = answer(
                live.try_raw(Method::PUT, &url, Some(data.to_vec()), &headers)
                    .await,
            );
            TrialRun {
                trial,
                cut,
                cut_at,
                sent,
                reupload,
                answered: Instant::now(),
            }
        }
        Order::FullWhileHeld(hold) => {
            let sent = Instant::now();
            let full = live.try_raw(Method::PUT, &url, Some(data.to_vec()), &headers);
            tokio::pin!(full);
            // The server may hold the full upload until the first ends: then it is cut after
            // a minute, and the full one waited for.
            let early = tokio::time::timeout(Duration::from_secs(60), &mut full).await;
            let answered = Instant::now();
            if early.is_ok() {
                tokio::time::sleep(hold).await;
            }
            let cut = cut_off(live, first).await;
            let cut_at = Instant::now();
            let (reupload, answered) = match early {
                Ok(result) => (answer(result), answered),
                Err(_) => {
                    let result = full.await;
                    (
                        format!("{} (only after the cut)", answer(result)),
                        Instant::now(),
                    )
                }
            };
            TrialRun {
                trial,
                cut,
                cut_at,
                sent,
                reupload,
                answered,
            }
        }
        Order::FullWhileHeldUntilServerEnds(limit) => {
            let sent = Instant::now();
            let reupload = answer(
                live.try_raw(Method::PUT, &url, Some(data.to_vec()), &headers)
                    .await,
            );
            let answered = Instant::now();
            let mut first = first;
            let ended = tokio::time::timeout(limit, &mut first.task).await;
            let cut_at = Instant::now();
            let cut = match ended {
                Ok(joined) => {
                    drop(first);
                    let how = match joined {
                        Ok(Ok(status)) => format!("HTTP {status}"),
                        Ok(Err(error)) => live.scrub(error.without_url().to_string()),
                        Err(_) => "its task failed".to_owned(),
                    };
                    format!(
                        "the server ended it {:.0} s after the full answer: {how}",
                        (cut_at - answered).as_secs_f64()
                    )
                }
                Err(_) => format!(
                    "still open after {} s, then {}",
                    limit.as_secs(),
                    cut_off(live, first).await
                ),
            };
            TrialRun {
                trial,
                cut,
                cut_at,
                sent,
                reupload,
                answered,
            }
        }
    };
    settled.lock().unwrap_or_else(PoisonError::into_inner)[trial.n] =
        Some(run.answered.max(run.cut_at));
    run
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// A direct request for the file at `url`: a `HEAD`, or a `GET` whose body is never read when
/// the `HEAD` answers neither 200 nor 404.
async fn look_at(live: &Live, url: &str) -> Seen {
    let ask = |method: Method| {
        live.http
            .request(method, url)
            .header("AccessKey", live.access_key.clone())
            .send()
    };
    let answer = match ask(Method::HEAD).await {
        Ok(head) if matches!(head.status().as_u16(), 200 | 404) => head,
        _ => match ask(Method::GET).await {
            Ok(get) => get,
            Err(error) => return Seen::Unknown(live.scrub(error.without_url().to_string())),
        },
    };
    match answer.status().as_u16() {
        200 => Seen::There(content_length(answer.headers()).unwrap_or(u64::MAX)),
        404 => Seen::Missing,
        status => Seen::Unknown(format!("HTTP {status}")),
    }
}

/// Looks at every trial's file every [`WATCH_EVERY`], from the start until each trial
/// `settled` (its full upload answered and its cut made) [`WATCH`] ago, fifteen minutes at
/// most: one listing of their folder, and a direct request for each file.
async fn watch(
    live: &Live,
    trials: &[Trial],
    settled: &std::sync::Mutex<Vec<Option<Instant>>>,
) -> Vec<Vec<Look>> {
    let dir = live.path("cut");
    let urls: Vec<String> = trials
        .iter()
        .map(|trial| live.url(&live.path(&trial.rel()), false))
        .collect();
    let mut looks = vec![Vec::new(); trials.len()];
    let started = Instant::now();
    loop {
        let round = Instant::now();
        let listing = live
            .try_raw(
                Method::GET,
                &live.url(&dir, true),
                None,
                &[("Accept", "application/json".to_owned())],
            )
            .await;
        let listed: Result<HashMap<String, u64>, String> = match listing {
            Ok(answer) if answer.status == StatusCode::OK => match answer.json() {
                Some(Value::Array(objects)) => Ok(objects
                    .iter()
                    .filter_map(|object| {
                        Some((
                            object["ObjectName"].as_str()?.to_owned(),
                            object["Length"].as_u64()?,
                        ))
                    })
                    .collect()),
                _ => Err("not a listing".to_owned()),
            },
            // The folder doesn't exist yet: nothing is in it.
            Ok(answer) if answer.status == StatusCode::NOT_FOUND => Ok(HashMap::new()),
            Ok(answer) => Err(format!("HTTP {}", answer.status.as_u16())),
            Err(failure) => Err(failure),
        };
        let directs = join_all(urls.iter().map(|url| look_at(live, url))).await;
        for ((trial, direct), seen) in trials.iter().zip(directs).zip(looks.iter_mut()) {
            let name = format!("t{:02}.bin", trial.n);
            let listed = match &listed {
                Ok(names) => names
                    .get(&name)
                    .map_or(Seen::Missing, |&len| Seen::There(len)),
                Err(failure) => Seen::Unknown(failure.clone()),
            };
            seen.push(Look {
                at: round,
                listed,
                direct,
            });
        }
        let settled = settled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let all_watched = settled
            .iter()
            .all(|at| at.is_some_and(|at| at.elapsed() >= WATCH));
        if all_watched || started.elapsed() >= Duration::from_secs(900) {
            return looks;
        }
        tokio::time::sleep_until(round + WATCH_EVERY).await;
    }
}

/// How `pick` saw the file from `from` on, as its changes: `there 0 s; missing 34 s` (seconds
/// after `from`). A size other than `size` is named; answers that said nothing are counted.
fn timeline(looks: &[Look], from: Instant, size: u64, pick: fn(&Look) -> &Seen) -> String {
    let mut changes: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    let mut unknown = 0;
    for look in looks.iter().filter(|look| look.at >= from) {
        let state = match pick(look) {
            Seen::There(len) if *len == size => "there".to_owned(),
            Seen::There(len) => format!("size {len}"),
            Seen::Missing => "missing".to_owned(),
            Seen::Unknown(_) => {
                unknown += 1;
                continue;
            }
        };
        if last.as_ref() != Some(&state) {
            changes.push(format!(
                "{state} {} s",
                look.at.saturating_duration_since(from).as_secs()
            ));
            last = Some(state);
        }
    }
    if changes.is_empty() {
        changes.push("never seen".to_owned());
    }
    if unknown > 0 {
        changes.push(format!("({unknown} looks said nothing)"));
    }
    changes.join("; ")
}

/// What `pick` saw last before `before`.
fn last_before(looks: &[Look], before: Instant, size: u64, pick: fn(&Look) -> &Seen) -> String {
    match looks.iter().rev().find(|look| look.at < before).map(pick) {
        Some(Seen::There(len)) if *len == size => "there".to_owned(),
        Some(Seen::There(len)) => format!("size {len}"),
        Some(Seen::Missing) => "missing".to_owned(),
        Some(Seen::Unknown(why)) => format!("? ({why})"),
        None => "not looked at".to_owned(),
    }
}

/// Seconds from `from` to `to`, signed.
fn signed_secs(from: Instant, to: Instant) -> String {
    if to >= from {
        format!("{:.1} s", (to - from).as_secs_f64())
    } else {
        format!("-{:.1} s", (from - to).as_secs_f64())
    }
}

/// Runs `trials` at once in a probe folder named `name`, watches their files, and prints a
/// table: how each cut and full upload went, what the listing said just before the full upload,
/// and what the listing and a direct request said from its answer on.
async fn cut_trials(name: &str, trials: Vec<Trial>) {
    probe(name, |live| async move {
        let settled = std::sync::Mutex::new(vec![None; trials.len()]);
        let me: &Live = &live;
        let (runs, looks) = tokio::join!(
            join_all(trials.iter().map(|&trial| run_trial(me, trial, &settled))),
            watch(me, &trials, &settled)
        );
        live.observe(
            "cut: trials",
            format!(
                "{} files, watched every {} s until {} s after the later of the full upload's answer and the cut",
                trials.len(),
                WATCH_EVERY.as_secs(),
                WATCH.as_secs()
            ),
        );
        println!(
            "| trial | size | checksum | cut | order | cut upload | full upload | took | cut (from the full answer) | listing before the full upload | listing from its answer | direct from its answer |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
        let mut vanished = 0;
        for (run, looks) in runs.iter().zip(&looks) {
            let size = run.trial.size as u64;
            let listing = timeline(looks, run.answered, size, |look| &look.listed);
            let direct = timeline(looks, run.answered, size, |look| &look.direct);
            if listing.contains("missing") || direct.contains("missing") {
                vanished += 1;
            }
            println!(
                "{}",
                live.scrub(format!(
                    "| {} | {} | {} | {:?} | {} | {} | {} | {:.1} s | {} | {} | {listing} | {direct} |",
                    run.trial.n,
                    run.trial.size,
                    if run.trial.checksum { "yes" } else { "no" },
                    run.trial.cut,
                    run.trial.order.describe(),
                    run.cut,
                    run.reupload,
                    run.answered.saturating_duration_since(run.sent).as_secs_f64(),
                    signed_secs(run.answered, run.cut_at),
                    last_before(looks, run.sent, size, |look| &look.listed),
                ))
            );
        }
        live.observe(
            "cut: files missing at some point after their full upload was answered",
            format!("{vanished} of {}", runs.len()),
        );
    })
    .await;
}

/// Ruling Q55's question, from acceptance S5: an upload cut off part way (a process killed
/// mid-upload), then sent again in full and answered 201: is the file there afterwards, and
/// does it stay? Twelve trials at once, each its own 3 MiB file: half the body or all of it
/// sent before the cut, with and without `Checksum`, the full upload right away, 5 s or 30 s
/// later. Every file is watched for three minutes, in the folder's listing and with a direct
/// request, every two seconds.
///
/// Seen (2026-11-04): none of the twelve went missing. A cut upload whose whole body was sent
/// was sometimes stored although its answer was never read.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone; about four minutes"]
async fn cut_upload_then_full_upload() {
    let mut trials = Vec::new();
    for checksum in [true, false] {
        for cut in [Cut::Half, Cut::AfterBody] {
            for delay in [0, 5, 30] {
                trials.push(Trial {
                    n: trials.len(),
                    size: 3 << 20,
                    cut,
                    checksum,
                    order: Order::CutFirst(Duration::from_secs(delay)),
                });
            }
        }
    }
    cut_trials("cut", trials).await;
}

/// As [`cut_upload_then_full_upload`], with the server not done with the cut upload when the
/// full one lands, as a slow server (S5's p99 was about 20 s) can be after a kill: the full
/// upload goes while the first hangs mid-body with its connection open, and the first is cut
/// 0, 10 or 45 s after the full upload's answer. 3 MiB files with and without `Checksum`, and
/// 64 KiB ones (S5's files were small) with it.
///
/// Seen (2026-11-04): all nine went missing, each within one look (2 s) of its cut, and none
/// came back: the full upload is answered 201 and is there until the older, cut-off upload of
/// the same path is ended, which deletes the path.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone; about five minutes"]
async fn full_upload_while_a_cut_one_hangs() {
    let mut trials = Vec::new();
    for (size, checksum) in [(3 << 20, true), (3 << 20, false), (64 << 10, true)] {
        for hold in [0, 10, 45] {
            trials.push(Trial {
                n: trials.len(),
                size,
                cut: Cut::Half,
                checksum,
                order: Order::FullWhileHeld(Duration::from_secs(hold)),
            });
        }
    }
    cut_trials("held", trials).await;
}

/// How long Bunny waits on an upload that stops mid-body without its connection closing (a PC
/// that lost power or its network sends no close), and whether giving up on it deletes a newer
/// full upload of the same path too: two such uploads (3 MiB and 64 KiB) are left for the
/// server to end, at most eight minutes; a third is cut 120 s after its full upload, to see the
/// delete still comes that late.
///
/// Seen (2026-11-04): the server closed both held uploads 54 and 59 s after the full upload's
/// answer (about a minute after their last bytes), and the full upload was gone from then on;
/// the third had gone at 55 s, before its cut. The 3 MiB one was not seen at all after its 201.
#[tokio::test]
#[ignore = "live: needs .env.test and the Bunny test zone; up to fourteen minutes"]
async fn a_held_upload_left_for_the_server_to_end() {
    let limit = Duration::from_secs(480);
    let trials = vec![
        Trial {
            n: 0,
            size: 3 << 20,
            cut: Cut::Half,
            checksum: true,
            order: Order::FullWhileHeldUntilServerEnds(limit),
        },
        Trial {
            n: 1,
            size: 64 << 10,
            cut: Cut::Half,
            checksum: true,
            order: Order::FullWhileHeldUntilServerEnds(limit),
        },
        Trial {
            n: 2,
            size: 64 << 10,
            cut: Cut::Half,
            checksum: true,
            order: Order::FullWhileHeld(Duration::from_secs(120)),
        },
    ];
    cut_trials("server-end", trials).await;
}
