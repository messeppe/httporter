//! The command palette (T7): Ctrl+K, and `?` for the keyboard shortcuts.
//!
//! It lists the window's actions, the sites, the publish profiles, the favorites and the folders
//! of the folder the cloud pane shows, in sections. Typing matches a subsequence of each title
//! (titles that start with what was typed first, then those with a word that does, then the
//! rest); the best 50 are shown. A query that starts with `?` lists the shortcuts instead. An
//! action that can't run now is shown off, with why in place of its subtitle. Enter (or a click)
//! runs the chosen row through the window's own callbacks, as its button, menu or key would.
//!
//! I5 (T3) adds Undo and Reopen Closed Tab to the actions, and its five keys (Ctrl+Z,
//! Ctrl+Shift+T, Ctrl+comma, Alt+Up, F6) to the end of the shortcuts.

use std::cmp::Reverse;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_core::{ProfileRecord, SiteId};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Manager, copy};
use crate::comfort::copy as comfort_copy;
use crate::{AppWindow, PaletteIcon, PaletteItem};

/// The most rows (sections not counted) the palette shows.
const TOP: usize = 50;
/// The most folders of the cloud pane it looks through.
const FOLDERS_MAX: usize = 500;

/// What a row of the palette does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Publish,
    Purge,
    Upload,
    Download,
    NewFolder,
    Rename,
    MoveTo,
    CopyLink,
    Compare,
    ToggleView,
    NewTab,
    Activity,
    Settings,
    AddSite,
    Refresh,
    Site(i32),
    Profile(i32),
    Favorite(i32),
    Folder(i32),
    QuickLook,
    Up,
    Back,
    Forward,
    Delete,
    SelectAll,
    Paste,
    CloseTab,
    NextTab,
    PreviousTab,
    // I5 (T3).
    Undo,
    ReopenTab,
    SwitchPane,
    /// About HTTPorter, Check for Updates (I5, `system/`).
    System(crate::system::PaletteCommand),
}

/// What each of [`copy::SHORTCUTS`] runs, in the same order (`None`: only shown).
const SHORTCUT_COMMANDS: [Option<Command>; 23] = [
    None,
    None,
    Some(Command::QuickLook),
    None,
    Some(Command::Up),
    Some(Command::Back),
    Some(Command::Forward),
    Some(Command::Refresh),
    Some(Command::Rename),
    Some(Command::Delete),
    Some(Command::NewFolder),
    Some(Command::Upload),
    Some(Command::Paste),
    Some(Command::SelectAll),
    Some(Command::Publish),
    Some(Command::CopyLink),
    None,
    Some(Command::NewTab),
    Some(Command::CloseTab),
    Some(Command::NextTab),
    Some(Command::PreviousTab),
    None,
    None,
];

/// What each of `comfort::copy::SHORTCUTS` (I5) runs, in the same order.
const COMFORT_SHORTCUT_COMMANDS: [Option<Command>; 5] = [
    Some(Command::Undo),
    Some(Command::ReopenTab),
    Some(Command::Settings),
    Some(Command::Up),
    Some(Command::SwitchPane),
];

/// What this view remembers between callbacks (T7).
#[derive(Default)]
pub struct PaletteState {
    /// What each row shown does (`None` for a section header), by row.
    commands: Vec<Option<Command>>,
    /// The publish profiles, read when the palette opens: Publish names the one it publishes.
    profiles: Vec<ProfileRecord>,
}

/// The palette's sections, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Actions,
    Sites,
    Profiles,
    Favorites,
    Folders,
    Shortcuts,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Actions => copy::SECTION_ACTIONS,
            Self::Sites => copy::SECTION_SITES,
            Self::Profiles => copy::SECTION_PROFILES,
            Self::Favorites => copy::SECTION_FAVORITES,
            Self::Folders => copy::SECTION_FOLDERS,
            Self::Shortcuts => copy::SECTION_SHORTCUTS,
        }
    }
}

/// A row the palette may show.
struct Candidate {
    section: Section,
    title: String,
    subtitle: String,
    icon: PaletteIcon,
    keys: &'static str,
    enabled: bool,
    command: Option<Command>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m` (T7).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_palette_query_changed({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |query| {
            if let Some(ui) = weak.upgrade() {
                // Ctrl+K and `?` open the palette with "" and "?".
                if query.is_empty() || query == "?" {
                    read_profiles(&ui, &m);
                }
                build(&ui, &m, &query);
            }
        }
    });
    ui.on_palette_run({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |row| {
            let command = usize::try_from(row)
                .ok()
                .and_then(|row| lock(&m.palette).commands.get(row).copied().flatten());
            if let (Some(ui), Some(command)) = (weak.upgrade(), command) {
                run(&ui, command);
            }
        }
    });
}

/// Reads the publish profiles (off the UI thread); the palette is built again if Publish's title
/// changes with them.
fn read_profiles(ui: &AppWindow, m: &Arc<Manager>) {
    let (session, weak, m) = (m.session.clone(), ui.as_weak(), m.clone());
    m.link.rt().clone().spawn(async move {
        let profiles = session.profiles().await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let changed = {
                let mut st = lock(&m.palette);
                let changed = st.profiles != profiles;
                st.profiles = profiles;
                changed
            };
            if changed && ui.get_palette_open() {
                build(&ui, &m, &ui.get_palette_query());
            }
        });
    });
}

/// Fills the palette with the rows `query` matches.
fn build(ui: &AppWindow, m: &Manager, query: &str) {
    let shortcuts = query.trim_start().strip_prefix('?');
    let candidates = match shortcuts {
        Some(_) => shortcut_candidates(),
        None => {
            let mut all = actions(ui, m);
            all.extend(sites(ui));
            all.extend(profiles(ui));
            all.extend(favorites(ui));
            all.extend(folders(ui));
            all
        }
    };
    let needle = shortcuts.unwrap_or(query).trim();
    let mut scored: Vec<(u32, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            // A site's or a favorite's second line says where it is ("Cloudflare R2"), worth
            // matching; the others' only repeat what the section says.
            let by_subtitle = matches!(c.section, Section::Sites | Section::Favorites);
            score(needle, &c.title)
                .or_else(|| {
                    by_subtitle
                        .then(|| score(needle, &c.subtitle))
                        .flatten()
                        .map(|s| s / 4)
                })
                .or_else(|| score(needle, c.keys).map(|s| s / 4))
                .map(|s| (s, i))
        })
        .collect();
    // The best first (equals keep their order), then grouped by section, best first in each.
    scored.sort_by_key(|&(s, _)| Reverse(s));
    scored.truncate(TOP);
    scored.sort_by_key(|&(s, i)| (candidates[i].section, Reverse(s)));

    let mut items = Vec::with_capacity(scored.len() + 6);
    let mut commands = Vec::with_capacity(scored.len() + 6);
    let mut section = None;
    for (_, i) in scored {
        let c = &candidates[i];
        if section != Some(c.section) {
            section = Some(c.section);
            items.push(PaletteItem {
                header: true,
                title: c.section.title().into(),
                subtitle: SharedString::new(),
                icon: PaletteIcon::Action,
                keys: SharedString::new(),
                enabled: true,
            });
            commands.push(None);
        }
        items.push(PaletteItem {
            header: false,
            title: c.title.as_str().into(),
            subtitle: c.subtitle.as_str().into(),
            icon: c.icon,
            keys: c.keys.into(),
            enabled: c.enabled,
        });
        commands.push(c.command);
    }
    lock(&m.palette).commands = commands;
    ui.set_palette_items(ModelRc::new(VecModel::from(items)));
}

/// How well `needle` matches `text`, if it does (higher is better): `text` starts with it, a word
/// of `text` does, `text` holds it, or its letters are in `text` in order (more of them at word
/// starts and closer together is better). Any case.
fn score(needle: &str, text: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(1);
    }
    let needle = needle.to_lowercase();
    let text = text.to_lowercase();
    if text.starts_with(&needle) {
        return Some(4000);
    }
    let starts = word_starts(&text);
    if starts.iter().any(|&at| text[at..].starts_with(&needle)) {
        return Some(3000);
    }
    if text.contains(&needle) {
        return Some(2000);
    }
    let mut at_starts = 0u32;
    let mut gaps = 0u32;
    let mut rest = text.char_indices().peekable();
    for wanted in needle.chars().filter(|c| !c.is_whitespace()) {
        let mut skipped = 0u32;
        loop {
            let (at, c) = rest.next()?;
            if c == wanted {
                if starts.contains(&at) {
                    at_starts += 1;
                }
                break;
            }
            skipped += 1;
        }
        gaps += skipped;
    }
    Some((1000 + 50 * at_starts).saturating_sub(gaps.min(900)).max(1))
}

/// Where the words of `text` start (byte offsets): its start, and after anything that isn't a
/// letter or a digit.
fn word_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut after_gap = true;
    for (at, c) in text.char_indices() {
        let word = c.is_alphanumeric();
        if word && after_gap {
            starts.push(at);
        }
        after_gap = !word;
    }
    starts
}

// ---- The rows ----

fn candidate(
    section: Section,
    title: impl Into<String>,
    subtitle: impl Into<String>,
    icon: PaletteIcon,
    keys: &'static str,
    command: Command,
) -> Candidate {
    Candidate {
        section,
        title: title.into(),
        subtitle: subtitle.into(),
        icon,
        keys,
        enabled: true,
        command: Some(command),
    }
}

/// An action that is off now shows `why` in place of its subtitle.
fn action(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    icon: PaletteIcon,
    keys: &'static str,
    command: Command,
    why_not: Option<&str>,
) -> Candidate {
    let mut c = candidate(Section::Actions, title, subtitle, icon, keys, command);
    if let Some(why) = why_not {
        c.enabled = false;
        c.subtitle = why.to_owned();
    }
    c
}

/// `why` when `on` is false.
fn unless(on: bool, why: &str) -> Option<&str> {
    (!on).then_some(why)
}

/// The window's actions, as its toolbar, menus and keys offer them now.
fn actions(ui: &AppWindow, m: &Manager) -> Vec<Candidate> {
    let site_id = chosen_site(ui);
    let site = site_id.as_ref().and_then(|id| m.session.site(id));
    let site_name = site.as_ref().map(|s| s.name.clone());
    let site_open = site.is_some() && ui.get_remote_crumbs().row_count() > 0;
    let remote_place = site_name
        .as_deref()
        .map(|name| copy::palette_site_folder(name, last(&ui.get_remote_crumbs()).as_deref()))
        .unwrap_or_default();
    let local_crumbs = ui.get_local_crumbs();
    let local_place = local_crumbs
        .row_count()
        .checked_sub(1)
        .and_then(|i| local_crumbs.row_data(i))
        .map_or_else(|| crate::copy::THIS_PC.to_owned(), |s| s.to_string());
    let pane_place = if ui.get_local_pane_active() {
        local_place.clone()
    } else {
        remote_place.clone()
    };
    let picked = ui.get_remote_selected_count();
    let on_site = site_name.as_deref().unwrap_or_default();
    let actions_note = ui.get_actions_note();
    let off_note = |fallback: &'static str| -> String {
        if actions_note.is_empty() {
            fallback.to_owned()
        } else {
            actions_note.to_string()
        }
    };
    let can_write = ui.get_can_write();
    let writes_off = if site_open {
        copy::PALETTE_WRITES_OFF
    } else {
        copy::PALETTE_OPEN_A_SITE
    };

    let (publish_title, publish_note) = publish_texts(ui, m, site.as_ref());
    let mut list = vec![
        action(
            publish_title,
            publish_note,
            PaletteIcon::Publish,
            copy::KEYS_PUBLISH,
            Command::Publish,
            unless(ui.get_can_publish(), copy::PALETTE_OPEN_A_SITE),
        ),
        action(
            copy::PALETTE_PURGE,
            copy::palette_selected_on(on_site),
            PaletteIcon::Purge,
            "",
            Command::Purge,
            unless(ui.get_can_purge(), copy::PALETTE_PURGE_OFF),
        ),
    ];
    let upload_off = off_note(copy::PALETTE_OPEN_A_SITE);
    list.push(action(
        copy::PALETTE_UPLOAD,
        copy::palette_to(&remote_place),
        PaletteIcon::Upload,
        copy::KEYS_UPLOAD,
        Command::Upload,
        unless(ui.get_can_upload(), &upload_off),
    ));
    list.push(action(
        copy::PALETTE_DOWNLOAD,
        copy::palette_to(&local_place),
        PaletteIcon::Download,
        "",
        Command::Download,
        unless(ui.get_can_download(), copy::PALETTE_DOWNLOAD_OFF),
    ));
    list.push(action(
        copy::PALETTE_NEW_FOLDER,
        copy::palette_in(&pane_place),
        PaletteIcon::Folder,
        copy::KEYS_NEW_FOLDER,
        Command::NewFolder,
        unless(ui.get_can_new_folder(), copy::PALETTE_NEW_FOLDER_OFF),
    ));
    let rename_off = if !can_write {
        Some(writes_off)
    } else {
        unless(picked == 1, copy::PALETTE_PICK_ONE_ON_SITE)
    };
    list.push(action(
        copy::PALETTE_RENAME,
        copy::palette_selected_on(on_site),
        PaletteIcon::Action,
        copy::KEYS_RENAME,
        Command::Rename,
        rename_off,
    ));
    let move_off = if !can_write {
        Some(writes_off)
    } else {
        unless(picked > 0, copy::PALETTE_PICK_ON_SITE)
    };
    list.push(action(
        copy::PALETTE_MOVE,
        copy::palette_selected_on(on_site),
        PaletteIcon::Action,
        "",
        Command::MoveTo,
        move_off,
    ));
    list.push(action(
        copy::PALETTE_COPY_LINK,
        copy::palette_selected_on(on_site),
        PaletteIcon::Action,
        copy::KEYS_COPY_LINK,
        Command::CopyLink,
        unless(picked > 0, copy::PALETTE_PICK_ON_SITE),
    ));
    let comparing = ui.get_comparing();
    list.push(action(
        copy::PALETTE_COMPARE,
        if comparing {
            copy::PALETTE_ON
        } else {
            copy::PALETTE_OFF
        },
        PaletteIcon::Action,
        "",
        Command::Compare,
        unless(ui.get_can_compare() || comparing, copy::PALETTE_COMPARE_OFF),
    ));
    list.push(action(
        if ui.get_view_mode() == 0 {
            copy::PALETTE_SINGLE_PANE
        } else {
            copy::PALETTE_TWO_PANES
        },
        copy::PALETTE_VIEW_NOTE,
        PaletteIcon::Action,
        "",
        Command::ToggleView,
        None,
    ));
    list.push(action(
        copy::PALETTE_NEW_TAB,
        copy::PALETTE_NEW_TAB_NOTE,
        PaletteIcon::Action,
        copy::KEYS_NEW_TAB,
        Command::NewTab,
        None,
    ));
    let undo_next = crate::comfort::undo::next_undo();
    list.push(action(
        comfort_copy::PALETTE_UNDO,
        undo_next.clone().unwrap_or_default(),
        PaletteIcon::Action,
        comfort_copy::KEYS_UNDO,
        Command::Undo,
        unless(undo_next.is_some(), comfort_copy::PALETTE_NOTHING_TO_UNDO),
    ));
    list.push(action(
        comfort_copy::PALETTE_REOPEN_TAB,
        "",
        PaletteIcon::Action,
        comfort_copy::KEYS_REOPEN_TAB,
        Command::ReopenTab,
        unless(
            crate::comfort::undo::has_closed_tab(),
            comfort_copy::PALETTE_NO_CLOSED_TAB,
        ),
    ));
    list.push(action(
        copy::PALETTE_ACTIVITY,
        copy::PALETTE_ACTIVITY_NOTE,
        PaletteIcon::Action,
        "",
        Command::Activity,
        None,
    ));
    list.push(action(
        copy::PALETTE_SETTINGS,
        copy::PALETTE_SETTINGS_NOTE,
        PaletteIcon::Settings,
        "",
        Command::Settings,
        unless(ui.get_settings_available(), copy::PALETTE_LATER),
    ));
    list.extend(crate::system::palette_entries(ui).into_iter().map(|e| {
        let command = Command::System(e.command);
        action(
            e.title,
            e.subtitle,
            PaletteIcon::Settings,
            "",
            command,
            e.off,
        )
    }));
    list.push(action(
        copy::PALETTE_ADD_SITE,
        copy::PALETTE_ADD_SITE_NOTE,
        PaletteIcon::Site,
        "",
        Command::AddSite,
        None,
    ));
    list.push(action(
        copy::PALETTE_REFRESH,
        pane_place,
        PaletteIcon::Action,
        copy::KEYS_REFRESH,
        Command::Refresh,
        None,
    ));
    list
}

/// Publish's title and subtitle: the profile shown, else the open site's profile published last
/// (as the toolbar's Publish chooses), else a new profile.
fn publish_texts(
    ui: &AppWindow,
    m: &Manager,
    site: Option<&httporter_core::SiteRecord>,
) -> (String, String) {
    let shown = usize::try_from(ui.get_selected_profile())
        .ok()
        .and_then(|row| ui.get_profiles().row_data(row));
    let st = lock(&m.palette);
    let profile = match &shown {
        Some(item) => st.profiles.iter().find(|p| p.id.0 == item.id.as_str()),
        None => site.and_then(|site| {
            st.profiles
                .iter()
                .filter(|p| p.site == site.id)
                .max_by_key(|p| p.last_published_at)
        }),
    };
    match (profile, &shown) {
        (Some(profile), _) => {
            let container = m
                .session
                .site(&profile.site)
                .map(|s| s.container)
                .unwrap_or_default();
            (
                copy::palette_publish(Some(profile.name.as_str())),
                copy::palette_publish_route(
                    &profile.local_dir.display().to_string(),
                    &container,
                    profile.remote_dir.as_str(),
                ),
            )
        }
        (None, Some(item)) => (
            copy::palette_publish(Some(item.name.as_str())),
            String::new(),
        ),
        (None, None) => (
            copy::palette_publish(None),
            site.map(|s| copy::palette_new_profile(&s.name))
                .unwrap_or_default(),
        ),
    }
}

fn sites(ui: &AppWindow) -> Vec<Candidate> {
    ui.get_sites()
        .iter()
        .zip(0..)
        .map(|(site, i)| {
            candidate(
                Section::Sites,
                site.name.as_str(),
                site.subtitle.as_str(),
                PaletteIcon::Site,
                "",
                Command::Site(i),
            )
        })
        .collect()
}

fn profiles(ui: &AppWindow) -> Vec<Candidate> {
    ui.get_profiles()
        .iter()
        .zip(0..)
        .map(|(profile, i)| {
            candidate(
                Section::Profiles,
                profile.name.as_str(),
                copy::palette_profile_note(&profile.meta),
                PaletteIcon::Profile,
                "",
                Command::Profile(i),
            )
        })
        .collect()
}

fn favorites(ui: &AppWindow) -> Vec<Candidate> {
    ui.get_favorites()
        .iter()
        .zip(0..)
        .map(|(place, i)| {
            candidate(
                Section::Favorites,
                place.name.as_str(),
                place.subtitle.as_str(),
                PaletteIcon::Favorite,
                "",
                Command::Favorite(i),
            )
        })
        .collect()
}

/// The folders of the folder the cloud pane shows.
fn folders(ui: &AppWindow) -> Vec<Candidate> {
    let Some(site) = chosen_site_name(ui) else {
        return Vec::new();
    };
    let note = copy::palette_folder_on(&site);
    ui.get_remote_rows()
        .iter()
        .zip(0..)
        .filter(|(row, _)| row.folder)
        .take(FOLDERS_MAX)
        .map(|(row, i)| {
            candidate(
                Section::Folders,
                row.name.as_str(),
                note.as_str(),
                PaletteIcon::Folder,
                "",
                Command::Folder(i),
            )
        })
        .collect()
}

fn shortcut_candidates() -> Vec<Candidate> {
    copy::SHORTCUTS
        .iter()
        .zip(SHORTCUT_COMMANDS)
        .chain(
            comfort_copy::SHORTCUTS
                .iter()
                .zip(COMFORT_SHORTCUT_COMMANDS),
        )
        .map(|(&(title, keys), command)| Candidate {
            section: Section::Shortcuts,
            title: title.to_owned(),
            subtitle: String::new(),
            icon: PaletteIcon::Shortcut,
            keys,
            enabled: true,
            command,
        })
        .collect()
}

// ---- Running a row ----

/// Runs `command` as the window's own button, menu or key would: only while that would.
fn run(ui: &AppWindow, command: Command) {
    let active = if ui.get_local_pane_active() { 0 } else { 1 };
    let tabs = i32::try_from(ui.get_tabs().row_count()).unwrap_or(0);
    let current_tab = ui.get_current_tab();
    match command {
        Command::Publish if ui.get_can_publish() => ui.invoke_publish(),
        Command::Purge if ui.get_can_purge() => ui.invoke_purge(),
        Command::Upload if ui.get_can_upload() => ui.invoke_upload(),
        Command::Download if ui.get_can_download() => ui.invoke_download(),
        Command::NewFolder if ui.get_can_new_folder() => ui.invoke_new_folder(),
        Command::Rename if ui.get_can_write() => ui.invoke_rename_start(),
        Command::MoveTo if ui.get_can_write() => ui.invoke_move_start(),
        Command::CopyLink => ui.invoke_copy_link(active),
        Command::Compare if ui.get_can_compare() || ui.get_comparing() => {
            ui.invoke_toggle_compare();
        }
        Command::ToggleView => ui.invoke_set_view(if ui.get_view_mode() == 0 { 1 } else { 0 }),
        Command::NewTab => ui.invoke_tab_new(),
        Command::Activity => {
            ui.set_activity_open(true);
            ui.invoke_activity_filter_changed(ui.get_activity_filter());
        }
        Command::Settings if ui.get_settings_available() => ui.invoke_open_settings(),
        Command::System(command) => crate::system::run_palette(ui, command),
        Command::AddSite => {
            ui.set_add_site_open(true);
            ui.invoke_add_site();
        }
        Command::Refresh => ui.invoke_refresh(),
        Command::Site(i) => {
            ui.set_selected_profile(-1);
            ui.set_selected_site(i);
            ui.invoke_site_selected(i);
        }
        Command::Profile(i) => {
            ui.set_selected_profile(i);
            ui.invoke_profile_selected(i);
        }
        Command::Favorite(i) => ui.invoke_favorite_selected(i),
        Command::Folder(row) => ui.invoke_open_remote(row),
        Command::QuickLook => ui.invoke_quick_look(active),
        Command::Up if active == 0 => ui.invoke_local_up(),
        Command::Up => ui.invoke_remote_up(),
        Command::Back if can_go(ui, true) => ui.invoke_go_back(),
        Command::Forward if can_go(ui, false) => ui.invoke_go_forward(),
        Command::Delete if ui.get_can_delete() => ui.invoke_delete(),
        Command::SelectAll => ui.invoke_select_all(active),
        Command::Paste if ui.get_can_upload() => ui.invoke_paste(),
        Command::CloseTab if tabs > 1 => ui.invoke_tab_close(current_tab),
        Command::NextTab if tabs > 1 => ui.invoke_tab_selected((current_tab + 1) % tabs),
        Command::PreviousTab if tabs > 1 => {
            ui.invoke_tab_selected((current_tab + tabs - 1) % tabs);
        }
        Command::Undo => ui.invoke_undo(),
        Command::ReopenTab => ui.invoke_reopen_tab(),
        Command::SwitchPane => ui.invoke_switch_pane(),
        _ => {}
    }
}

/// Back (or Forward) may be pressed, for the pane last used.
fn can_go(ui: &AppWindow, back: bool) -> bool {
    match (ui.get_local_pane_active(), back) {
        (true, true) => ui.get_local_can_go_back(),
        (true, false) => ui.get_local_can_go_forward(),
        (false, true) => ui.get_remote_can_go_back(),
        (false, false) => ui.get_remote_can_go_forward(),
    }
}

// ---- Helpers ----

/// The site chosen in the sidebar, if any.
fn chosen_site(ui: &AppWindow) -> Option<SiteId> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| SiteId(site.id.to_string()))
}

fn chosen_site_name(ui: &AppWindow) -> Option<String> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| site.name.to_string())
}

/// The last of `crumbs` past the first (the folder a pane shows, not its drive or zone).
fn last(crumbs: &ModelRc<SharedString>) -> Option<String> {
    let n = crumbs.row_count();
    (n > 1)
        .then(|| crumbs.row_data(n - 1))
        .flatten()
        .map(|s| s.to_string())
}
