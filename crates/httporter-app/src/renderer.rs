// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which renderer draws the windows (I5, spec §6): Slint's software renderer by default, for the
//! lowest memory use (S7), or the GPU one (FemtoVG on OpenGL) when it is chosen in Settings ›
//! Advanced. The choice applies at the next start, and HTTPorter uses the software renderer
//! whenever the GPU one can't start:
//!
//! - **No OpenGL 2.0** (an old driver, some remote sessions): Slint finds out in
//!   `AppWindow::new()` and quietly makes the window with the software renderer. [`Gpu::check`]
//!   sees it: only a GPU renderer takes a rendering notifier. The setting stays GPU, since
//!   finding this out is safe and quick, and the next start tries again (on another screen or
//!   driver it may work).
//! - **The OpenGL context can't be made** when the event loop starts, or the GPU renderer fails
//!   later: the event loop ends with an error, and a renderer can't be changed in a running app.
//!   [`Gpu::ended`] puts the setting back to software for the next start, and the app ends with
//!   an error that says so.
//! - **A crash or hang in the driver**, which no code can catch: a start with the GPU renderer
//!   stores `gpu_failed` before it makes a window, and clears it once the first frame is drawn.
//!   The next start finds it still set, and takes software ([`select`]).
//!
//! Whenever the GPU renderer couldn't start, Settings › Advanced says so ([`fell_back`]); choosing
//! a renderer there clears `gpu_failed`. A `SLINT_BACKEND` environment variable overrides all of
//! this (`tools/measure.ps1 -Backend` relies on it).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use httporter_core::{UI_STATE_KEY, UiState};
use httporter_platform::{Store, default_db_path};
use slint::{ComponentHandle, PlatformError, RenderingState};

use crate::AppWindow;
use crate::session::Session;

/// Slint's names for its winit backend and its two renderers there.
const BACKEND: &str = "winit";
const SOFTWARE: &str = "software";
const GPU: &str = "femtovg";

/// [`UiState::renderer`] for the GPU renderer; 0 is the software one.
pub const GPU_RENDERER: u8 = 1;

/// This run wanted the GPU renderer and uses the software one.
static FELL_BACK: AtomicBool = AtomicBool::new(false);

/// Whether Settings › Advanced says the GPU renderer couldn't start: this run fell back, or the
/// last start with it failed and no renderer has been chosen since.
pub fn fell_back(state: &UiState) -> bool {
    FELL_BACK.load(Ordering::Relaxed) || state.gpu_failed
}

/// Selects Slint's backend and renderer, before any window exists: the GPU renderer when it is
/// chosen and its last start didn't fail, the software one otherwise. With the GPU renderer the
/// start is marked as not finished yet (see [`Gpu`]). Without a `SLINT_BACKEND` override only.
pub fn select(session: &Session) -> Result<Option<Gpu>, PlatformError> {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return Ok(None);
    }
    let state = session.ui_state();
    let wanted = state.renderer == GPU_RENDERER;
    if wanted && state.gpu_failed {
        // The last start with the GPU renderer never drew its first frame: it crashed, hung or
        // was ended. Software from now on, until the GPU renderer is chosen again.
        FELL_BACK.store(true, Ordering::Relaxed);
        session.save_ui_state(&UiState {
            renderer: 0,
            ..state
        });
    }
    let gpu = (wanted && !state.gpu_failed).then(Gpu::starting);
    // Only the event loop can fail here (no display, say), and winit makes one event loop per
    // process: trying again with the software renderer couldn't help.
    let selected = slint::BackendSelector::new()
        .backend_name(BACKEND.into())
        .renderer_name(if gpu.is_some() { GPU } else { SOFTWARE }.into())
        .select();
    if let (Err(_), Some(gpu)) = (&selected, &gpu) {
        gpu.mark(false, false);
    }
    selected.map(|()| gpu)
}

/// A start with the GPU renderer. Until its first frame is drawn, `gpu_failed` is stored, on a
/// connection of its own to the site database, so that the next start can tell a start that never
/// got there.
pub struct Gpu {
    /// None when the database can't be opened: then nothing is marked.
    store: Rc<Option<Store>>,
    drawn: Rc<Cell<bool>>,
}

impl Gpu {
    /// Marks the start before Slint makes anything with OpenGL.
    fn starting() -> Self {
        let gpu = Self {
            store: Rc::new(Store::open(&default_db_path()).ok()),
            drawn: Rc::default(),
        };
        gpu.mark(true, false);
        gpu
    }

    /// After `AppWindow::new()`: the start, if the window really has the GPU renderer. Without
    /// OpenGL 2.0 Slint gave it the software one, which takes no rendering notifier: then this run
    /// fell back, and the mark is cleared (that was no crash).
    pub fn check(self, ui: &AppWindow) -> Option<Self> {
        let (store, drawn) = (self.store.clone(), self.drawn.clone());
        let notified = ui.window().set_rendering_notifier(move |state, _| {
            if matches!(state, RenderingState::AfterRendering) && !drawn.replace(true) {
                write((*store).as_ref(), false, false);
            }
        });
        if notified.is_err() {
            FELL_BACK.store(true, Ordering::Relaxed);
            self.mark(false, false);
            return None;
        }
        Some(self)
    }

    /// After the event loop. An error ended it (the OpenGL context couldn't be made, or drawing
    /// failed): the setting goes back to software, and the next start says why. Without an error
    /// a mark still set (the window was never drawn) is cleared.
    pub fn ended(&self, failed: bool) {
        if failed {
            self.mark(true, true);
        } else if !self.drawn.get() {
            self.mark(false, false);
        }
    }

    fn mark(&self, gpu_failed: bool, software: bool) {
        write((*self.store).as_ref(), gpu_failed, software);
    }
}

/// Stores `gpu_failed` at once and, with `software`, the setting back to the software renderer.
/// Only those fields are written: every other stays as stored (`Store::update_settings`).
fn write(store: Option<&Store>, gpu_failed: bool, software: bool) {
    let Some(store) = store else {
        return;
    };
    let before = UiState {
        renderer: GPU_RENDERER,
        gpu_failed: !gpu_failed,
        ..UiState::default()
    };
    let after = UiState {
        renderer: if software { 0 } else { GPU_RENDERER },
        gpu_failed,
        ..UiState::default()
    };
    // A failure leaves the stored value as it was: at worst the next start tries the GPU
    // renderer again, or uses software once more.
    let _ = store.update_settings(UI_STATE_KEY, &before, &after);
}
