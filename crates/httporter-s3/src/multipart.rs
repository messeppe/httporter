//! Multipart uploads: how [`S3Storage`](crate::storage::S3Storage) sends a file that is too
//! large to send in one piece, and carries on after an interruption.
//!
//! **When.** A file of more than [`MULTIPART_THRESHOLD`] is sent as a multipart upload: a
//! `CreateMultipartUpload` request, the file cut into parts of
//! [`S3Config::part_size`](crate::storage::S3Config::part_size) (16 MiB,
//! see [`PART_SIZE`] and [`part_size_for`]) sent with `UploadPart`, four at a time, and a
//! `CompleteMultipartUpload` that lists them. Each part is read from the file by itself, from
//! its offset (never the whole file at once), carries the `Content-MD5` of its own bytes and
//! goes through the upload's meter piece by piece, like the body of a single `PutObject`.
//!
//! **Resuming.** The part journal in [`UploadOptions::parts`] is told the upload id and part
//! size when the upload is created, and every part when the service has stored it. An upload
//! started with a journal that remembers an earlier attempt carries on from it: it keeps that
//! attempt's upload id and part size (whatever the configuration says now), skips the parts
//! recorded as stored and sends the rest. If the service no longer has the upload (it answers
//! `NoSuchUpload`, which is [`ProviderError::NotFound`]), the recorded progress is worthless,
//! and the file is uploaded again from the start as a new upload. So it is when the service
//! refuses the recorded parts as it completes the upload (`InvalidPart`, `InvalidPartOrder`:
//! they are not what it stored), except that the old upload is aborted first. Whether the file
//! is still the one the parts were cut from is the journal's to check before it answers
//! [`PartJournal::load`] (ruling Q49).
//!
//! **Like every call of the provider, one attempt.** A part that fails ends the upload with that
//! error once the parts in flight beside it are done (their progress is kept); whoever called
//! retries, and the retry resumes.
//!
//! **Cancelling.** Whether to keep an unfinished upload for a resume or to abort it on the
//! service (`AbortMultipartUpload`, which frees the stored parts) is the journal's answer to
//! [`PartJournal::cancelled`], asked when the upload ends without completing: with an error, or
//! because the future was dropped. A pause or a shutdown drops the future and keeps the upload;
//! a cancel by the user is the engine's to mark in the journal first. Without a journal nothing
//! can resume, so an unfinished upload is aborted. (An upload that was dropped is aborted by a
//! task spawned on the runtime that was running it, which is not waited for; the engine sends
//! its own abort for a cancelled upload, with
//! [`StorageProvider::abort_multipart`](httporter_core::StorageProvider::abort_multipart), from
//! a task the app waits for as it closes.)

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use httporter_core::{
    MultipartProgress, PartJournal, ProviderError, S3Preset, UploadOptions, encode_segment,
};
use reqwest::header::{CONTENT_LENGTH, ETAG};
use reqwest::{Body, Method};
use url::Url;

use crate::body::{
    check_file_len, file_range_body, file_range_md5, read_capped, read_limited, until_stalled,
};
use crate::errors::{self, ERROR_BODY_MAX, network_error};
use crate::sigv4::{Credentials, EMPTY_PAYLOAD_SHA256, UNSIGNED_PAYLOAD, sha256_hex};
use crate::storage::{ANSWER_MAX, S3Storage, b64, failure, object_headers, signed_headers};
use crate::xml;

/// Files larger than this are uploaded in parts.
pub const MULTIPART_THRESHOLD: u64 = 64 << 20;

/// The size of one part, unless the service needs another (see [`part_size_for`]).
pub const PART_SIZE: u64 = 16 << 20;

/// The part size to use with `preset`.
pub fn part_size_for(preset: S3Preset) -> u64 {
    match preset {
        S3Preset::DigitalOceanSpaces => 8 << 20,
        _ => PART_SIZE,
    }
}

/// Parts of an upload sent at once.
const PARTS_IN_FLIGHT: usize = 4;

/// No part but the last may be smaller than this, or larger than [`MAX_PART_SIZE`].
const MIN_PART_SIZE: u64 = 5 << 20;
const MAX_PART_SIZE: u64 = 5 << 30;

/// An upload has at most this many parts.
const MAX_PARTS: u64 = 10_000;

/// Sizes of parts are rounded up to a multiple of this when they have to grow.
const MIB: u64 = 1 << 20;

/// `CompleteMultipartUpload` can take a while on a large object (the service assembles it
/// before it answers), so it may take longer than a short request, at least this long.
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(120);

/// The size of the parts of a new upload of `len` bytes, from the configured `size`: within what
/// services accept, and large enough that [`MAX_PARTS`] parts are enough.
fn plan_part_size(size: u64, len: u64) -> u64 {
    let needed = len.div_ceil(MAX_PARTS).next_multiple_of(MIB);
    size.clamp(MIN_PART_SIZE, MAX_PART_SIZE)
        .max(needed)
        .min(MAX_PART_SIZE)
}

/// What a journal remembers of an earlier attempt at uploading a file of `len` bytes, if it is
/// something that can be carried on from.
struct Resume {
    upload_id: String,
    part_size: u64,
    /// Number to ETag of the parts stored.
    done: BTreeMap<u32, String>,
}

impl Resume {
    fn from(progress: MultipartProgress, len: u64) -> Option<Self> {
        if progress.upload_id.is_empty() || progress.part_size == 0 {
            return None;
        }
        let total = len.div_ceil(progress.part_size);
        if total > MAX_PARTS {
            return None;
        }
        let done = progress
            .parts
            .into_iter()
            .filter(|(number, etag)| {
                (1..=total).contains(&u64::from(*number)) && !etag.trim().is_empty()
            })
            .collect();
        Some(Self {
            upload_id: progress.upload_id,
            part_size: progress.part_size,
            done,
        })
    }
}

/// The upload being worked on.
struct Job<'a> {
    /// The object's address, without a query.
    object: &'a Url,
    file: &'a Path,
    len: u64,
    upload_id: &'a str,
    part_size: u64,
    opts: &'a UploadOptions,
}

impl Job<'_> {
    fn total_parts(&self) -> u32 {
        // At most MAX_PARTS: checked when the part size was chosen.
        u32::try_from(self.len.div_ceil(self.part_size)).unwrap_or(u32::MAX)
    }

    /// Where part `number` (1-based) starts in the file, and how long it is.
    fn range(&self, number: u32) -> (u64, u64) {
        let offset = u64::from(number - 1) * self.part_size;
        (offset, self.part_size.min(self.len - offset))
    }

    /// The address of a request about this upload, with `query` after the upload id.
    fn url(&self, query: &[(&str, String)]) -> Url {
        query_url(self.object, self.upload_id, query)
    }
}

/// `object` with `uploadId` and the other parameters in `query` as its query.
fn query_url(object: &Url, upload_id: &str, query: &[(&str, String)]) -> Url {
    let mut text = String::new();
    for (name, value) in query {
        text.push_str(&format!("{name}={}&", encode_segment(value)));
    }
    text.push_str(&format!("uploadId={}", encode_segment(upload_id)));
    let mut url = object.clone();
    url.set_query(Some(&text));
    url
}

impl S3Storage {
    /// Uploads the `len` bytes of the file at `file` to `object` in parts, resuming from
    /// `opts.parts` when it remembers an earlier attempt. The module docs tell the whole story.
    pub(crate) async fn upload_in_parts(
        &self,
        object: &Url,
        file: &Path,
        len: u64,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        check_file_len(file, len).await?;
        let journal = opts.parts.clone();
        let mut resume = journal
            .as_ref()
            .and_then(|journal| journal.load())
            .and_then(|progress| Resume::from(progress, len));
        loop {
            let resumed = resume.is_some();
            let (upload_id, part_size, done) = match resume.take() {
                Some(earlier) => (earlier.upload_id, earlier.part_size, earlier.done),
                None => {
                    let part_size = plan_part_size(self.part_size, len);
                    let upload_id = self.create_upload(object, opts).await?;
                    if let Some(journal) = &journal {
                        journal.started(&upload_id, part_size);
                    }
                    (upload_id, part_size, BTreeMap::new())
                }
            };

            // From here on, an upload that ends without completing is looked after: kept
            // for a resume, or aborted.
            let mut unfinished = Unfinished::new(self, object, &upload_id, journal.clone());
            let job = Job {
                object,
                file,
                len,
                upload_id: &upload_id,
                part_size,
                opts,
            };
            match self.send_all(&job, done).await {
                Ok(()) => {
                    unfinished.forget();
                    if let Some(journal) = &journal {
                        journal.finished();
                    }
                    return Ok(());
                }
                // The service does not have the upload any more: there is nothing to resume.
                Err(Stopped::Gone) if resumed => unfinished.forget(),
                // It has the upload, but not the parts the journal recorded (ruling Q49): they
                // are worth nothing, so the upload is given up and the file sent again.
                Err(Stopped::BadParts(_)) if resumed => unfinished.abort_now().await,
                Err(stopped) => {
                    unfinished.settle().await;
                    return Err(stopped.into_error());
                }
            }
        }
    }

    /// `AbortMultipartUpload` for the upload `upload_id` of `object`. An upload the service no
    /// longer has is as good as aborted.
    pub(crate) async fn abort_upload(
        &self,
        object: &Url,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        let url = query_url(object, upload_id, &[]);
        let response = self
            .call(Method::DELETE, url, &[], EMPTY_PAYLOAD_SHA256, None, None)
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        match failure(response, self.request_timeout).await {
            ProviderError::NotFound => Ok(()),
            error => Err(error),
        }
    }

    /// `CreateMultipartUpload`; answers the upload id.
    async fn create_upload(
        &self,
        object: &Url,
        opts: &UploadOptions,
    ) -> Result<String, ProviderError> {
        let mut url = object.clone();
        url.set_query(Some("uploads"));
        let extra = object_headers(opts);
        let response = self
            .call(
                Method::POST,
                url,
                &extra,
                EMPTY_PAYLOAD_SHA256,
                Some(Vec::new()),
                None,
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response, self.request_timeout).await);
        }
        let body = read_limited(response, ANSWER_MAX).await?;
        xml::parse_initiate(&body, status.as_u16())
    }

    /// Sends the parts that are not `done` yet, then completes the upload.
    async fn send_all(
        &self,
        job: &Job<'_>,
        mut done: BTreeMap<u32, String>,
    ) -> Result<(), Stopped> {
        let mut waiting: VecDeque<u32> = (1..=job.total_parts())
            .filter(|number| !done.contains_key(number))
            .collect();
        // The parts an earlier attempt stored count as done, but not as sent now, so they don't
        // make a burst of speed.
        if let Some(meter) = &job.opts.meter {
            let stored: u64 = done
                .keys()
                .filter(|&&number| (1..=job.total_parts()).contains(&number))
                .map(|&number| job.range(number).1)
                .sum();
            if stored > 0 {
                meter.skipped(stored).await;
            }
        }
        let mut running = FuturesUnordered::new();
        let mut failed: Option<ProviderError> = None;
        loop {
            // After a failure the parts in flight are let finish, so that what they store is
            // kept; no new one starts.
            while failed.is_none()
                && running.len() < PARTS_IN_FLIGHT
                && let Some(number) = waiting.pop_front()
            {
                running.push(self.send_part(job, number));
            }
            let Some(result) = running.next().await else {
                break;
            };
            match result {
                Ok((number, etag)) => {
                    if let Some(journal) = &job.opts.parts {
                        journal.part_done(number, &etag);
                    }
                    done.insert(number, etag);
                }
                // The upload is gone: the parts beside this one are lost with it.
                Err(ProviderError::NotFound) => return Err(Stopped::Gone),
                Err(error) => {
                    failed.get_or_insert(error);
                }
            }
        }
        if let Some(error) = failed {
            return Err(Stopped::Failed(error));
        }
        let parts: Vec<(u32, String)> = done.into_iter().collect();
        self.complete(job, &parts).await
    }

    /// `UploadPart` for part `number`; answers it with the `ETag` the service gave it.
    async fn send_part(&self, job: &Job<'_>, number: u32) -> Result<(u32, String), ProviderError> {
        let (offset, size) = job.range(number);
        // The service checks the part against this, so the bytes are read once to hash and
        // once to send; a part is never held in memory.
        let md5 = file_range_md5(job.file, offset, size).await?;
        let body = file_range_body(job.file, offset, size, job.opts.meter.clone()).await?;
        let url = job.url(&[("partNumber", number.to_string())]);
        let extra = [("content-md5".to_owned(), b64(&md5))];
        let headers = self.sign_request(&Method::PUT, &url, &extra, UNSIGNED_PAYLOAD)?;
        let request = self
            .transfer_client
            .put(url)
            .headers(headers)
            // Declared, so the body is sent with a length: S3 refuses a chunked upload.
            .header(CONTENT_LENGTH, body.len)
            .body(Body::wrap_stream(body.stream));

        // As for a single upload: the answer comes after the whole body, so what is limited
        // is the time since the connection last asked for more of it.
        let sent = until_stalled(
            request.send(),
            &body.progress,
            self.transfer_idle_timeout,
            "upload",
        )
        .await?;
        let response = match sent {
            Ok(response) => response,
            // A body that failed because the file was not what it was said to be.
            Err(error) => {
                return Err(body.fault.take().unwrap_or_else(|| network_error(error)));
            }
        };
        if !response.status().is_success() {
            return Err(failure(response, self.transfer_idle_timeout).await);
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| {
                ProviderError::Permanent(
                    "unexpected response from server (a part was stored without an ETag)".into(),
                )
            })?;
        Ok((number, etag.to_owned()))
    }

    /// `CompleteMultipartUpload` for `parts`, which are all of them in order.
    async fn complete(&self, job: &Job<'_>, parts: &[(u32, String)]) -> Result<(), Stopped> {
        let body = xml::complete_request(parts).into_bytes();
        let extra = [("content-type".to_owned(), "application/xml".to_owned())];
        let payload = sha256_hex(&body);
        let timeout = self.request_timeout.max(COMPLETE_TIMEOUT);
        let response = self
            .call(
                Method::POST,
                job.url(&[]),
                &extra,
                &payload,
                Some(body),
                Some(timeout),
            )
            .await
            .map_err(Stopped::Failed)?;
        let status = response.status();
        let retry_after = errors::retry_after(response.headers());
        let body = if status.is_success() {
            read_limited(response, ANSWER_MAX)
                .await
                .map_err(Stopped::Failed)?
        } else {
            tokio::time::timeout(timeout, read_capped(response, ERROR_BODY_MAX))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
        };
        // The answer can be a failure that came after the `200`.
        let failed = if status.is_success() {
            xml::parse_complete(&body, status.as_u16()).err()
        } else {
            Some(errors::from_response(status.as_u16(), retry_after, &body))
        };
        let Some(error) = failed else {
            return Ok(());
        };
        let code = xml::parse_error(&body).code;
        Err(match code.as_deref() {
            _ if error == ProviderError::NotFound => Stopped::Gone,
            Some("InvalidPart" | "InvalidPartOrder") => Stopped::BadParts(error),
            _ => Stopped::Failed(error),
        })
    }
}

/// Why sending an upload's parts stopped short of completing it.
#[derive(Debug)]
enum Stopped {
    /// The service no longer has the upload (`NoSuchUpload`).
    Gone,
    /// The service refused the parts listed to complete it (`InvalidPart`, `InvalidPartOrder`):
    /// they are not the parts it stored.
    BadParts(ProviderError),
    Failed(ProviderError),
}

impl Stopped {
    fn into_error(self) -> ProviderError {
        match self {
            Self::Gone => ProviderError::NotFound,
            Self::BadParts(error) | Self::Failed(error) => error,
        }
    }
}

/// A multipart upload that exists on the service and may not be completed. When this is dropped
/// or settled, the upload is aborted if nobody could resume it (there is no journal) or the
/// journal says the user cancelled; otherwise it is left for a resume.
struct Unfinished {
    abort: Option<Abort>,
    journal: Option<Arc<dyn PartJournal>>,
}

/// Everything `AbortMultipartUpload` needs, owned, so that it can be sent from a task.
struct Abort {
    client: reqwest::Client,
    creds: Credentials,
    region: String,
    url: Url,
}

impl Abort {
    /// Best effort: a failure here leaves the parts on the service, which an abort could
    /// not do anything about either.
    async fn send(self) {
        let Ok(headers) = signed_headers(
            &self.creds,
            &self.region,
            &Method::DELETE,
            &self.url,
            &[],
            EMPTY_PAYLOAD_SHA256,
        ) else {
            return;
        };
        let _ = self.client.delete(self.url).headers(headers).send().await;
    }
}

impl Unfinished {
    fn new(
        storage: &S3Storage,
        object: &Url,
        upload_id: &str,
        journal: Option<Arc<dyn PartJournal>>,
    ) -> Self {
        Self {
            abort: Some(Abort {
                client: storage.client.clone(),
                creds: storage.creds.clone(),
                region: storage.endpoint.region.clone(),
                url: query_url(object, upload_id, &[]),
            }),
            journal,
        }
    }

    /// The upload is finished, or gone: there is nothing to look after.
    fn forget(&mut self) {
        self.abort = None;
    }

    fn should_abort(&self) -> bool {
        self.journal
            .as_ref()
            .is_none_or(|journal| journal.cancelled())
    }

    /// The upload is worth nothing to anyone: abort it now, whatever the journal says.
    async fn abort_now(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.send().await;
        }
    }

    /// The upload failed: abort it now, or keep it for the retry.
    async fn settle(&mut self) {
        if let Some(abort) = self.abort.take()
            && self.should_abort()
        {
            abort.send().await;
        }
    }
}

impl Drop for Unfinished {
    /// The upload was dropped before it ended: it was paused or cancelled.
    fn drop(&mut self) {
        let Some(abort) = self.abort.take() else {
            return;
        };
        if self.should_abort()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(abort.send());
        }
    }
}
