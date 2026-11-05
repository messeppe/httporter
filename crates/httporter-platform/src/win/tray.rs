//! The notification-area icon shown while transfers run with the window closed (§8.7.8).

/// What the user picked on the tray icon: a left click is [`TrayEvent::Show`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Show,
    PauseResume,
    Quit,
}

/// The tray icon. Keeps a message-only window and its NOTIFYICONDATAW; dropping it deletes the
/// icon.
#[derive(Debug)]
pub struct Tray {
    _private: (),
}

impl Tray {
    /// Adds the icon (exe icon resource 1) with `tooltip` and a menu of `labels`: Show HTTPorter /
    /// Pause All or Resume All / Quit.
    pub fn new(
        tooltip: &str,
        labels: [&str; 3],
        on_event: Box<dyn Fn(TrayEvent)>,
    ) -> Result<Tray, String> {
        let _ = (tooltip, labels, on_event);
        todo!("T3")
    }

    /// The tooltip, cut to 127 UTF-16 units with "…".
    pub fn set_tooltip(&self, text: &str) {
        let _ = text;
        todo!("T3")
    }

    /// The menu's labels: Show HTTPorter / Pause All|Resume All / Quit.
    pub fn set_labels(&self, labels: [&str; 3]) {
        let _ = labels;
        todo!("T3")
    }
}
