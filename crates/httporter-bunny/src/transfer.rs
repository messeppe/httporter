//! Uploads and downloads.
//!
//! Transfers have no overall time limit: a big file on a slow link can take hours. They are
//! abandoned only when they stop making progress for [`crate::BunnyConfig::transfer_idle_timeout`]:
//! - an upload body is watched piece by piece as the connection takes it. Once it has taken the
//!   last piece, what it buffered (a few hundred KiB) still has to cross the link, and nothing
//!   shows that happening (ledger 180). So from then on the upload gets one more idle period for
//!   that, plus [`crate::BunnyConfig::request_timeout`] for the server to answer;
//! - a download waits up to the idle timeout for its headers, then for each piece of its body.
//!
//! reqwest's own `read_timeout` is not enough: it only resets between response-body chunks, and
//! the deadline for the response headers is never reset, so it would cut off any upload that
//! takes longer than the timeout to send.

use std::fmt;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{Either, select};
use futures_util::stream::{self, Stream, StreamExt};
use httporter_core::{
    BodyMeter, ByteStream, ProviderError, RemotePath, UploadOptions, UploadSource, with_retry,
};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Body, StatusCode};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Instant;
use tokio_util::io::ReaderStream;

use crate::client::{
    BunnyStorage, ERROR_BODY_MAX, Op, map_status, map_upload_400, network_error, parse_retry_after,
    read_capped,
};

/// The size of the pieces an upload body is fed to the connection in.
const BODY_CHUNK: usize = 64 * 1024;

/// An upload body's pieces, from memory or from a file.
type Pieces = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

impl BunnyStorage {
    pub(crate) async fn upload_to(
        &self,
        path: &RemotePath,
        source: &UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let url = self.file_url(path)?;
        let url = url.as_str();
        with_retry(&self.retry, move |_| self.put_once(url, source, opts)).await
    }

    pub(crate) async fn download_from(
        &self,
        path: &RemotePath,
    ) -> Result<ByteStream, ProviderError> {
        let url = self.file_url(path)?;
        let url = url.as_str();
        let (response, slot) = with_retry(&self.retry, move |_| self.get_once(url)).await?;
        Ok(guard_download(
            Box::pin(response.bytes_stream()),
            slot,
            self.transfer_idle_timeout,
        ))
    }

    /// One attempt at uploading `source`. Called afresh for every retry, so a file is opened
    /// (and a body built) again each time.
    async fn put_once(
        &self,
        url: &str,
        source: &UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let _slot = self
            .transfer_slots
            .acquire()
            .await
            .map_err(|_| transfer_limiter_closed())?;

        let (pieces, len): (Pieces, u64) = match source {
            UploadSource::Bytes(bytes) => (Box::pin(chunked(bytes.clone())), bytes.len() as u64),
            UploadSource::File { path, len } => {
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|e| local_file_error(path, &e))?;
                (
                    Box::pin(ReaderStream::with_capacity(file, BODY_CHUNK)),
                    *len,
                )
            }
        };
        let activity = Activity::new(len);
        let body = upload_body(pieces, opts.meter.clone(), activity.clone());
        let mut request = self
            .with_key(self.transfer_client.put(url))
            .header(CONTENT_TYPE, opts.content_type.as_str())
            // Declared, so the body is sent with a length rather than chunked.
            .header(CONTENT_LENGTH, len)
            .body(Body::wrap_stream(body));
        if let Some(sha256) = &opts.sha256 {
            request = request.header("Checksum", hex_upper(sha256));
        }

        // The server answers only after the whole body has arrived, so the wait covers sending
        // it as well.
        let idle = self.transfer_idle_timeout;
        let mut response = until_stalled(
            request.send(),
            &activity,
            Limits {
                idle,
                answer: self.request_timeout,
            },
            "upload",
        )
        .await?
        .map_err(network_error)?;

        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        // Read the answer whatever it says: one left unread costs the connection (ledger 143),
        // and only the body tells a busy 400 from a rejected upload.
        let body = tokio::time::timeout(idle, read_capped(&mut response, ERROR_BODY_MAX)).await;
        if matches!(status.as_u16(), 200 | 201) {
            return Ok(());
        }
        let with_checksum = opts.sha256.is_some();
        if status == StatusCode::BAD_REQUEST {
            let body = body
                .map_err(|_| stall_error("upload", idle))?
                .map_err(network_error)?;
            return Err(map_upload_400(&body, with_checksum));
        }
        Err(map_status(
            status,
            retry_after,
            Op::Upload { with_checksum },
        ))
    }

    /// One attempt at starting a download: the response headers, plus the transfer slot the
    /// body will keep occupying.
    async fn get_once(
        &self,
        url: &str,
    ) -> Result<(reqwest::Response, OwnedSemaphorePermit), ProviderError> {
        let slot = Arc::clone(&self.transfer_slots)
            .acquire_owned()
            .await
            .map_err(|_| transfer_limiter_closed())?;

        let idle = self.transfer_idle_timeout;
        let sent = tokio::time::timeout(idle, self.with_key(self.transfer_client.get(url)).send())
            .await
            .map_err(|_| stall_error("download", idle))?;
        let mut response = sent.map_err(network_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            // Read the answer, so the connection can carry the next request (ledger 143).
            let _ = tokio::time::timeout(idle, read_capped(&mut response, ERROR_BODY_MAX)).await;
            return Err(map_status(status, retry_after, Op::Download));
        }
        Ok((response, slot))
    }
}

/// Uppercase hex, which is how Bunny wants the `Checksum` header.
fn hex_upper(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02X}");
        out
    })
}

fn transfer_limiter_closed() -> ProviderError {
    ProviderError::Permanent("the transfer limiter was closed".into())
}

/// A local file that could not be opened. Retrying will not conjure it up.
fn local_file_error(path: &Path, error: &io::Error) -> ProviderError {
    ProviderError::Permanent(format!("could not read {}: {error}", path.display()))
}

/// No progress for `waited`: as good as no answer.
fn stall_error(what: &str, waited: Duration) -> ProviderError {
    ProviderError::Network(format!("the {what} stalled (no progress for {waited:?})"))
}

/// An upload body in pieces, so the connection asks for it a bit at a time.
fn chunked(bytes: Bytes) -> impl Stream<Item = io::Result<Bytes>> + Unpin + Send + 'static {
    let len = bytes.len();
    stream::iter(
        (0..len)
            .step_by(BODY_CHUNK)
            .map(move |start| Ok(bytes.slice(start..len.min(start + BODY_CHUNK)))),
    )
}

/// The upload body as the connection sees it: each piece is first announced to the `meter`,
/// which may hold it back (for a bandwidth limit), then handed over and noted in `activity`.
fn upload_body(
    pieces: Pieces,
    meter: Option<Arc<dyn BodyMeter>>,
    activity: Activity,
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    stream::unfold(
        (pieces, meter, activity),
        |(mut pieces, meter, activity)| async move {
            let piece = pieces.next().await?;
            if let Ok(bytes) = &piece {
                if let Some(meter) = &meter {
                    let _held = activity.metering();
                    meter.chunk(bytes.len()).await;
                }
                activity.took(bytes.len());
            }
            Some((piece, (pieces, meter, activity)))
        },
    )
}

/// How long an upload may go without visible progress.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Between pieces of the body.
    idle: Duration,
    /// For the server to answer, once the connection has the whole body (on top of one more
    /// `idle` for what it buffered to drain).
    answer: Duration,
}

/// How an upload body is getting on, for telling a slow upload from a stalled one.
#[derive(Clone)]
struct Activity(Arc<Mutex<Progress>>);

struct Progress {
    /// When the connection last took a piece, or the meter last let one go.
    last: Instant,
    /// Body bytes the connection has yet to take.
    left: u64,
    /// When the connection took the last of the body.
    done: Option<Instant>,
    /// Pieces the meter is holding back right now. Waiting on our own limit is not a stall.
    metering: u32,
}

impl Activity {
    /// A body of `len` bytes, none of it taken yet.
    fn new(len: u64) -> Self {
        let now = Instant::now();
        Self(Arc::new(Mutex::new(Progress {
            last: now,
            left: len,
            done: (len == 0).then_some(now),
            metering: 0,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Progress> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The connection took `len` more bytes. It asks for the next piece only once it has
    /// taken the previous one, so a long gap means the transfer has stalled; merely being
    /// polled does not count, as a poll that finds nothing ready says nothing about progress.
    fn took(&self, len: usize) {
        let mut progress = self.lock();
        let now = Instant::now();
        progress.last = now;
        progress.left = progress.left.saturating_sub(len as u64);
        if progress.left == 0 && progress.done.is_none() {
            progress.done = Some(now);
        }
    }

    /// Marks a piece as held back by the meter until the returned guard is dropped.
    fn metering(&self) -> Metering {
        self.lock().metering += 1;
        Metering(self.clone())
    }

    /// When the upload counts as stalled, and how long it will then have gone without progress;
    /// `None` while the meter holds a piece back.
    fn deadline(&self, limits: Limits) -> Option<(Instant, Duration)> {
        let progress = self.lock();
        match progress.done {
            Some(done) => {
                let waited = limits.idle + limits.answer;
                Some((done + waited, waited))
            }
            None if progress.metering > 0 => None,
            None => Some((progress.last + limits.idle, limits.idle)),
        }
    }

    /// Completes once the upload has stalled, with how long it went without progress.
    async fn stalled(&self, limits: Limits) -> Duration {
        loop {
            match self.deadline(limits) {
                Some((deadline, waited)) if deadline <= Instant::now() => return waited,
                Some((deadline, _)) => tokio::time::sleep_until(deadline).await,
                // Looked at again once the meter could have let the piece go.
                None => tokio::time::sleep(limits.idle).await,
            }
        }
    }
}

/// A piece held back by the meter. Its release counts as progress.
struct Metering(Activity);

impl Drop for Metering {
    fn drop(&mut self) {
        let mut progress = self.0.lock();
        progress.metering -= 1;
        progress.last = Instant::now();
    }
}

/// Runs `work`, giving up with a network error once `activity` shows it has stalled.
async fn until_stalled<T>(
    work: impl Future<Output = T>,
    activity: &Activity,
    limits: Limits,
    what: &str,
) -> Result<T, ProviderError> {
    match select(pin!(work), pin!(activity.stalled(limits))).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right((waited, _)) => Err(stall_error(what, waited)),
    }
}

/// A download body as the engine sees it. It fails with a network error if no data arrives
/// for `idle`, and it holds its transfer `slot` until it ends or is dropped.
fn guard_download<S>(body: S, slot: OwnedSemaphorePermit, idle: Duration) -> ByteStream
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::FutureExt;
    use tokio::sync::Semaphore;

    const PIECE: usize = 1024;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn limits(idle: u64, answer: u64) -> Limits {
        Limits {
            idle: secs(idle),
            answer: secs(answer),
        }
    }

    /// `count` pieces of [`PIECE`] bytes, one every `every`, after which the stream ends or
    /// hangs.
    fn trickle(count: usize, every: Duration, then_hang: bool) -> Pieces {
        Box::pin(stream::unfold(0, move |n| async move {
            if n < count {
                tokio::time::sleep(every).await;
                Some((Ok(Bytes::from(vec![7; PIECE])), n + 1))
            } else if then_hang {
                std::future::pending().await
            } else {
                None
            }
        }))
    }

    /// Takes the whole body the way a connection does, a piece as soon as it is ready, and
    /// says how many bytes it got.
    async fn take_all(body: impl Stream<Item = io::Result<Bytes>>) -> usize {
        let mut body = pin!(body);
        let mut total = 0;
        while let Some(piece) = body.next().await {
            total += piece.unwrap().len();
        }
        total
    }

    /// Ledger 183: on a paused clock, so a loaded machine can't make it flaky.
    #[tokio::test(start_paused = true)]
    async fn a_transfer_is_cut_off_only_when_it_stops_moving() {
        // Eight pieces 2 s apart take 16 s, far past the 5 s limit, but never go quiet for 5 s.
        let activity = Activity::new(8 * PIECE as u64);
        let body = upload_body(trickle(8, secs(2), false), None, activity.clone());
        let done = until_stalled(take_all(body), &activity, limits(5, 60), "upload").await;
        assert_eq!(done, Ok(8 * PIECE));

        // Two pieces of ten, then nothing: cut off 5 s after the second.
        let activity = Activity::new(10 * PIECE as u64);
        let body = upload_body(trickle(2, secs(2), true), None, activity.clone());
        let started = Instant::now();
        match until_stalled(take_all(body), &activity, limits(5, 60), "upload").await {
            Err(ProviderError::Network(m)) => {
                assert!(m.contains("upload stalled (no progress for 5s)"), "{m}");
            }
            other => panic!("expected a stall, got {other:?}"),
        }
        assert_eq!(started.elapsed(), secs(9));
    }

    /// Ledger 180: once the connection has the whole body, the server gets one more idle
    /// period plus the answer time, however long the tail takes to drain.
    #[tokio::test(start_paused = true)]
    async fn once_the_body_is_in_the_answer_gets_its_own_time() {
        // The body goes at once; the answer comes 8 s later, past the 5 s idle limit.
        let activity = Activity::new(3 * PIECE as u64);
        let body = upload_body(trickle(3, Duration::ZERO, false), None, activity.clone());
        let connection = async {
            let sent = take_all(body).await;
            tokio::time::sleep(secs(8)).await;
            sent
        };
        let answered = until_stalled(connection, &activity, limits(5, 5), "upload").await;
        assert_eq!(answered, Ok(3 * PIECE));

        // No answer at all: given up 5 s + 5 s after the last piece.
        let activity = Activity::new(3 * PIECE as u64);
        let body = upload_body(trickle(3, Duration::ZERO, false), None, activity.clone());
        let connection = async {
            take_all(body).await;
            std::future::pending::<()>().await
        };
        let started = Instant::now();
        match until_stalled(connection, &activity, limits(5, 5), "upload").await {
            Err(ProviderError::Network(m)) => {
                assert!(m.contains("upload stalled (no progress for 10s)"), "{m}");
            }
            other => panic!("expected a stall, got {other:?}"),
        }
        assert_eq!(started.elapsed(), secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_body_is_in_from_the_start() {
        let activity = Activity::new(0);
        let started = Instant::now();
        let never = std::future::pending::<()>();
        let result = until_stalled(never, &activity, limits(5, 20), "upload").await;
        assert!(result.is_err());
        assert_eq!(started.elapsed(), secs(25));
    }

    /// Holds every piece for `hold` (a tight bandwidth limit), remembering their sizes.
    struct SlowMeter {
        hold: Duration,
        seen: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl BodyMeter for SlowMeter {
        async fn chunk(&self, len: usize) {
            self.seen.lock().unwrap().push(len);
            tokio::time::sleep(self.hold).await;
        }
    }

    /// A bandwidth limit holding a piece back longer than the idle timeout is our own doing,
    /// not a stalled connection.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_the_meter_is_not_a_stall() {
        let meter = Arc::new(SlowMeter {
            hold: secs(60),
            seen: Mutex::new(Vec::new()),
        });
        let activity = Activity::new(3 * PIECE as u64);
        let body = upload_body(
            trickle(3, Duration::ZERO, false),
            Some(meter.clone()),
            activity.clone(),
        );
        let started = Instant::now();
        let done = until_stalled(take_all(body), &activity, limits(5, 5), "upload").await;
        assert_eq!(done, Ok(3 * PIECE));
        assert_eq!(started.elapsed(), secs(180));
        assert_eq!(*meter.seen.lock().unwrap(), [PIECE; 3]);
    }

    /// Lets one piece through per permit, after noting its size.
    struct GateMeter {
        gate: Semaphore,
        seen: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl BodyMeter for GateMeter {
        async fn chunk(&self, len: usize) {
            self.seen.lock().unwrap().push(len);
            self.gate.acquire().await.unwrap().forget();
        }
    }

    #[tokio::test]
    async fn the_meter_hears_of_each_piece_before_it_is_handed_over() {
        let meter = Arc::new(GateMeter {
            gate: Semaphore::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let data = Bytes::from(vec![1; BODY_CHUNK + 10]);
        let body = upload_body(
            Box::pin(chunked(data.clone())),
            Some(meter.clone()),
            Activity::new(data.len() as u64),
        );
        let mut body = pin!(body);

        assert!(body.next().now_or_never().is_none(), "held by the meter");
        assert_eq!(*meter.seen.lock().unwrap(), [BODY_CHUNK]);
        meter.gate.add_permits(1);
        assert_eq!(body.next().await.unwrap().unwrap().len(), BODY_CHUNK);

        assert!(body.next().now_or_never().is_none(), "held by the meter");
        assert_eq!(*meter.seen.lock().unwrap(), [BODY_CHUNK, 10]);
        meter.gate.add_permits(1);
        assert_eq!(body.next().await.unwrap().unwrap().len(), 10);
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_download_body_stalls_out_and_frees_its_slot() {
        let slots = Arc::new(Semaphore::new(1));
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        let body = stream::iter([Ok(Bytes::from_static(b"x"))])
            .chain(stream::pending::<reqwest::Result<Bytes>>());
        let mut guarded = guard_download(body, slot, secs(5));

        assert_eq!(slots.available_permits(), 0, "held while the body is open");
        assert_eq!(guarded.next().await, Some(Ok(Bytes::from_static(b"x"))));
        let started = Instant::now();
        match guarded.next().await {
            Some(Err(ProviderError::Network(m))) => {
                assert!(m.contains("download stalled"), "{m}");
            }
            other => panic!("expected a stall, got {other:?}"),
        }
        assert_eq!(started.elapsed(), secs(5));
        assert!(
            guarded.next().await.is_none(),
            "the body ends after an error"
        );
        assert_eq!(slots.available_permits(), 1, "freed once the body is over");
    }

    #[tokio::test]
    async fn a_download_body_frees_its_slot_when_dropped_early() {
        let slots = Arc::new(Semaphore::new(1));
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        let body = stream::pending::<reqwest::Result<Bytes>>();
        let guarded = guard_download(body, slot, secs(60));
        assert_eq!(slots.available_permits(), 0);
        drop(guarded);
        assert_eq!(slots.available_permits(), 1);
    }

    #[test]
    fn checksums_are_sent_as_uppercase_hex() {
        assert_eq!(hex_upper(&[0x00, 0x0a, 0xff, 0x5c]), "000AFF5C");
        assert_eq!(hex_upper(&[]), "");
    }

    #[tokio::test]
    async fn upload_bodies_are_fed_in_pieces_that_add_up() {
        let data: Vec<u8> = (0..BODY_CHUNK * 2 + 5).map(|i| (i % 251) as u8).collect();
        let pieces: Vec<Bytes> = chunked(Bytes::from(data.clone()))
            .map(Result::unwrap)
            .collect()
            .await;
        let sizes: Vec<usize> = pieces.iter().map(Bytes::len).collect();
        assert_eq!(sizes, [BODY_CHUNK, BODY_CHUNK, 5]);
        assert_eq!(pieces.concat(), data);
        assert_eq!(chunked(Bytes::new()).count().await, 0);
    }
}
