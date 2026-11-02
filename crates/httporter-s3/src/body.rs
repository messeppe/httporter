//! Upload and download bodies: cutting an upload into pieces that go through the meter, noticing
//! a transfer that has stopped making progress, and reading answers without trusting their size.
//!
//! reqwest's own timeouts cannot express "give up when nothing moves for N seconds": a total
//! timeout cuts off any transfer that takes long, and `read_timeout` only resets between
//! response-body chunks (the wait for the response headers of an upload, which is the whole
//! time the body is being sent, is never reset). So a transfer runs without a total timeout and
//! a [`Progress`] watches it instead.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{Either, select};
use futures_util::stream::{self, Stream, StreamExt};
use httporter_core::{BodyMeter, ByteStream, ProviderError, UploadSource};
use tokio::io::AsyncReadExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;

use crate::errors::{network_error, stall_error};

/// The size of the pieces an upload body is fed to the connection in, and so of the steps the
/// meter sees.
pub(crate) const BODY_CHUNK: usize = 64 * 1024;

/// When a transfer last made progress.
#[derive(Clone)]
pub(crate) struct Progress(Arc<Mutex<Moment>>);

struct Moment {
    last: Instant,
    /// The transfer is waiting for the bandwidth meter, which is the app's doing and not a
    /// stall: a tight limit can hold a piece back for longer than the idle timeout.
    held: bool,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Moment {
            last: Instant::now(),
            held: false,
        })))
    }

    fn moment(&self) -> MutexGuard<'_, Moment> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn touch(&self) {
        self.moment().last = Instant::now();
    }

    fn hold(&self) {
        self.moment().held = true;
    }

    fn release(&self) {
        let mut moment = self.moment();
        moment.held = false;
        moment.last = Instant::now();
    }

    fn idle(&self) -> Duration {
        let moment = self.moment();
        if moment.held {
            Duration::ZERO
        } else {
            moment.last.elapsed()
        }
    }

    /// Completes once nothing has happened for `limit`.
    async fn stalled(&self, limit: Duration) {
        loop {
            let idle = self.idle();
            if idle >= limit {
                return;
            }
            tokio::time::sleep(limit - idle).await;
        }
    }
}

/// Runs `work`, giving up with a network error if `progress` shows nothing for `limit`.
pub(crate) async fn until_stalled<T>(
    work: impl Future<Output = T>,
    progress: &Progress,
    limit: Duration,
    what: &str,
) -> Result<T, ProviderError> {
    match select(pin!(work), pin!(progress.stalled(limit))).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(((), _)) => Err(stall_error(what, limit)),
    }
}

/// Why an upload body could not be produced, for the one case where the HTTP client reports
/// only "the body failed": the local file was not what it was said to be.
#[derive(Clone, Default)]
pub(crate) struct Fault(Arc<Mutex<Option<ProviderError>>>);

impl Fault {
    fn set(&self, error: ProviderError) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
    }

    pub(crate) fn take(&self) -> Option<ProviderError> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// The pieces of an upload body, for `reqwest::Body::wrap_stream`.
pub(crate) type UploadStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

/// An upload ready to send.
pub(crate) struct UploadBody {
    pub stream: UploadStream,
    /// The exact number of bytes the body will produce, for `Content-Length` (S3 refuses a
    /// chunked upload).
    pub len: u64,
    pub progress: Progress,
    pub fault: Fault,
}

/// The body for `source`. A file that is not `len` bytes long has changed since it was looked
/// at, and the upload fails before anything is sent.
pub(crate) async fn upload_body(
    source: &UploadSource,
    meter: Option<Arc<dyn BodyMeter>>,
) -> Result<UploadBody, ProviderError> {
    let (reader, len) = match source {
        UploadSource::Bytes(bytes) => (Reader::Memory(bytes.clone()), bytes.len() as u64),
        UploadSource::File { path, len } => {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|e| local_file_error(path, &e))?;
            let actual = file
                .metadata()
                .await
                .map_err(|e| local_file_error(path, &e))?
                .len();
            if actual != *len {
                return Err(changed_file(path));
            }
            (
                Reader::File {
                    file,
                    path: path.clone().into_boxed_path(),
                },
                *len,
            )
        }
    };
    let progress = Progress::new();
    let fault = Fault::default();
    let feed = Feed {
        reader,
        left: len,
        meter,
        progress: progress.clone(),
        fault: fault.clone(),
    };
    Ok(UploadBody {
        stream: Box::pin(stream::try_unfold(feed, Feed::next)),
        len,
        progress,
        fault,
    })
}

enum Reader {
    Memory(Bytes),
    File {
        file: tokio::fs::File,
        path: Box<Path>,
    },
}

struct Feed {
    reader: Reader,
    /// Bytes still to produce.
    left: u64,
    meter: Option<Arc<dyn BodyMeter>>,
    progress: Progress,
    fault: Fault,
}

impl Feed {
    /// The next piece of the body, after the meter has allowed it, or `None` at the end.
    async fn next(mut self) -> io::Result<Option<(Bytes, Self)>> {
        if self.left == 0 {
            return Ok(None);
        }
        let want = usize::try_from(self.left).map_or(BODY_CHUNK, |left| left.min(BODY_CHUNK));
        let chunk = match &mut self.reader {
            Reader::Memory(bytes) => bytes.split_to(want.min(bytes.len())),
            Reader::File { file, path } => {
                let mut buf = vec![0u8; want];
                let read = file.read(&mut buf).await.inspect_err(|e| {
                    self.fault.set(local_file_error(path, e));
                })?;
                buf.truncate(read);
                Bytes::from(buf)
            }
        };
        if chunk.is_empty() {
            // The source ended before the length it was said to have.
            if let Reader::File { path, .. } = &self.reader {
                self.fault.set(changed_file(path));
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the upload source ended early",
            ));
        }
        self.left -= chunk.len() as u64;
        match self.meter.clone() {
            Some(meter) => {
                self.progress.hold();
                meter.chunk(chunk.len()).await;
                self.progress.release();
            }
            None => self.progress.touch(),
        }
        Ok(Some((chunk, self)))
    }
}

/// A local file that could not be opened or read. Retrying will not conjure it up.
fn local_file_error(path: &Path, error: &io::Error) -> ProviderError {
    ProviderError::Permanent(format!("could not read {}: {error}", path.display()))
}

fn changed_file(path: &Path) -> ProviderError {
    ProviderError::Permanent(format!(
        "{} changed while it was being uploaded",
        path.display()
    ))
}

/// A download body as the engine sees it. It fails with a network error if no data arrives for
/// `idle`, and it holds its transfer `slot` until it ends or is dropped.
pub(crate) fn guard_download<S>(body: S, slot: OwnedSemaphorePermit, idle: Duration) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    Box::pin(stream::try_unfold(
        (body, slot),
        move |(mut body, slot)| async move {
            match tokio::time::timeout(idle, body.next()).await {
                Err(_) => Err(stall_error("download", idle)),
                Ok(None) => Ok(None),
                Ok(Some(Err(error))) => Err(network_error(error)),
                Ok(Some(Ok(chunk))) => Ok(Some((chunk, (body, slot)))),
            }
        },
    ))
}

/// Up to `max` bytes of a response body; the rest is not read.
pub(crate) async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < max {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(body)
}

/// A whole response body, or a permanent error if it is larger than `max`: a listing page of
/// 1,000 keys is a couple of megabytes at most, and a server that sends more is broken.
pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        if body.len() + chunk.len() > max {
            return Err(ProviderError::Permanent(
                "unexpected response from server (answer too large)".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures_util::TryStreamExt;

    #[derive(Default)]
    struct Counting {
        calls: AtomicUsize,
        bytes: AtomicU64,
    }

    #[async_trait]
    impl BodyMeter for Counting {
        async fn chunk(&self, len: usize) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.bytes.fetch_add(len as u64, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn bytes_are_cut_into_metered_chunks() {
        let data: Vec<u8> = (0..(BODY_CHUNK * 2 + 10))
            .map(|i| (i % 251) as u8)
            .collect();
        let meter = Arc::new(Counting::default());
        let built = upload_body(
            &UploadSource::Bytes(Bytes::from(data.clone())),
            Some(meter.clone()),
        )
        .await
        .unwrap();
        assert_eq!(built.len, data.len() as u64);
        let pieces: Vec<Bytes> = built.stream.try_collect().await.unwrap();
        let sizes: Vec<usize> = pieces.iter().map(Bytes::len).collect();
        assert_eq!(sizes, [BODY_CHUNK, BODY_CHUNK, 10]);
        assert_eq!(pieces.concat(), data);
        assert_eq!(meter.calls.load(Ordering::SeqCst), 3);
        assert_eq!(meter.bytes.load(Ordering::SeqCst), data.len() as u64);
    }

    #[tokio::test]
    async fn an_empty_body_has_no_pieces_and_no_meter_calls() {
        let meter = Arc::new(Counting::default());
        let built = upload_body(&UploadSource::Bytes(Bytes::new()), Some(meter.clone()))
            .await
            .unwrap();
        assert_eq!(built.len, 0);
        let pieces: Vec<Bytes> = built.stream.try_collect().await.unwrap();
        assert!(pieces.is_empty());
        assert_eq!(meter.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_file_is_read_in_chunks_without_loading_it_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data: Vec<u8> = (0..(BODY_CHUNK * 3 + 1)).map(|i| (i % 13) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let meter = Arc::new(Counting::default());
        let built = upload_body(
            &UploadSource::File {
                path,
                len: data.len() as u64,
            },
            Some(meter.clone()),
        )
        .await
        .unwrap();
        let pieces: Vec<Bytes> = built.stream.try_collect().await.unwrap();
        assert!(pieces.iter().all(|p| p.len() <= BODY_CHUNK));
        assert_eq!(pieces.concat(), data);
        assert_eq!(meter.bytes.load(Ordering::SeqCst), data.len() as u64);
    }

    #[tokio::test]
    async fn a_file_of_another_length_fails_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"12345").unwrap();
        for len in [4u64, 6] {
            let result = upload_body(
                &UploadSource::File {
                    path: path.clone(),
                    len,
                },
                None,
            )
            .await;
            assert!(
                matches!(&result, Err(ProviderError::Permanent(m)) if m.contains("changed")),
                "len {len}"
            );
        }
        let missing = upload_body(
            &UploadSource::File {
                path: dir.path().join("nope"),
                len: 1,
            },
            None,
        )
        .await;
        assert!(
            matches!(&missing, Err(ProviderError::Permanent(m)) if m.contains("could not read"))
        );
    }

    #[tokio::test]
    async fn a_file_that_shrinks_mid_upload_sets_the_fault() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![7u8; BODY_CHUNK * 2]).unwrap();
        let built = upload_body(
            &UploadSource::File {
                path: path.clone(),
                len: (BODY_CHUNK * 2) as u64,
            },
            None,
        )
        .await
        .unwrap();
        // Truncated after the length was checked and before the body is read.
        std::fs::write(&path, vec![7u8; BODY_CHUNK]).unwrap();
        let fault = built.fault.clone();
        let result: io::Result<Vec<Bytes>> = built.stream.try_collect().await;
        assert!(result.is_err());
        assert!(matches!(
            fault.take(),
            Some(ProviderError::Permanent(m)) if m.contains("changed")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_with_no_progress_stalls_and_one_with_progress_does_not() {
        let progress = Progress::new();
        let limit = Duration::from_secs(10);
        // Work that never finishes and never reports progress.
        let result = until_stalled(std::future::pending::<()>(), &progress, limit, "upload").await;
        assert!(matches!(result, Err(ProviderError::Network(m)) if m.contains("stalled")));

        // Progress every 6 s keeps a 30 s transfer alive past the 10 s limit.
        let progress = Progress::new();
        let busy = {
            let progress = progress.clone();
            async move {
                for _ in 0..5 {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    progress.touch();
                }
                "done"
            }
        };
        assert_eq!(
            until_stalled(busy, &progress, limit, "upload").await,
            Ok("done")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_the_meter_is_not_a_stall() {
        let progress = Progress::new();
        let limit = Duration::from_secs(10);
        let held = {
            let progress = progress.clone();
            async move {
                progress.hold();
                tokio::time::sleep(Duration::from_secs(60)).await;
                progress.release();
                "done"
            }
        };
        assert_eq!(
            until_stalled(held, &progress, limit, "upload").await,
            Ok("done")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_holds_its_slot_until_it_ends_and_stalls_when_idle() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let taken = || Arc::clone(&slots).try_acquire_owned().unwrap();

        // Ends normally: the slot comes back once the stream is exhausted and dropped.
        let body = stream::iter([Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))]);
        let guarded = guard_download(body, taken(), Duration::from_secs(5));
        assert_eq!(slots.available_permits(), 0);
        let chunks: Vec<Bytes> = guarded.try_collect().await.unwrap();
        assert_eq!(chunks.concat(), b"abcd");
        assert_eq!(slots.available_permits(), 1);

        // Nothing arrives: a network error after the idle time, and the slot is released.
        let guarded = guard_download(stream::pending(), taken(), Duration::from_secs(5));
        let result: Result<Vec<Bytes>, _> = guarded.try_collect().await;
        assert!(matches!(result, Err(ProviderError::Network(m)) if m.contains("stalled")));
        assert_eq!(slots.available_permits(), 1);
    }
}
