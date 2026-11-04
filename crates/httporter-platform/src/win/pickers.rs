//! Native file and folder pickers (the standard Windows dialogs).
//!
//! Each call blocks the calling thread until the dialog is closed. COM is set up by the
//! dialog library itself, per call.

use std::path::PathBuf;

use rfd::FileDialog;

/// Asks for one or more files. Empty if the user cancelled.
pub fn pick_files(title: &str) -> Vec<PathBuf> {
    FileDialog::new()
        .set_title(title)
        .pick_files()
        .unwrap_or_default()
}

/// Asks for a folder. `None` if the user cancelled.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    FileDialog::new().set_title(title).pick_folder()
}

/// Asks for one file. `filters` are `(name, extensions)` pairs without dots, for example
/// `("Images", &["png", "jpg"])`; none means every file. `None` if the user cancelled.
pub fn pick_file(title: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf> {
    let mut dialog = FileDialog::new().set_title(title);
    for &(name, extensions) in filters {
        dialog = dialog.add_filter(name, extensions);
    }
    dialog.pick_file()
}

/// Asks where to save a file, offering `default_name`. `filters` are as for [`pick_file`]. `None`
/// if the user cancelled.
pub fn pick_save_file(
    title: &str,
    default_name: &str,
    filters: &[(&str, &[&str])],
) -> Option<PathBuf> {
    let mut dialog = FileDialog::new()
        .set_title(title)
        .set_file_name(default_name);
    for &(name, extensions) in filters {
        dialog = dialog.add_filter(name, extensions);
    }
    dialog.save_file()
}
