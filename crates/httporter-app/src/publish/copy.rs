//! The words of Publish: everything it shows that Rust builds. One section per task, so that
//! parallel inserts merge: add below your own header only.

// ---- compare (T6) ----

use httporter_core::{ProviderKind, SiteRecord};
use httporter_transfer::{CompareError, ScanProgress};

use crate::format::format_count;

/// The site's side of a compare, as "Only on {}" and "{n} only on {}" name it: Bunny for a Bunny
/// site, the site's own name for an S3 one.
pub fn remote_label(site: &SiteRecord) -> String {
    match site.provider {
        ProviderKind::Bunny => crate::copy::BUNNY.to_owned(),
        ProviderKind::S3 => site.name.clone(),
    }
}

/// The status bar while a compare starts.
pub const COMPARING: &str = "Comparing…";

/// The status bar while a compare runs: the files listed so far on both sides, then the files
/// whose contents are being checked.
pub fn compare_progress(progress: ScanProgress) -> String {
    let listed = progress.local_files + progress.remote_files;
    if progress.to_hash > 0 {
        format!(
            "Comparing… checked {} of {} files",
            format_count(progress.hashed),
            format_count(progress.to_hash)
        )
    } else if listed > 0 {
        format!("Comparing… {} files listed", format_count(listed))
    } else {
        COMPARING.to_owned()
    }
}

/// Why a compare stopped, for the status bar.
pub fn compare_failed(error: &CompareError) -> String {
    match error {
        CompareError::Local { path, message } => {
            format!(
                "Couldn't compare: {} can't be read ({message})",
                path.display()
            )
        }
        CompareError::Remote(error) => format!(
            "Couldn't compare: {}",
            crate::copy::describe_transfer_error(error)
        ),
    }
}

//
//
//
//
//

// ---- sheet (T7) ----
//
//
//
//
//

// ---- profile (T8) ----
//
//
//
//
//
