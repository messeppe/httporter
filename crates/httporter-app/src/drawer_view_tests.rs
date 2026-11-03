//! Tests of the transfer drawer's view. The tests themselves come in the testing phase; what is
//! here is the helper they all build their jobs with.

use httporter_core::{JobId, JobKind, JobView};

/// The one place a `JobView` is built in tests (Ruling Q40). When the core gives `JobView` a
/// `paused` field, this is the only line that has to learn it.
#[allow(dead_code)] // The tests are written in the testing phase.
pub(super) fn job_view(id: u64, kind: JobKind, path: &str, size: u64, done_bytes: u64) -> JobView {
    JobView {
        id: JobId(id),
        kind,
        path: path.to_owned(),
        size,
        done_bytes,
        error: None,
    }
}
