//! Reset HTTPorter (§11): confirming writes the marker `%APPDATA%\HTTPorter\reset-pending`,
//! starts `httporter.exe --after-reset` and quits; the next start wipes everything HTTPorter
//! keeps on this PC before anything opens it.

/// The wipe the marker asks for, before the store opens: waits ≤ 10 s for the single-instance
/// mutex, deletes every credential `keys()` lists, `%APPDATA%\HTTPorter` and
/// `%LOCALAPPDATA%\HTTPorter`, and removes the marker last. On any error the marker stays, and
/// the next start tries again. Without a marker it does nothing.
pub fn run_pending_reset() {
    // T4: the wipe. Until then there is never a marker to act on.
}
