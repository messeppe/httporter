//! HTTP-level tests of S3 multipart uploads against a fake service on a local mock server.
//! Nothing here contacts a real service.
//!
//! The fake ([`Fake`]) answers `CreateMultipartUpload`, `UploadPart`, `CompleteMultipartUpload`
//! and `AbortMultipartUpload` for one object, and writes down what it was asked and in which
//! order, so a test can say what the provider did. Every request must be validly signed
//! ([`Signed`]), and every part that arrives is compared with the bytes of the file it should
//! have been cut from. Anything else (a `ListParts`, a single `PutObject`) is noted as unexpected
//! and fails the test through [`Fake::check`].
//!
//! Every test uploads the same file of 70 MiB, which is more than the 64 MiB threshold and cuts
//! into 4 parts of 16 MiB and one of 6 MiB (or 8 of 8 MiB and one of 6 MiB).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use httporter_core::{
    BodyMeter, MultipartProgress, PartJournal, ProviderError, S3Preset, StorageProvider,
    UploadOptions, UploadSource,
};
use httporter_s3::{MULTIPART_THRESHOLD, PART_SIZE, S3Storage, part_size_for};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod common;

use common::*;

const MIB: u64 = 1 << 20;
const FILE_LEN: u64 = 70 * MIB;
const OBJECT: &str = "video/big.bin";
const OBJECT_PATH: &str = "/site/video/big.bin";
/// An upload id with the characters that need care in a query string (`+`, `/` and `=`).
const UPLOAD_ID: &str = "VXBsb2Fk+SUQ/Zm9y=.part~one_two-three";
const STALE_ID: &str = "c3RhbGUgdXBsb2Fk";

// ---------------------------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------------------------

/// The file every test uploads. It is large, so it is made once for the whole run, in the
/// target directory (`cargo clean` removes it), and only read afterwards.
fn big_file() -> &'static Path {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    FILE.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let path = dir.join("multipart-70mib.bin");
        let right_size = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() == FILE_LEN);
        if !right_size(&path) {
            // Written under another name and moved into place, so that a second process
            // never sees half a file.
            let partial = dir.join(format!("multipart-70mib.{}.tmp", std::process::id()));
            write_pattern(&partial);
            if std::fs::rename(&partial, &path).is_err() {
                let _ = std::fs::remove_file(&partial);
                assert!(right_size(&path), "could not make the test file");
            }
        }
        path
    })
}

/// [`FILE_LEN`] bytes of noise with the number of each MiB stamped on its start, so that no two
/// parts (and no two pieces of a part) are alike.
fn write_pattern(path: &Path) {
    let mut block = vec![0u8; MIB as usize];
    let mut x: u32 = 0x9E37_79B9;
    for byte in &mut block {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = (x >> 24) as u8;
    }
    let mut out = BufWriter::new(File::create(path).unwrap());
    let mut left = FILE_LEN;
    let mut index = 0u64;
    while left > 0 {
        block[..8].copy_from_slice(&index.to_le_bytes());
        let size = left.min(MIB);
        out.write_all(&block[..size as usize]).unwrap();
        left -= size;
        index += 1;
    }
    out.flush().unwrap();
}

fn read_range(path: &Path, offset: u64, len: u64) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = vec![0u8; len as usize];
    file.read_exact(&mut bytes).unwrap();
    bytes
}

fn source() -> UploadSource {
    UploadSource::File {
        path: big_file().to_path_buf(),
        len: FILE_LEN,
    }
}

// ---------------------------------------------------------------------------------------------
// The part journal
// ---------------------------------------------------------------------------------------------

/// What the provider told the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Note {
    Started { upload_id: String, part_size: u64 },
    PartDone { number: u32, etag: String },
    Finished,
}

/// A [`PartJournal`] that remembers an earlier attempt (or none), says whether the user
/// cancelled, and writes down everything it is told.
struct RecordingJournal {
    earlier: Option<MultipartProgress>,
    cancelled: AtomicBool,
    notes: Mutex<Vec<Note>>,
}

impl RecordingJournal {
    fn fresh() -> Arc<Self> {
        Self::with(None)
    }

    fn resuming(earlier: MultipartProgress) -> Arc<Self> {
        Self::with(Some(earlier))
    }

    fn with(earlier: Option<MultipartProgress>) -> Arc<Self> {
        Arc::new(Self {
            earlier,
            cancelled: AtomicBool::new(false),
            notes: Mutex::new(Vec::new()),
        })
    }

    /// The user cancels the upload.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn notes(&self) -> Vec<Note> {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn note(&self, note: Note) {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(note);
    }

    /// The numbers of the parts recorded as stored, in order.
    fn recorded(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .notes()
            .iter()
            .filter_map(|note| match note {
                Note::PartDone { number, .. } => Some(*number),
                _ => None,
            })
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// What a real journal would hand the next attempt: the upload and the parts recorded.
    fn progress(&self) -> Option<MultipartProgress> {
        let notes = self.notes();
        let (upload_id, part_size) = notes.iter().find_map(|note| match note {
            Note::Started {
                upload_id,
                part_size,
            } => Some((upload_id.clone(), *part_size)),
            _ => None,
        })?;
        let parts = notes
            .iter()
            .filter_map(|note| match note {
                Note::PartDone { number, etag } => Some((*number, etag.clone())),
                _ => None,
            })
            .collect();
        Some(MultipartProgress {
            upload_id,
            part_size,
            parts,
        })
    }
}

impl PartJournal for RecordingJournal {
    fn load(&self) -> Option<MultipartProgress> {
        self.earlier.clone()
    }

    fn started(&self, upload_id: &str, part_size: u64) {
        self.note(Note::Started {
            upload_id: upload_id.to_owned(),
            part_size,
        });
    }

    fn part_done(&self, number: u32, etag: &str) {
        self.note(Note::PartDone {
            number,
            etag: etag.to_owned(),
        });
    }

    fn finished(&self) {
        self.note(Note::Finished);
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// An earlier attempt that stored the parts `recorded` of `upload_id`.
fn progress(upload_id: &str, part_size: u64, recorded: &[u32]) -> MultipartProgress {
    MultipartProgress {
        upload_id: upload_id.to_owned(),
        part_size,
        parts: recorded
            .iter()
            .map(|&number| (number, recorded_etag(number)))
            .collect(),
    }
}

/// The ETag the journal of an earlier attempt holds for a part.
fn recorded_etag(number: u32) -> String {
    format!("\"recorded-{number}\"")
}

/// Options that keep the upload's progress in `journal`.
fn journalled(journal: &Arc<RecordingJournal>) -> UploadOptions {
    UploadOptions {
        parts: Some(journal.clone()),
        ..opts()
    }
}

/// Counts what the meter is told.
#[derive(Default)]
struct CountingMeter {
    calls: AtomicU64,
    bytes: AtomicU64,
}

#[async_trait]
impl BodyMeter for CountingMeter {
    async fn chunk(&self, len: usize) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bytes.fetch_add(len as u64, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------------------------
// The fake service
// ---------------------------------------------------------------------------------------------

/// One `UploadPart` the fake received.
#[derive(Debug, Clone)]
struct PartCall {
    id: String,
    number: u32,
    len: u64,
    /// The body is the bytes the file has at that part's place.
    body_ok: bool,
    /// `Content-MD5` is the MD5 of the body; `None` when the fake was not asked to check.
    md5_ok: Option<bool>,
    /// The ETag the fake gave the part; `None` when it did not store it.
    etag: Option<String>,
    at: Instant,
}

#[derive(Debug, Clone)]
enum Call {
    Create,
    Part(PartCall),
    Complete {
        id: String,
        parts: Vec<(u32, String)>,
    },
    Abort {
        id: String,
    },
    /// Not one of the four requests of a multipart upload.
    Other(String),
}

#[derive(Debug, Clone)]
struct Seen {
    call: Call,
    /// The request's headers, names in lower case.
    headers: HashMap<String, String>,
}

/// How the fake answers `CompleteMultipartUpload`.
#[derive(Clone, Copy)]
enum Completion {
    Done,
    /// `200`, and an `<Error>` where the result should be, which S3 does when it fails late.
    FailsAfterOk(&'static str),
}

struct Fake {
    /// The size the parts of the upload are expected to have.
    part_size: u64,
    /// The id the fake gives a new upload.
    create_id: String,
    /// Uploads the service no longer has: every request about them is `NoSuchUpload`.
    gone: HashSet<String>,
    /// How long the answer to a part is held back.
    part_delay: Duration,
    /// Parts that fail, with the status and code of their error.
    failing: HashMap<u32, (u16, &'static str)>,
    completion: Completion,
    check_md5: bool,
    seen: Mutex<Vec<Seen>>,
}

impl Fake {
    fn new(part_size: u64) -> Self {
        Self {
            part_size,
            create_id: UPLOAD_ID.to_owned(),
            gone: HashSet::new(),
            part_delay: Duration::ZERO,
            failing: HashMap::new(),
            completion: Completion::Done,
            check_md5: false,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// New uploads get this id.
    fn creating(mut self, id: &str) -> Self {
        self.create_id = id.to_owned();
        self
    }

    /// The service has lost the upload `id`.
    fn without(mut self, id: &str) -> Self {
        self.gone.insert(id.to_owned());
        self
    }

    fn holding_parts_for(mut self, delay: Duration) -> Self {
        self.part_delay = delay;
        self
    }

    fn failing_part(mut self, number: u32, status: u16, code: &'static str) -> Self {
        self.failing.insert(number, (status, code));
        self
    }

    fn completing(mut self, completion: Completion) -> Self {
        self.completion = completion;
        self
    }

    /// Hashes every part body, to compare with its `Content-MD5` (slow in a debug build).
    fn checking_md5(mut self) -> Self {
        self.check_md5 = true;
        self
    }

    async fn serve(self, server: &MockServer) -> Arc<Self> {
        let fake = Arc::new(self);
        Mock::given(Signed::new())
            .respond_with(Handler(fake.clone()))
            .mount(server)
            .await;
        fake
    }

    fn answer(&self, request: &Request) -> ResponseTemplate {
        let query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        let find = |name: &str| {
            query
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let (call, response) = if request.url.path() != OBJECT_PATH {
            (
                Call::Other(format!("{} {}", request.method, request.url)),
                s3_error(400, "InvalidRequest", "not the object of the test"),
            )
        } else {
            match (
                request.method.as_str(),
                find("uploads").is_some(),
                find("uploadId"),
                find("partNumber"),
            ) {
                ("POST", true, None, None) => (
                    Call::Create,
                    xml(format!(
                        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                         <InitiateMultipartUploadResult \
                         xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                         <Bucket>{BUCKET}</Bucket><Key>{OBJECT}</Key>\
                         <UploadId>{}</UploadId></InitiateMultipartUploadResult>",
                        self.create_id
                    )),
                ),
                ("PUT", false, Some(id), Some(number)) => self.part(&id, &number, request),
                ("POST", false, Some(id), None) => self.complete(id, request),
                ("DELETE", false, Some(id), None) => self.abort(id),
                _ => (
                    Call::Other(format!("{} {}", request.method, request.url)),
                    s3_error(400, "InvalidRequest", "not part of a multipart upload"),
                ),
            }
        };
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen {
                call,
                headers: header_map(request),
            });
        response
    }

    fn part(&self, id: &str, number: &str, request: &Request) -> (Call, ResponseTemplate) {
        let at = Instant::now();
        let total = FILE_LEN.div_ceil(self.part_size);
        let Some(number) = number
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=total).contains(&u64::from(*n)))
        else {
            return (
                Call::Other(format!("part number {number}")),
                s3_error(400, "InvalidArgument", "no such part number"),
            );
        };
        let len = request.body.len() as u64;
        let called = |body_ok, md5_ok, etag| {
            Call::Part(PartCall {
                id: id.to_owned(),
                number,
                len,
                body_ok,
                md5_ok,
                etag,
                at,
            })
        };
        if self.gone.contains(id) {
            // Held back, so that the parts sent beside this one have arrived whole by the time
            // the provider gives up on the upload and drops them: a body that is cut short
            // makes the mock server panic on a thread of its own, and spoil the test output.
            return (
                called(true, None, None),
                no_such_upload().set_delay(Duration::from_millis(500)),
            );
        }
        if let Some((status, code)) = self.failing.get(&number) {
            return (
                called(true, None, None),
                s3_error(*status, code, "injected"),
            );
        }
        let offset = (u64::from(number) - 1) * self.part_size;
        let expected = read_range(big_file(), offset, self.part_size.min(FILE_LEN - offset));
        let body_ok = request.body == expected;
        let md5_ok = self.check_md5.then(|| {
            header_of(request, "content-md5") == Some(b64(&md5_of(&request.body)).as_str())
        });
        let etag = etag_of(id, number);
        (
            called(body_ok, md5_ok, Some(etag.clone())),
            ResponseTemplate::new(200)
                .insert_header("etag", etag)
                .set_delay(self.part_delay),
        )
    }

    fn complete(&self, id: String, request: &Request) -> (Call, ResponseTemplate) {
        let parts = listed_parts(&request.body);
        let response = if self.gone.contains(&id) {
            no_such_upload()
        } else {
            match self.completion {
                Completion::Done => xml(format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                     <CompleteMultipartUploadResult \
                     xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                     <Location>http://s3.example/{BUCKET}/{OBJECT}</Location>\
                     <Bucket>{BUCKET}</Bucket><Key>{OBJECT}</Key>\
                     <ETag>&quot;3858f62230ac3c915f300c664312c63f-5&quot;</ETag>\
                     </CompleteMultipartUploadResult>"
                )),
                Completion::FailsAfterOk(code) => xml(format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
                     <Message>injected</Message><RequestId>4442587FB7D0A2F9</RequestId></Error>"
                )),
            }
        };
        (Call::Complete { id, parts }, response)
    }

    fn abort(&self, id: String) -> (Call, ResponseTemplate) {
        let response = if self.gone.contains(&id) {
            no_such_upload()
        } else {
            ResponseTemplate::new(204)
        };
        (Call::Abort { id }, response)
    }

    // -----------------------------------------------------------------------------------------
    // What happened
    // -----------------------------------------------------------------------------------------

    fn log(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The requests in the order they arrived: `create`, `part`, `complete`, `abort`, `other`.
    fn trace(&self) -> Vec<&'static str> {
        self.log()
            .iter()
            .map(|seen| match seen.call {
                Call::Create => "create",
                Call::Part(_) => "part",
                Call::Complete { .. } => "complete",
                Call::Abort { .. } => "abort",
                Call::Other(_) => "other",
            })
            .collect()
    }

    fn part_calls(&self) -> Vec<PartCall> {
        self.log()
            .into_iter()
            .filter_map(|seen| match seen.call {
                Call::Part(part) => Some(part),
                _ => None,
            })
            .collect()
    }

    /// The parts asked of the upload `id`, stored or not, in the order they arrived.
    fn parts_to(&self, id: &str) -> Vec<PartCall> {
        let mut parts = self.part_calls();
        parts.retain(|part| part.id == id);
        parts
    }

    /// The numbers of the parts asked of the upload `id`, in order of number.
    fn part_numbers(&self, id: &str) -> Vec<u32> {
        let mut numbers: Vec<u32> = self.parts_to(id).iter().map(|part| part.number).collect();
        numbers.sort_unstable();
        numbers
    }

    /// The numbers of the parts the fake stored, in order of number.
    fn stored_numbers(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .part_calls()
            .iter()
            .filter(|part| part.etag.is_some())
            .map(|part| part.number)
            .collect();
        numbers.sort_unstable();
        numbers
    }

    fn creates(&self) -> usize {
        self.trace()
            .iter()
            .filter(|kind| **kind == "create")
            .count()
    }

    /// The upload and the parts of every `CompleteMultipartUpload`.
    fn completes(&self) -> Vec<(String, Vec<(u32, String)>)> {
        self.log()
            .into_iter()
            .filter_map(|seen| match seen.call {
                Call::Complete { id, parts } => Some((id, parts)),
                _ => None,
            })
            .collect()
    }

    /// The uploads asked to be aborted.
    fn aborts(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter_map(|seen| match seen.call {
                Call::Abort { id } => Some(id),
                _ => None,
            })
            .collect()
    }

    /// The most parts that had been asked of the fake and not yet answered at once.
    fn peak_in_flight(&self) -> usize {
        let starts: Vec<Instant> = self.part_calls().iter().map(|part| part.at).collect();
        starts
            .iter()
            .map(|&start| {
                starts
                    .iter()
                    .filter(|&&other| other >= start && other < start + self.part_delay)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    /// Fails the test if the provider asked for anything that is not part of a multipart upload
    /// (a `ListParts`, a single `PutObject`), or sent a part that is not the file's bytes.
    fn check(&self) {
        for seen in self.log() {
            match &seen.call {
                Call::Other(what) => panic!("the provider sent a request it should not: {what}"),
                Call::Part(part) => {
                    assert!(part.body_ok, "part {} is not the file's bytes", part.number);
                    assert_ne!(
                        part.md5_ok,
                        Some(false),
                        "part {} has a wrong MD5",
                        part.number
                    );
                }
                _ => {}
            }
        }
    }
}

struct Handler(Arc<Fake>);

impl Respond for Handler {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.answer(request)
    }
}

fn no_such_upload() -> ResponseTemplate {
    s3_error(404, "NoSuchUpload", "The specified upload does not exist.")
}

/// What the fake gives a stored part as its ETag.
fn etag_of(id: &str, number: u32) -> String {
    format!("\"{id}/{number}\"")
}

fn header_map(request: &Request) -> HashMap<String, String> {
    request
        .headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

/// The parts a `CompleteMultipartUpload` body lists, in the order it lists them.
fn listed_parts(body: &[u8]) -> Vec<(u32, String)> {
    fn between<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
        let start = text.find(&format!("<{tag}>"))? + tag.len() + 2;
        let end = text.find(&format!("</{tag}>"))?;
        text.get(start..end)
    }
    String::from_utf8_lossy(body)
        .split("<Part>")
        .skip(1)
        .filter_map(|part| {
            Some((
                between(part, "PartNumber")?.parse().ok()?,
                between(part, "ETag")?.to_owned(),
            ))
        })
        .collect()
}

/// A server that keeps nothing but what the fake writes down: it would otherwise hold on to
/// every part body it is sent, which is 70 MiB per test.
async fn start() -> MockServer {
    MockServer::builder()
        .disable_request_recording()
        .start()
        .await
}

async fn upload(storage: &S3Storage, options: &UploadOptions) -> Result<(), ProviderError> {
    storage.upload(&p(OBJECT), source(), options).await
}

/// Waits until `done` is true.
async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Starts an upload whose parts the fake keeps waiting, lets it get as far as having four parts
/// on the way (all that fit, each one received whole: a body cut short makes the mock server
/// panic on a thread of its own), and drops it, as the engine does to pause or cancel a transfer.
/// `cancel` marks the journal cancelled first, as the engine does when the user cancels.
async fn interrupt(
    storage: &S3Storage,
    options: &UploadOptions,
    fake: &Fake,
    journal: &RecordingJournal,
    cancel: bool,
) {
    let upload = upload(storage, options);
    tokio::pin!(upload);
    tokio::select! {
        result = &mut upload => panic!("the upload ended by itself: {result:?}"),
        () = eventually("four parts to be on their way", || {
            fake.parts_to(UPLOAD_ID).len() >= 4
        }) => {}
    }
    if cancel {
        journal.cancel();
    }
    // `upload` is dropped here, half way.
}

fn numbers(parts: &[(u32, String)]) -> Vec<u32> {
    parts.iter().map(|(number, _)| *number).collect()
}

// ---------------------------------------------------------------------------------------------
// The brief's tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn large_file_uses_create_parts_complete() {
    let server = start().await;
    let fake = Fake::new(16 * MIB).checking_md5().serve(&server).await;
    let journal = RecordingJournal::fresh();
    let meter = Arc::new(CountingMeter::default());
    let options = UploadOptions {
        meter: Some(meter.clone()),
        parts: Some(journal.clone()),
        ..UploadOptions::new("video/mp4")
    };

    upload(&provider(&server), &options).await.unwrap();

    // One Create, the five parts, one Complete in the end: no single PutObject, no ListParts
    // and no abort in between.
    fake.check();
    let mut trace = vec!["create"];
    trace.extend(["part"; 5]);
    trace.push("complete");
    assert_eq!(fake.trace(), trace);

    let log = fake.log();
    assert_eq!(log[0].headers["content-type"], "video/mp4");
    assert!(
        !log[0].headers.contains_key("content-md5"),
        "only the parts carry a digest"
    );

    // 70 MiB is 4 parts of 16 MiB and one of 6 MiB, each sent with the MD5 of its own bytes.
    let parts = fake.parts_to(UPLOAD_ID);
    assert_eq!(fake.part_numbers(UPLOAD_ID), [1, 2, 3, 4, 5]);
    for part in &parts {
        let expected = if part.number < 5 { 16 * MIB } else { 6 * MIB };
        assert_eq!(part.len, expected, "the size of part {}", part.number);
        assert_eq!(
            part.md5_ok,
            Some(true),
            "Content-MD5 of part {}",
            part.number
        );
    }
    for seen in &log[1..6] {
        assert!(seen.headers.contains_key("content-md5"));
    }

    // Complete lists the five parts in order, with the ETags the service gave them.
    let listed: Vec<(u32, String)> = (1..=5).map(|n| (n, etag_of(UPLOAD_ID, n))).collect();
    assert_eq!(fake.completes(), [(UPLOAD_ID.to_owned(), listed.clone())]);
    assert_eq!(log[6].headers["content-type"], "application/xml");

    // The journal heard of the upload, then of each part, then that it was done.
    let notes = journal.notes();
    assert_eq!(notes.len(), 7, "{notes:?}");
    assert_eq!(
        notes[0],
        Note::Started {
            upload_id: UPLOAD_ID.to_owned(),
            part_size: 16 * MIB
        }
    );
    assert_eq!(notes[6], Note::Finished);
    let mut done: Vec<(u32, String)> = journal.progress().unwrap().parts;
    done.sort();
    assert_eq!(done, listed);

    // Every byte went through the meter, in pieces.
    assert_eq!(meter.bytes.load(Ordering::SeqCst), FILE_LEN);
    assert!(
        meter.calls.load(Ordering::SeqCst) >= FILE_LEN / (64 << 10),
        "pieces of 64 KiB at most"
    );
}

#[tokio::test]
async fn at_most_four_parts_in_flight() {
    let server = start().await;
    let fake = Fake::new(5 * MIB)
        .holding_parts_for(Duration::from_millis(500))
        .serve(&server)
        .await;
    let mut cfg = config(&server);
    cfg.part_size = 5 * MIB;

    upload(&S3Storage::new(cfg).unwrap(), &opts())
        .await
        .unwrap();

    fake.check();
    // 70 MiB in parts of 5 MiB is 14 of them: more than can be on the way at once.
    assert_eq!(fake.part_numbers(UPLOAD_ID), (1..=14).collect::<Vec<_>>());
    assert_eq!(
        fake.peak_in_flight(),
        4,
        "four at once, and never a fifth beside them"
    );
}

#[tokio::test]
async fn resume_skips_recorded_parts() {
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .creating("must-not-be-created")
        .serve(&server)
        .await;
    let journal = RecordingJournal::resuming(progress(UPLOAD_ID, 16 * MIB, &[1, 2, 3]));

    upload(&provider(&server), &journalled(&journal))
        .await
        .unwrap();

    // The journal is believed: the upload is not created again, the service is not asked what
    // it has stored (no ListParts, which `check` would refuse), and only parts 4 and 5 are sent.
    fake.check();
    assert_eq!(fake.trace(), ["part", "part", "complete"]);
    assert_eq!(fake.creates(), 0);
    assert_eq!(fake.part_numbers(UPLOAD_ID), [4, 5]);

    // Complete lists all five, the recorded ones with the ETags the journal holds.
    let listed = vec![
        (1, recorded_etag(1)),
        (2, recorded_etag(2)),
        (3, recorded_etag(3)),
        (4, etag_of(UPLOAD_ID, 4)),
        (5, etag_of(UPLOAD_ID, 5)),
    ];
    assert_eq!(fake.completes(), [(UPLOAD_ID.to_owned(), listed)]);

    // The journal hears of the new parts and of the end, and of no new upload.
    let notes = journal.notes();
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert_eq!(journal.recorded(), [4, 5]);
    assert_eq!(notes[2], Note::Finished);
}

#[tokio::test]
async fn no_such_upload_on_resume_starts_over() {
    let server = start().await;
    let fake = Fake::new(16 * MIB).without(STALE_ID).serve(&server).await;
    let journal = RecordingJournal::resuming(progress(STALE_ID, 16 * MIB, &[1, 2, 3]));

    upload(&provider(&server), &journalled(&journal))
        .await
        .unwrap();

    fake.check();
    // The recorded upload was tried (only for the parts it still needed), found gone, left
    // alone (there is nothing to abort), and the file was uploaded again as a new one.
    assert!(
        fake.part_numbers(STALE_ID)
            .iter()
            .all(|number| [4, 5].contains(number)),
        "{:?}",
        fake.part_numbers(STALE_ID)
    );
    assert!(!fake.part_numbers(STALE_ID).is_empty());
    assert_eq!(fake.creates(), 1);
    assert_eq!(fake.part_numbers(UPLOAD_ID), [1, 2, 3, 4, 5]);
    assert!(fake.aborts().is_empty(), "{:?}", fake.aborts());
    let listed: Vec<(u32, String)> = (1..=5).map(|n| (n, etag_of(UPLOAD_ID, n))).collect();
    assert_eq!(
        fake.completes(),
        [(UPLOAD_ID.to_owned(), listed)],
        "only the new upload is completed, with none of the recorded ETags"
    );

    // The journal now holds the new upload and nothing of the old one.
    let notes = journal.notes();
    assert_eq!(
        notes[0],
        Note::Started {
            upload_id: UPLOAD_ID.to_owned(),
            part_size: 16 * MIB
        }
    );
    assert_eq!(journal.recorded(), [1, 2, 3, 4, 5]);
    assert_eq!(notes.last(), Some(&Note::Finished));
}

#[tokio::test]
async fn cancelled_aborts_the_upload() {
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .holding_parts_for(Duration::from_secs(5))
        .serve(&server)
        .await;
    let journal = RecordingJournal::fresh();

    interrupt(
        &provider(&server),
        &journalled(&journal),
        &fake,
        &journal,
        true,
    )
    .await;

    // The future was dropped, and the abort is sent by a task it leaves behind.
    eventually("the abort", || !fake.aborts().is_empty()).await;
    assert_eq!(fake.aborts(), [UPLOAD_ID]);
    assert!(fake.completes().is_empty());
    assert!(!journal.notes().contains(&Note::Finished));
    fake.check();
}

#[tokio::test]
async fn dropped_without_cancel_keeps_the_upload() {
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .holding_parts_for(Duration::from_secs(5))
        .serve(&server)
        .await;
    let journal = RecordingJournal::fresh();

    interrupt(
        &provider(&server),
        &journalled(&journal),
        &fake,
        &journal,
        false,
    )
    .await;

    // A pause or a shutdown drops the upload the same way, and the upload is left for a resume.
    // (`cancelled_aborts_the_upload` shows that an abort would have arrived by now.)
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(fake.aborts().is_empty(), "{:?}", fake.aborts());
    assert!(fake.completes().is_empty());
    let notes = journal.notes();
    assert!(
        matches!(&notes[0], Note::Started { upload_id, .. } if upload_id == UPLOAD_ID),
        "{notes:?}"
    );
    assert!(!notes.contains(&Note::Finished));
    fake.check();
}

#[tokio::test]
async fn sha256_meta_set_on_create() {
    let server = start().await;
    let fake = Fake::new(16 * MIB).serve(&server).await;
    let sha = sha256_of(b"the whole file");
    // A digest of the whole file cannot be checked by a service that gets it in parts, so the
    // MD5 option has no place on any of the requests: each part carries the MD5 of its own bytes.
    let whole_file_md5 = b64(&md5_of(b"the whole file"));
    let options = UploadOptions {
        sha256: Some(sha),
        md5: Some(md5_of(b"the whole file")),
        ..opts()
    };

    upload(&provider(&server), &options).await.unwrap();

    fake.check();
    let log = fake.log();
    let meta = "x-amz-meta-httporter-sha256";
    assert_eq!(log[0].headers[meta], hex::encode(sha));
    assert!(matches!(log[0].call, Call::Create));
    for seen in &log[1..] {
        assert!(
            !seen.headers.contains_key(meta),
            "{:?} carries the metadata too",
            seen.call
        );
    }
    for seen in &log {
        assert_ne!(
            seen.headers.get("content-md5"),
            Some(&whole_file_md5),
            "{:?}",
            seen.call
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Ruling Q25: the size of a part
// ---------------------------------------------------------------------------------------------

#[test]
fn part_size_is_8_mib_for_digitalocean_spaces_and_16_mib_for_every_other_preset() {
    assert_eq!(PART_SIZE, 16 * MIB);
    assert_eq!(MULTIPART_THRESHOLD, 64 * MIB);

    // Every preset, so that a new one has to be given a place here.
    let presets = [
        S3Preset::AwsS3,
        S3Preset::CloudflareR2,
        S3Preset::BackblazeB2,
        S3Preset::Wasabi,
        S3Preset::DigitalOceanSpaces,
        S3Preset::Minio,
        S3Preset::Other,
    ];
    for preset in presets {
        match preset {
            S3Preset::DigitalOceanSpaces => assert_eq!(part_size_for(preset), 8 * MIB),
            S3Preset::AwsS3
            | S3Preset::CloudflareR2
            | S3Preset::BackblazeB2
            | S3Preset::Wasabi
            | S3Preset::Minio
            | S3Preset::Other => {
                assert_eq!(part_size_for(preset), 16 * MIB, "{preset:?}");
            }
        }
    }
}

#[tokio::test]
async fn a_new_upload_cuts_parts_of_the_configured_size() {
    let server = start().await;
    let fake = Fake::new(8 * MIB).serve(&server).await;
    let journal = RecordingJournal::fresh();
    let mut cfg = config(&server);
    cfg.part_size = part_size_for(S3Preset::DigitalOceanSpaces);

    upload(&S3Storage::new(cfg).unwrap(), &journalled(&journal))
        .await
        .unwrap();

    // 70 MiB is eight parts of 8 MiB and one of 6 MiB.
    fake.check();
    assert_eq!(fake.part_numbers(UPLOAD_ID), (1..=9).collect::<Vec<_>>());
    for part in fake.parts_to(UPLOAD_ID) {
        let expected = if part.number < 9 { 8 * MIB } else { 6 * MIB };
        assert_eq!(part.len, expected, "the size of part {}", part.number);
    }
    assert_eq!(numbers(&fake.completes()[0].1), (1..=9).collect::<Vec<_>>());
    assert_eq!(
        journal.notes()[0],
        Note::Started {
            upload_id: UPLOAD_ID.to_owned(),
            part_size: 8 * MIB
        }
    );
}

#[tokio::test]
async fn a_configured_part_size_below_the_minimum_is_raised_to_5_mib() {
    let server = start().await;
    let fake = Fake::new(5 * MIB).serve(&server).await;
    let journal = RecordingJournal::fresh();
    let mut cfg = config(&server);
    cfg.part_size = MIB;

    upload(&S3Storage::new(cfg).unwrap(), &journalled(&journal))
        .await
        .unwrap();

    // Services refuse parts under 5 MiB (the last one apart).
    fake.check();
    assert_eq!(fake.part_numbers(UPLOAD_ID), (1..=14).collect::<Vec<_>>());
    assert_eq!(
        journal.notes()[0],
        Note::Started {
            upload_id: UPLOAD_ID.to_owned(),
            part_size: 5 * MIB
        }
    );
}

/// Resumes an upload that began with parts of `journalled_size` while the configuration now says
/// `configured_size`, recording the parts `recorded`; answers what the fake saw.
async fn resume_with_part_sizes(
    journalled_size: u64,
    configured_size: u64,
    recorded: &[u32],
) -> (Arc<Fake>, Arc<RecordingJournal>) {
    let server = start().await;
    // The fake expects the parts at the size the journal remembers.
    let fake = Fake::new(journalled_size).serve(&server).await;
    let journal = RecordingJournal::resuming(progress(UPLOAD_ID, journalled_size, recorded));
    let mut cfg = config(&server);
    cfg.part_size = configured_size;

    upload(&S3Storage::new(cfg).unwrap(), &journalled(&journal))
        .await
        .unwrap();
    (fake, journal)
}

#[tokio::test]
async fn a_resumed_upload_keeps_its_journalled_part_size_when_the_configuration_asks_for_more() {
    // It began as a DigitalOcean Spaces upload (8 MiB) and the configuration now says 16 MiB.
    let (fake, journal) = resume_with_part_sizes(8 * MIB, 16 * MIB, &[1, 2]).await;

    fake.check();
    assert_eq!(fake.creates(), 0);
    assert_eq!(fake.part_numbers(UPLOAD_ID), (3..=9).collect::<Vec<_>>());
    for part in fake.parts_to(UPLOAD_ID) {
        let expected = if part.number < 9 { 8 * MIB } else { 6 * MIB };
        assert_eq!(part.len, expected, "the size of part {}", part.number);
    }
    assert_eq!(numbers(&fake.completes()[0].1), (1..=9).collect::<Vec<_>>());
    assert_eq!(journal.recorded(), (3..=9).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_resumed_upload_keeps_its_journalled_part_size_when_the_configuration_asks_for_less() {
    let (fake, journal) = resume_with_part_sizes(16 * MIB, 8 * MIB, &[1, 2, 3]).await;

    fake.check();
    assert_eq!(fake.creates(), 0);
    assert_eq!(fake.part_numbers(UPLOAD_ID), [4, 5]);
    assert_eq!(numbers(&fake.completes()[0].1), [1, 2, 3, 4, 5]);
    assert_eq!(journal.recorded(), [4, 5]);
}

// ---------------------------------------------------------------------------------------------
// Failing, and what happens to the upload
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_upload_is_kept_for_the_retry_which_resumes_from_what_was_stored() {
    // First attempt: part 2 fails, the user has not cancelled.
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .failing_part(2, 500, "InternalError")
        .serve(&server)
        .await;
    let journal = RecordingJournal::fresh();

    let result = upload(&provider(&server), &journalled(&journal)).await;

    assert!(
        matches!(result, Err(ProviderError::Server { status: 500, .. })),
        "{result:?}"
    );
    fake.check();
    // Nothing is completed or aborted: the parts that were stored stay for the retry, and the
    // journal knows exactly which ones those are.
    assert!(fake.completes().is_empty());
    assert!(fake.aborts().is_empty(), "{:?}", fake.aborts());
    assert!(!journal.notes().contains(&Note::Finished));
    let recorded = journal.recorded();
    assert_eq!(recorded, fake.stored_numbers());
    assert!(!recorded.contains(&2));
    assert!(
        !recorded.is_empty(),
        "the parts beside the failed one finish"
    );

    // The retry gets what the journal holds.
    let server = start().await;
    let retry = Fake::new(16 * MIB).serve(&server).await;
    // (The journal hands back the ETags of the first attempt; they are told apart from the
    // retry's here by giving them the form of `recorded_etag`.)
    let mut earlier = journal.progress().unwrap();
    for (number, etag) in &mut earlier.parts {
        *etag = recorded_etag(*number);
    }
    let journal = RecordingJournal::resuming(earlier);
    upload(&provider(&server), &journalled(&journal))
        .await
        .unwrap();

    retry.check();
    assert_eq!(retry.creates(), 0);
    let missing: Vec<u32> = (1..=5).filter(|n| !recorded.contains(n)).collect();
    assert_eq!(retry.part_numbers(UPLOAD_ID), missing);
    let listed = retry.completes();
    assert_eq!(numbers(&listed[0].1), [1, 2, 3, 4, 5]);
    // The ETags are the journal's for the parts the first attempt stored, and the service's own
    // answer for the ones the retry sent.
    for (number, etag) in &listed[0].1 {
        let expected = if recorded.contains(number) {
            recorded_etag(*number)
        } else {
            etag_of(UPLOAD_ID, *number)
        };
        assert_eq!(etag, &expected, "part {number}");
    }
}

#[tokio::test]
async fn a_failed_upload_the_user_cancelled_is_aborted_before_the_call_returns() {
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .failing_part(2, 500, "InternalError")
        .serve(&server)
        .await;
    let journal = RecordingJournal::fresh();
    journal.cancel();

    let result = upload(&provider(&server), &journalled(&journal)).await;

    assert!(
        matches!(result, Err(ProviderError::Server { status: 500, .. })),
        "{result:?}"
    );
    // The abort was awaited, not left to a task.
    assert_eq!(fake.aborts(), [UPLOAD_ID]);
    assert!(fake.completes().is_empty());
    fake.check();
}

#[tokio::test]
async fn without_a_journal_a_failed_upload_is_aborted() {
    // Nothing could resume it: the stored parts would only cost storage.
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .failing_part(2, 500, "InternalError")
        .serve(&server)
        .await;

    let result = upload(&provider(&server), &opts()).await;

    assert!(
        matches!(result, Err(ProviderError::Server { status: 500, .. })),
        "{result:?}"
    );
    assert_eq!(fake.aborts(), [UPLOAD_ID]);
    assert!(fake.completes().is_empty());
    fake.check();
}

#[tokio::test]
async fn a_complete_that_fails_after_a_200_fails_the_upload_and_keeps_it() {
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .completing(Completion::FailsAfterOk("InternalError"))
        .serve(&server)
        .await;
    let journal = RecordingJournal::fresh();

    let result = upload(&provider(&server), &journalled(&journal)).await;

    // S3 can begin its answer with 200 and fail after that: it is not a success.
    assert!(
        matches!(result, Err(ProviderError::Server { status: 500, .. })),
        "{result:?}"
    );
    fake.check();
    assert_eq!(fake.completes().len(), 1);
    assert!(fake.aborts().is_empty());
    assert_eq!(journal.recorded(), [1, 2, 3, 4, 5]);
    assert!(
        !journal.notes().contains(&Note::Finished),
        "the retry has to complete it"
    );
}

#[tokio::test]
async fn no_such_upload_when_completing_a_fully_recorded_upload_also_starts_over() {
    let server = start().await;
    let fake = Fake::new(16 * MIB).without(STALE_ID).serve(&server).await;
    let journal = RecordingJournal::resuming(progress(STALE_ID, 16 * MIB, &[1, 2, 3, 4, 5]));

    upload(&provider(&server), &journalled(&journal))
        .await
        .unwrap();

    fake.check();
    // Nothing was left to send, so the first request is the Complete the service refuses.
    assert_eq!(fake.part_numbers(STALE_ID), Vec::<u32>::new());
    let completes = fake.completes();
    assert_eq!(completes.len(), 2, "{completes:?}");
    assert_eq!(completes[0].0, STALE_ID);
    assert_eq!(completes[1].0, UPLOAD_ID);
    assert_eq!(fake.part_numbers(UPLOAD_ID), [1, 2, 3, 4, 5]);
    assert!(fake.aborts().is_empty());
    assert_eq!(journal.notes().last(), Some(&Note::Finished));
}

#[tokio::test]
async fn an_upload_that_was_started_over_is_not_started_over_again() {
    // The service loses the new upload as well: that is an error, not a loop.
    let server = start().await;
    let fake = Fake::new(16 * MIB)
        .without(STALE_ID)
        .without(UPLOAD_ID)
        .serve(&server)
        .await;
    let journal = RecordingJournal::resuming(progress(STALE_ID, 16 * MIB, &[1, 2, 3]));

    let result = upload(&provider(&server), &journalled(&journal)).await;

    assert_eq!(result, Err(ProviderError::NotFound));
    assert_eq!(fake.creates(), 1);
    assert!(fake.completes().is_empty());
    assert!(!journal.notes().contains(&Note::Finished));
    fake.check();
}

#[tokio::test]
async fn a_file_that_changed_is_permanent_and_nothing_is_created() {
    let server = start().await;
    let fake = Fake::new(16 * MIB).serve(&server).await;
    let journal = RecordingJournal::fresh();
    let source = UploadSource::File {
        path: big_file().to_path_buf(),
        len: FILE_LEN + 1,
    };

    let result = provider(&server)
        .upload(&p(OBJECT), source, &journalled(&journal))
        .await;

    assert!(
        matches!(result, Err(ProviderError::Permanent(_))),
        "{result:?}"
    );
    assert!(fake.log().is_empty());
    assert!(journal.notes().is_empty());
}
