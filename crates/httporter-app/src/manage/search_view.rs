// SPDX-License-Identifier: MIT OR Apache-2.0

//! Searching the open site (T7): the toolbar field "Search {site}", the filter bar and the results.
//!
//! Return in the field searches the open site from its root ("Whole site") or from the folder the
//! cloud pane shows ("This folder"), with the filter bar's Kind, Size and Modified. The matches
//! stream into the cloud pane in place of the folder (the session's search mode), named by their
//! path, and the status line counts the folders listed and the matches found. They are ordinary
//! rows: selection, Quick Look, download, delete, copy link and the inspector work on them.
//!
//! The search ends with Escape, a cleared field or the bar's close button (the pane lists the
//! folder it began in again), and whenever the pane moves elsewhere (a site, a folder, a tab).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeDelta, Utc};
use httporter_core::Entry;
use httporter_transfer::{KindFilter, SearchProgress, SearchQuery, search};
use slint::{ComponentHandle, Model, SharedString};
use tokio::task::JoinHandle;

use super::{Manager, Side, copy};
use crate::binding::{Change, Link, show_status};
use crate::session::{Session, Ticket};
use crate::{AppWindow, SearchFilters};

/// How often the pane takes in the matches found meanwhile.
const VIEW_EVERY: Duration = Duration::from_millis(250);

/// What this view remembers between callbacks (T7).
#[derive(Default)]
pub struct SearchState {
    /// The search running, which a newer one or the end of the search gives up.
    task: Option<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m` (T7) and shows the search field.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.set_search_placeholder(copy::search_placeholder(chosen_site_name(ui).as_deref()).into());
    ui.set_search_available(true);
    ui.on_search_submitted({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |text| {
            if let Some(ui) = weak.upgrade() {
                start(&ui, &m, &text);
            }
        }
    });
    ui.on_search_filters_changed({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |_| {
            if let Some(ui) = weak.upgrade()
                && ui.get_searching()
            {
                start(&ui, &m, &ui.get_search_text());
            }
        }
    });
    ui.on_search_cancel({
        let (m, weak) = (m.clone(), ui.as_weak());
        move || {
            if let Some(ui) = weak.upgrade() {
                cancel(&ui, &m);
            }
        }
    });
}

/// The cloud pane was put on screen (from [`super::tabs::navigated`]): the field names the site,
/// and a search the pane has left ends in the window too.
pub(super) fn navigated(ui: &AppWindow, m: &Manager, side: Side) {
    if side != Side::Remote {
        return;
    }
    let placeholder = copy::search_placeholder(chosen_site_name(ui).as_deref());
    if ui.get_search_placeholder().as_str() != placeholder {
        ui.set_search_placeholder(placeholder.into());
    }
    if ui.get_searching() && !m.session.remote_searching() {
        stop_task(m);
        ui.set_searching(false);
        ui.set_search_text(SharedString::new());
    }
}

/// Return in the search field, or a filter changed while searching: (re)starts the search.
fn start(ui: &AppWindow, m: &Manager, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        cancel(ui, m);
        return;
    }
    stop_task(m);
    let filters = ui.get_search_filters();
    let ticket = m.session.remote_ticket();
    let Some((view, provider, root)) =
        m.session
            .start_search(ticket, filters.scope == 1, copy::search_results_for(text))
    else {
        show_status(ui, None, copy::SEARCH_NEEDS_SITE.to_owned());
        return;
    };
    // The matches are no folder to compare.
    if ui.get_comparing() {
        ui.invoke_toggle_compare();
    }
    ui.set_searching(true);
    // The status line follows the cloud pane, where the matches are.
    ui.set_local_pane_active(false);
    m.link.show_remote_as(view, Change::Listed);
    set_status(&m.link, copy::SEARCH_STARTING.to_owned());

    let query = query_of(text, &filters);
    let (session, link) = (m.session.clone(), m.link.clone());
    let task = m.link.rt().spawn(async move {
        let last = Mutex::new(None::<Instant>);
        let on_found = |found: Vec<Entry>, progress: SearchProgress| {
            if !session.search_add(ticket, found) {
                return;
            }
            let mut last = lock(&last);
            if last.is_none_or(|at| at.elapsed() >= VIEW_EVERY) {
                *last = Some(Instant::now());
                drop(last);
                show_found(&session, &link, ticket, copy::search_progress(progress));
            }
        };
        let status = match search(&*provider, &root, &query, &on_found).await {
            Ok(progress) => copy::search_done(progress),
            Err(error) => copy::search_failed(&crate::copy::describe_transfer_error(&error)),
        };
        show_found(&session, &link, ticket, status);
    });
    lock(&m.search).task = Some(task);
}

/// Puts the matches search `ticket` has found so far in the cloud pane (the selection stays), and
/// `status` in the status line; nothing once the pane has moved on.
fn show_found(session: &Session, link: &Link, ticket: Ticket, status: String) {
    let Some(view) = session.search_view(ticket) else {
        return;
    };
    link.show_remote_as(view, Change::Refreshed);
    set_status(link, status);
}

/// The cloud pane's status (after the view on its way, which sets its item count).
fn set_status(link: &Link, text: String) {
    let _ = link
        .ui()
        .upgrade_in_event_loop(move |ui| ui.set_remote_status(text.into()));
}

/// Escape or a cleared field, or the filter bar's close button: the search ends and the pane
/// lists the folder it began in again.
fn cancel(ui: &AppWindow, m: &Manager) {
    stop_task(m);
    ui.set_searching(false);
    ui.set_search_text(SharedString::new());
    if !m.session.remote_searching() {
        return;
    }
    let ticket = m.session.remote_ticket();
    let (session, link) = (m.session.clone(), m.link.clone());
    m.link.rt().spawn(async move {
        let started = {
            let link = link.clone();
            move |view| link.show_remote_as(view, Change::Loading)
        };
        let view = session.end_search(ticket, started).await;
        link.show_remote_as(view, Change::Listed);
    });
}

fn stop_task(m: &Manager) {
    if let Some(task) = lock(&m.search).task.take() {
        task.abort();
    }
}

/// What to look for: `text`, with the filter bar's choices (their positions in its pop-ups).
fn query_of(text: &str, filters: &SearchFilters) -> SearchQuery {
    let kind = match filters.kind {
        1 => Some(KindFilter::Image),
        2 => Some(KindFilter::Code),
        3 => Some(KindFilter::Document),
        4 => Some(KindFilter::Archive),
        5 => Some(KindFilter::Media),
        _ => None,
    };
    let min_size = match filters.size {
        1 => Some(100 << 10),
        2 => Some(1 << 20),
        3 => Some(10 << 20),
        _ => None,
    };
    let modified_after = match filters.modified {
        1 => start_of_today(),
        2 => Some(Utc::now() - TimeDelta::days(7)),
        3 => Some(Utc::now() - TimeDelta::days(30)),
        _ => None,
    };
    SearchQuery {
        text: text.to_owned(),
        kind,
        min_size,
        modified_after,
    }
}

/// Midnight at the start of today, on this PC's clock.
fn start_of_today() -> Option<DateTime<Utc>> {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(Local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

/// The name of the site chosen in the sidebar, if any.
fn chosen_site_name(ui: &AppWindow) -> Option<String> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| site.name.to_string())
}
