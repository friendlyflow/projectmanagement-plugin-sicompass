//! The project-management plugin. Its first feature is a kanban board.
//!
//! A sicompass plugin: a program sicompass starts (`src/main.rs`), with the
//! user's rights. The board lives in the plugin's own folder (`"storage":
//! true`, the same directory the built-in used, so nothing moves), and the
//! optional cloud sync is in [`cloud`].
//!
//! # Two surfaces, one board
//!
//! The same two-level tree is reachable two ways, and which one you want depends
//! on whether you are reading or arranging:
//!
//! * **The list** (general mode) is the provider root: columns as `Obj` rows,
//!   cards as `Str` rows one level in. Everything structural happens here through
//!   the app's shared structural-edit capability, so Ctrl+I / Ctrl+A / Ctrl+D /
//!   Delete / Ctrl+X / Ctrl+C / Ctrl+V behave exactly as they do in every other
//!   provider that declares it, and the app records the undo.
//! * **The board** (the `d` dashboard) draws the columns side by side. Arrow keys
//!   move between and within them, and the same editing keys work, but here the
//!   provider implements them itself: the app forwards every keystroke into an
//!   interactive dashboard without interpreting it.
//!
//! # The archive is a column the board does not draw
//!
//! `archive card` retires a card. Where it goes is an ordinary [`board::Column`],
//! pinned last and named by `Board::archive`, which the list shows like any other
//! and the board simply stops drawing one column short of.
//!
//! That is the whole mechanism, and it is why the archive cost almost no code:
//! navigating into it, editing it, reconciling it, storing it and undoing into it
//! are the paths that already existed for every other column. A `Vec<Card>` parked
//! outside `columns` would have left the board untouched instead, at the price of
//! a second case in `locate_card`, `reconcile`, the path handling and the store.
//!
//! Two invariants carry it, both in `board.rs`: the archive column is always
//! **last**, so `visible_len()` is a prefix and hiding it is arithmetic rather
//! than a per-column test; and an id naming no column reads as **no archive**, so
//! deleting it in the list needs no special handling. `clamp_focus` is the single
//! line that keeps the board's cursor below `visible_len`, which is what lets
//! everything downstream of it go on indexing `columns` directly.
//!
//! There is no `unarchive` command, because the archive is a real column and
//! `move left` already walks a card back out of it.
//!
//! # Every list opens with its list meta
//!
//! Like the notes plugin's, the first row of the columns list and of every
//! column is a `list meta:` row (below the cloud row, at the top). Inside it:
//! the Merkle hash of the board or of that column (`board.rs`), which changes
//! when anything in it changes, and, while cloud sync is on, whether it is as
//! it was at the last sync. It is rendered, never stored: `reconcile` skips
//! it, it cannot be edited or deleted, and the board view, which draws from
//! [`board::Board`] and not from these rows, never sees it. Being a row, it
//! shifts every index the app counts in `fetch()` rows (the dashboard entry
//! path and `SelectPath`), which [`ProjectManagementProvider::lead_rows`]
//! accounts for.
//!
//! # Why a card is a `Str`
//!
//! `Obj` versus `Str` is the depth cap, not a stylistic choice. The app descends
//! into an `Obj` and cannot descend into a `Str`, so making cards `Str` means
//! "more layers are not shown" needs no guard anywhere in the navigation code —
//! there is nowhere for a third level to appear.
//!
//! # The `<id>` prefix
//!
//! Every row is `<id>N</id><input>text</input>`, with the id **outside** the
//! `<input>` — inside it, the app's live edit eats it on the first keystroke.
//! Ids are what make `sync_ffon_body_children` a diff rather than a guess:
//! `commit_edit(old, new)` cannot tell two identically titled siblings apart,
//! because `old` is only the previous display text.
//!
//! # Undo, in two halves
//!
//! List edits ride the app's `TimelineEntry::Structural` records, which reverse
//! by mutating the app's FFON tree and calling back into
//! `sync_ffon_body_children`. A board edit never touches that tree, so those arms
//! could not reverse one even if they were recorded. Board edits therefore emit
//! a `ProviderOp` and are reversed here, in [`Plugin::undo`]. Both
//! kinds land on the same per-tab timeline in the order they happened, so there
//! is still one undo history rather than a board-shaped exception to it.

pub mod board;
pub mod cloud;
mod escape;
pub mod localize;
pub mod render;
pub mod store;

use cloud::{Cloud, CloudHost, Finished, PluginHost};
use serde::{Deserialize, Serialize};
use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::input::{self, InputLine, InputState};
use sicompass_sdk::plugin::{
    DashboardKind, DashboardRequest, Descriptor, Key, Keysym, NavigationRequest, Plugin,
    PollResult, ProviderOp,
};
use sicompass_sdk::tags;
use std::collections::HashMap;
use std::path::PathBuf;

use board::{Board, Card, Column, Id};
use render::{Focus, View};

// ---------------------------------------------------------------------------
// Command ids
//
// Stable identifiers, matched by equality in `handle_command`. The app's palette
// renders these raw strings, `command_label` localizes them.
//
// `"delete"` is deliberately absent. It is a reserved id: `avail_provider_has_delete`
// would claim both Ctrl+D and Delete and route them to `invoke_provider_delete`,
// which unwinds the cursor to depth 3 before deleting — email-shaped surgery
// that has no meaning on a two-level board.
// ---------------------------------------------------------------------------

pub const CMD_MOVE_UP: &str = "move up";
pub const CMD_MOVE_DOWN: &str = "move down";
pub const CMD_MOVE_LEFT: &str = "move left";
pub const CMD_MOVE_RIGHT: &str = "move right";

/// Retire a card: move it into the archive, which the board never draws.
///
/// Spelled the way the user reads it, because the app's palette renders the raw
/// strings from `commands()` and never calls `command_label` (see
/// `src/sicompass/src/list.rs`). Lowercase, like every other command id in the
/// app.
pub const CMD_ARCHIVE: &str = "archive card";
pub const CMD_SYNC_NOW: &str = "sync with the cloud now";

// ---------------------------------------------------------------------------
// Board operations, as they cross the timeline
// ---------------------------------------------------------------------------

/// What a card or column looked like, in enough detail to put it back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CardData {
    id: Id,
    text: String,
}

/// One reversible board edit.
///
/// Cards only. Columns are created, renamed, reordered and deleted in the list
/// view, where the app's own structural-edit capability records them as
/// `Structural` entries — the board has no gesture that reaches a column, so it
/// has no column op to record.
///
/// Carried in a `ProviderOp` as JSON in the payload rather than as
/// a tagged FFON shape: the app never inspects it, only hands it back, so a
/// self-describing blob keeps the wire format in one place instead of spreading
/// it across the FFON encoder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
enum BoardOp {
    Add {
        column: Id,
        index: usize,
        card: CardData,
    },
    Delete {
        column: Id,
        index: usize,
        card: CardData,
    },
    Rename {
        id: Id,
        before: String,
        after: String,
    },
    Move {
        card: Id,
        from_column: Id,
        from_index: usize,
        to_column: Id,
        to_index: usize,
    },
    /// The same motion as [`BoardOp::Move`], recorded apart so the undo history
    /// says what happened. `record` derives the timeline label from
    /// [`BoardOp::command`], so reusing `Move` here would make the undo screen
    /// and the spoken undo both say "move card" for an archive.
    Archive {
        card: Id,
        from_column: Id,
        from_index: usize,
        to_column: Id,
        to_index: usize,
    },
}

impl BoardOp {
    /// The stable command id, also the label key suffix (`projectmanagement-op-{id}`).
    ///
    /// Spelled out rather than derived from the variant name: these strings
    /// reach the user through the undo-history screen, so renaming a variant
    /// must not silently rename a label key and leave it unresolved.
    fn command(&self) -> &'static str {
        match self {
            BoardOp::Add { .. } => "add-card",
            BoardOp::Delete { .. } => "delete-card",
            BoardOp::Rename { .. } => "rename-card",
            BoardOp::Move { .. } => "move-card",
            BoardOp::Archive { .. } => "archive-card",
        }
    }
}

// ---------------------------------------------------------------------------
// The dashboard's own modes and clipboard
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoardMode {
    Board,
    Insert,
}

/// The board's own clipboard. Cards only: a column head is not focusable, so
/// there is no gesture that could put one here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Clip {
    Card(CardData),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    Card(Id),
}

/// An open insert-mode edit.
///
/// `creating` distinguishes "renaming something that exists" from "typing into
/// something this keypress just made". The difference matters on commit: an empty
/// rename is a rename to nothing, but an empty creation is a cancelled one, and
/// leaving a blank card behind is not what the user asked for.
#[derive(Debug, Clone)]
struct EditState {
    target: EditTarget,
    text: String,
    /// Caret position as a byte offset into `text`.
    caret: usize,
    /// The column a run of Up/Down aims for. See [`input::vertical`].
    goal_col: Option<usize>,
    original: String,
    creating: bool,
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

pub struct ProjectManagementProvider {
    board: Board,
    /// The column the list cursor has descended into, if any. A board is two
    /// levels deep, so this and `in_meta` are the whole path.
    open_column: Option<Id>,
    /// Inside the list meta of the open column, or of the board at the top.
    in_meta: bool,
    rendered_path: String,
    /// Displayed label back to the id it names, per level. `push_path` is handed
    /// the label the user was looking at, not an id.
    labels: HashMap<Option<Id>, HashMap<String, Id>>,
    /// Per-instance store location, for the tests. In sicompass the store is
    /// the plugin's own folder, [`sicompass_sdk::plugin::storage_dir`].
    root_override: Option<PathBuf>,
    loaded: bool,
    load_failed: bool,
    error: Option<String>,
    announcement: Option<String>,
    refresh: bool,
    timeline: Vec<ProviderOp>,
    /// Text taken by `commit_edit` for a row the app has not told us about yet.
    pending_create: Option<String>,
    /// Opt-in sync of the board through Sicompass Cloud. Inert until the user
    /// ticks "enable cloud sync" in the board's settings; see [`cloud`].
    cloud: Cloud,
    /// The host calls the cloud needs, injectable so the tests run natively.
    host: Box<dyn CloudHost>,

    // ---- Dashboard ------------------------------------------------------
    mode: BoardMode,
    focus: Focus,
    edit: Option<EditState>,
    clip: Option<Clip>,
    /// The letter that opened insert mode, swallowed once. See `dashboard_text`.
    suppress_text: Option<String>,
    /// Where the list cursor should land, queued when the board is left.
    navigation: Option<NavigationRequest>,
    /// Where the list cursor was when the board was entered, handed over by the
    /// app just before `enter_dashboard`.
    entry_path: Vec<usize>,
    /// The app's live palette, refreshed before every frame. Its default is the
    /// app's dark theme, so a provider drawn before the app has handed one over
    /// still draws in real colours.
    palette: sicompass_sdk::DashboardPalette,
    /// Width of the last frame drawn, so Up/Down in a card follow the same
    /// wrapping the user sees. Zero before the first frame.
    board_cols: u16,
    dashboard_request: Option<DashboardRequest>,
    /// True between `enter_dashboard` and `leave_dashboard`.
    ///
    /// A colon command dispatched from the board arrives through the very same
    /// `handle_command` as one dispatched from the list, and the `elem_key` it
    /// carries comes from the *list* cursor, which is wherever the user left it
    /// before pressing `d`. This is what tells the two apart, so the board acts
    /// on its own `focus` instead.
    ///
    /// It is load-bearing that opening the palette does not disturb it: the app's
    /// `handle_colon` only swaps its own coordinate and never calls
    /// `leave_dashboard`, so the flag is still set when the command is dispatched.
    in_dashboard: bool,
}

impl ProjectManagementProvider {
    pub fn with_host(host: Box<dyn CloudHost>) -> Self {
        ProjectManagementProvider {
            board: Board::new(),
            open_column: None,
            in_meta: false,
            rendered_path: String::new(),
            labels: HashMap::new(),
            root_override: None,
            loaded: false,
            load_failed: false,
            error: None,
            announcement: None,
            refresh: false,
            timeline: Vec::new(),
            pending_create: None,
            cloud: Cloud::new(cloud::SERVICE),
            host,
            mode: BoardMode::Board,
            focus: Focus::default(),
            edit: None,
            clip: None,
            suppress_text: None,
            navigation: None,
            entry_path: Vec::new(),
            palette: sicompass_sdk::DashboardPalette::default(),
            board_cols: 0,
            dashboard_request: None,
            in_dashboard: false,
        }
    }

    /// Point the store somewhere else (the tests use a temporary directory).
    pub fn set_root(&mut self, path: PathBuf) {
        self.root_override = Some(path);
        self.loaded = false;
        self.load_failed = false;
    }

    /// Where the board lives. `None` means nowhere usable, and the board stays in
    /// memory for the session rather than being silently discarded.
    ///
    /// In sicompass that is the plugin's own folder, which the app creates in
    /// the data directory (not the state directory: on macOS that is
    /// `~/Library/Logs`, which cleanup tools treat as disposable, and a board is
    /// a document). Outside sicompass, in the unit tests, nothing unless a test
    /// set one: a test that forgot must fail closed, never reach a real board.
    fn root(&self) -> Option<PathBuf> {
        if let Some(p) = &self.root_override {
            return Some(p.clone());
        }
        sicompass_sdk::plugin::storage_dir()
    }

    pub fn at_root(&self) -> bool {
        self.open_column.is_none() && !self.in_meta
    }

    pub fn needs_refresh(&self) -> bool {
        self.refresh || self.cloud.needs_refresh()
    }

    pub fn clear_needs_refresh(&mut self) {
        self.refresh = false;
        self.cloud.clear_needs_refresh();
    }

    pub fn take_error(&mut self) -> Option<String> {
        // The board's own error leads: a board that could not be saved matters
        // more than a sync that could not run.
        self.error.take().or_else(|| self.cloud.take_error())
    }

    pub fn take_announcement(&mut self) -> Option<String> {
        self.announcement
            .take()
            .or_else(|| self.cloud.take_announcement())
    }

    pub fn take_dashboard_request(&mut self) -> Option<DashboardRequest> {
        self.dashboard_request.take()
    }

    pub fn take_navigation_request(&mut self) -> Option<NavigationRequest> {
        self.navigation.take()
    }

    /// The board, drawn at `cols` by `rows`, in the renderer's own types.
    pub fn render_frame(&mut self, cols: u16, rows: u16) -> sicompass_sdk::DashboardFrame {
        self.ensure_loaded();
        self.clamp_focus();
        self.board_cols = cols;
        let editing = self.edit.as_ref().map(|e| (e.text.as_str(), e.caret));
        // Resolved here rather than inside the renderer: `render` is pure drawing
        // and has no business reaching the localizer.
        let empty_label = localize::t("projectmanagement-board-empty-slot");
        let no_columns_label = localize::t("projectmanagement-board-no-columns");
        let view = View {
            focus: self.focus,
            editing,
            empty_label: &empty_label,
            no_columns_label: &no_columns_label,
            palette: self.palette,
        };
        render::render(&self.board, &view, cols, rows)
    }

    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(root) = self.root() else {
            return;
        };
        match store::load_board(&root) {
            Some(mut b) => {
                // Normalise on the way in, not only on the way out. Stripping a
                // trailing colon when a title is *written* leaves every title
                // written before that fix still carrying one, and the board would
                // go on showing it until the user happened to rename the column.
                // Doing it here heals what is already on disk, and the next save
                // persists the clean form.
                for c in b.columns.iter_mut() {
                    c.title = column_title(&c.title);
                }
                self.board = b;
                // A store last written by a build without the archive flag can
                // have the archive sitting anywhere. Heal it on the way in, the
                // same way the trailing colons above are healed.
                self.board.pin_archive_last();
            }
            None => {
                // Not an empty board. Refusing to write is the whole point: a
                // save would reconcile the directory against a board that failed
                // to load and delete what could not be read.
                self.load_failed = true;
                self.error = Some(localize::t("projectmanagement-error-unreadable"));
            }
        }
        self.cloud.load_base(&root);
    }

    fn persist(&mut self) {
        if self.load_failed {
            return;
        }
        let Some(root) = self.root() else {
            return;
        };
        if store::save_board(&root, &self.board).is_err() {
            self.error = Some(localize::t("projectmanagement-error-save"));
            // The disk write failed, so there is no new state worth syncing.
            return;
        }
        // Queues only. Every board edit lands here, so the sync itself is a
        // background task, started from `poll` once the board is quiet.
        self.cloud.mark_dirty(&*self.host);
    }

    // ---- Row rendering --------------------------------------------------

    /// A row: `<id>N</id><input>text</input>`.
    fn row_label(id: Id, text: &str) -> String {
        format!(
            "{}{}",
            tags::format_id(&id.to_string()),
            tags::format_input(&escape::escape(text))
        )
    }

    fn remember(&mut self, level: Option<Id>, label: &str, id: Id) {
        let entry = self.labels.entry(level).or_default();
        entry.insert(label.to_owned(), id);
        entry.insert(tags::strip_display(label), id);
    }

    // ---- The list meta ----------------------------------------------------

    /// The `list meta:` row's text.
    ///
    /// Localized, and therefore never literally `"meta"`: the app special-cases
    /// an Obj keyed exactly `"meta"` and skips `pop_path` when leaving it,
    /// which would leave this provider's path one segment deeper than the
    /// cursor.
    fn meta_label() -> String {
        localize::t("projectmanagement-list-meta")
    }

    /// Rows the app shows above the board's own in the list the cursor is on:
    /// the cloud row (columns list, sync on) and the list meta. Every index the
    /// app counts in `fetch()` rows is off by this much from a position in
    /// [`Board::columns`] or a column's cards.
    fn lead_rows(&self, columns_list: bool) -> usize {
        1 + usize::from(columns_list && self.cloud.is_enabled())
    }

    /// Inside the list meta: the Merkle hash of the board (at the top) or of
    /// the open column, and, with cloud sync on, whether that is still what
    /// the last sync agreed on.
    fn meta_children(&self) -> Vec<FfonElement> {
        let (id, hash) = match self.open_column.and_then(|c| self.board.column(c)) {
            Some(c) => (Some(c.id), c.hash_hex()),
            None => (None, self.board.root_hash_hex()),
        };
        let status = self.cloud.sync_status(id, &hash, &*self.host);
        let mut args = localize::Args::new();
        args.set("hash", hash);
        let mut out = vec![FfonElement::new_str(localize::t_args(
            "projectmanagement-sha256",
            &args,
        ))];
        if let Some(line) = status {
            out.push(FfonElement::new_str(line));
        }
        out
    }

    /// The rows for the level the cursor is on.
    fn level_children(&mut self) -> Vec<FfonElement> {
        // Before forgetting the level's labels: inside the meta the level is
        // still the open column's, and its rows are wanted again on the way out.
        if self.in_meta {
            return self.meta_children();
        }
        let level = self.open_column;
        self.labels.remove(&level);

        let rows: Vec<(Id, String, bool)> = match level {
            None => self
                .board
                .columns
                .iter()
                .map(|c| (c.id, c.title.clone(), true))
                .collect(),
            Some(col) => match self.board.column(col) {
                Some(c) => c
                    .cards
                    .iter()
                    .map(|k| (k.id, k.text.clone(), false))
                    .collect(),
                // The column was deleted in another tab. An empty level is
                // right; the app's Left will take the cursor back out.
                None => Vec::new(),
            },
        };

        let rows_len = rows.len();
        let mut out = Vec::with_capacity(rows_len.max(1) + 1);
        // Only on the columns level, and only when the switch is on: one row
        // per provider, in one place. The board itself is listed below it
        // whether or not the subscription is paid for.
        if level.is_none()
            && let Some(row) = cloud::row(&self.cloud, &*self.host)
        {
            out.push(row);
        }
        // Every list opens with its meta, the columns list included.
        out.push(FfonElement::new_obj(Self::meta_label()));
        for (id, text, is_column) in rows {
            let label = Self::row_label(id, &text);
            self.remember(level, &label, id);
            out.push(if is_column {
                FfonElement::new_obj(label)
            } else {
                FfonElement::Str(label)
            });
        }

        // Never empty. The app seeds its own insert placeholder into an empty
        // level, which would then look like a row this provider had rendered.
        //
        // Counted against the board's own rows, not against `out`: with cloud
        // backup on, `out` already holds the cloud row, and an empty board
        // would otherwise lose its "no columns yet" line.
        if rows_len == 0 {
            out.push(FfonElement::new_str(localize::t(if level.is_none() {
                "projectmanagement-empty-columns"
            } else {
                "projectmanagement-empty-cards"
            })));
        }
        out
    }

    fn sync_rendered_path(&mut self) {
        let column = match self.open_column {
            None => String::new(),
            Some(id) => format!("/c{id}"),
        };
        self.rendered_path = if self.in_meta {
            format!("{column}/m")
        } else {
            column
        };
    }

    // ---- The write path -------------------------------------------------

    /// Reconcile one list against what the app says it now holds.
    ///
    /// Rows carry their `<id>`, so this is a diff rather than a guess: a row with
    /// a known id keeps its node and takes the new text, a row with no id is new,
    /// and a node whose row is gone was deleted. Order is the order the app gave.
    fn reconcile(&mut self, children: &[FfonElement]) {
        if self.load_failed {
            self.error = Some(localize::t("projectmanagement-error-unreadable"));
            return;
        }

        // Rows this provider renders but does not store. The app hands back
        // whatever it was displaying, so without this the "no columns yet" line
        // would become the user's first column the moment they made a second.
        let rendered_only = [
            localize::t("projectmanagement-empty-columns"),
            localize::t("projectmanagement-empty-cards"),
            Self::meta_label(),
        ];

        // Which list is this? Not necessarily the one the cursor is on: undo and
        // redo hand back a list from wherever the reversed edit happened, and
        // they do not move the provider's path to match. The app cannot supply it
        // either — a provider's path is not in step with FFON depth.
        //
        // The rows answer it themselves. Any row with an id names something, and
        // that thing's owner is the list. Only a list of nothing but new rows
        // falls back to the cursor, which is right: a list of nothing but new
        // rows can only be the one the user is typing into.
        let ids: Vec<Id> = children.iter().filter_map(|e| row_id(raw_of(e))).collect();
        let target = ids
            .iter()
            .find_map(|id| {
                if self.board.column_index(*id).is_some() {
                    Some(None)
                } else {
                    self.board
                        .locate_card(*id)
                        .map(|(ci, _)| Some(self.board.columns[ci].id))
                }
            })
            .unwrap_or(self.open_column);

        match target {
            None => self.reconcile_columns(children, &rendered_only),
            Some(col) => self.reconcile_cards(col, children, &rendered_only),
        }
        self.persist();
    }

    fn reconcile_columns(&mut self, children: &[FfonElement], rendered_only: &[String]) {
        let mut rebuilt: Vec<Column> = Vec::new();
        for elem in children {
            let raw = raw_of(elem);
            if rendered_only.iter().any(|r| r == raw) {
                continue;
            }
            // The cloud row is rendered, never stored. Matched by its `<id>`
            // rather than by text, because its wording changes with the
            // subscription and with the user's language. No column can claim
            // that id: column ids are numbers, and `row_id` reads only the
            // prefix this plugin wrote.
            if cloud::is_row(raw) {
                continue;
            }
            let text = column_title(&row_text(raw));
            match row_id(raw) {
                Some(id) => {
                    let existing = self.board.column(id).cloned();
                    match existing {
                        Some(mut c) => {
                            c.title = text;
                            rebuilt.push(c);
                        }
                        None => rebuilt.push(Column::new(id, text)),
                    }
                }
                None => {
                    let text = column_title(&self.resolve_new_text(text));
                    // A row the app is still showing as its blank placeholder is
                    // not a column. Only a committed one has text.
                    if text.is_empty() {
                        continue;
                    }
                    let id = self.board.mint_id();
                    rebuilt.push(Column::new(id, text));
                }
            }
        }
        self.board.columns = rebuilt;
        self.board.reseat_counter();
        // The app hands the rows back in *its* order, and the user may have
        // deleted the archive column outright. Both halves of the invariant are
        // re-established here: a dangling id is forgotten, and a surviving
        // archive goes back to last so the board still cannot see it.
        self.board.pin_archive_last();
    }

    fn reconcile_cards(&mut self, col: Id, children: &[FfonElement], rendered_only: &[String]) {
        let Some(ci) = self.board.column_index(col) else {
            return;
        };
        let mut rebuilt: Vec<Card> = Vec::new();
        for elem in children {
            let raw = raw_of(elem);
            if rendered_only.iter().any(|r| r == raw) {
                continue;
            }
            let text = row_text(raw);
            match row_id(raw) {
                Some(id) => {
                    // The card may be moving in from another column, so look it
                    // up board-wide rather than only in this one.
                    match self.board.card(id).cloned() {
                        Some(mut k) => {
                            k.text = text;
                            rebuilt.push(k);
                        }
                        None => rebuilt.push(Card::new(id, text)),
                    }
                }
                None => {
                    let text = self.resolve_new_text(text);
                    if text.is_empty() {
                        continue;
                    }
                    let id = self.board.mint_id();
                    rebuilt.push(Card::new(id, text));
                }
            }
        }
        // A card that moved into this column has to leave the one it came from,
        // or the same id would exist twice and `locate_card` would find the stale
        // copy first.
        let moved: Vec<Id> = rebuilt.iter().map(|k| k.id).collect();
        for (i, c) in self.board.columns.iter_mut().enumerate() {
            if i != ci {
                c.cards.retain(|k| !moved.contains(&k.id));
            }
        }
        self.board.columns[ci].cards = rebuilt;
        self.board.reseat_counter();
    }

    /// The text for a brand-new row: what `commit_edit` took, when the tree the
    /// app hands back still holds its blank placeholder and cannot say.
    fn resolve_new_text(&mut self, from_row: String) -> String {
        if !from_row.is_empty() {
            self.pending_create = None;
            return from_row;
        }
        self.pending_create.take().unwrap_or_default()
    }

    // ---- Board mutation, with a timeline entry --------------------------

    fn record(&mut self, op: BoardOp) {
        let label = localize::t(&format!("projectmanagement-op-{}", op.command()));
        let payload = serde_json::to_string(&op).unwrap_or_default();
        self.timeline.push(ProviderOp {
            command: op.command().to_owned(),
            payload: sicompass_sdk::plugin::encode_one(&FfonElement::Str(payload)),
            label,
        });
    }

    /// Apply an op, or its inverse.
    ///
    /// One function for both directions so the two can never drift: a redo that
    /// is not exactly the undo's inverse is the classic way an undo history stops
    /// matching what is on screen.
    fn apply(&mut self, op: &BoardOp, forward: bool) {
        match op {
            BoardOp::Add {
                column,
                index,
                card,
            } => {
                if forward {
                    self.insert_card(*column, *index, card.clone());
                } else {
                    self.remove_card(card.id);
                }
            }
            BoardOp::Delete {
                column,
                index,
                card,
            } => {
                if forward {
                    self.remove_card(card.id);
                } else {
                    self.insert_card(*column, *index, card.clone());
                }
            }
            BoardOp::Rename { id, before, after } => {
                let want = if forward { after } else { before };
                if let Some((ci, ki)) = self.board.locate_card(*id) {
                    self.board.columns[ci].cards[ki].text = want.clone();
                }
            }
            BoardOp::Move {
                card,
                from_column,
                from_index,
                to_column,
                to_index,
            }
            | BoardOp::Archive {
                card,
                from_column,
                from_index,
                to_column,
                to_index,
            } => {
                // One arm for both, so an archive and a move can never come to
                // disagree about what reversing one means. They differ only in
                // the label the timeline shows.
                //
                // Where it came from is not named: the card is lifted from
                // wherever it actually is, so a stale `from_*` can never make the
                // two disagree. Only the destination differs by direction.
                let (dst, di) = if forward {
                    (*to_column, *to_index)
                } else {
                    (*from_column, *from_index)
                };
                if let Some((ci, ki)) = self.board.locate_card(*card) {
                    let taken = self.board.columns[ci].cards.remove(ki);
                    if let Some(target) = self.board.column_index(dst) {
                        let at = di.min(self.board.columns[target].cards.len());
                        self.board.columns[target].cards.insert(at, taken);
                    } else {
                        // The destination column is gone. Put it back rather than
                        // dropping the card on the floor.
                        self.board.columns[ci].cards.insert(ki, taken);
                    }
                }
            }
        }
        self.board.reseat_counter();
        self.refresh = true;
        self.persist();
    }

    fn insert_card(&mut self, column: Id, index: usize, card: CardData) {
        if let Some(ci) = self.board.column_index(column) {
            let at = index.min(self.board.columns[ci].cards.len());
            self.board.columns[ci]
                .cards
                .insert(at, Card::new(card.id, card.text));
        }
    }

    fn remove_card(&mut self, id: Id) {
        if let Some((ci, ki)) = self.board.locate_card(id) {
            self.board.columns[ci].cards.remove(ki);
        }
    }

    // ---- Announcements --------------------------------------------------

    fn say(&mut self, text: String) {
        self.announcement = Some(text);
    }

    fn say_key(&mut self, key: &str, args: &[(&str, String)]) {
        let mut a = localize::Args::new();
        for (k, v) in args {
            a.set(k, v.clone());
        }
        self.announcement = Some(localize::t_args(key, &a));
    }

    /// Describe whatever the board cursor is now on.
    ///
    /// The column is named on every card, not only when the cursor crosses into
    /// a new one. Left and Right change columns without changing the card's
    /// position in its own list, so "card 2 of 4" alone would leave a listener
    /// unable to tell a sideways move from a vertical one.
    fn say_focus(&mut self) {
        // The visible count, so "column 2 of 3" never counts an archive the
        // listener cannot see or reach.
        let total = self.board.visible_len();
        let Some(col) = self.board.columns.get(self.focus.col) else {
            self.say(localize::t("projectmanagement-empty-columns"));
            return;
        };
        let title = col.title.clone();
        let cards = col.cards.len();
        if cards == 0 {
            self.say_key(
                "projectmanagement-say-column-empty",
                &[
                    ("index", (self.focus.col + 1).to_string()),
                    ("total", total.to_string()),
                    ("title", title),
                ],
            );
            return;
        }
        let i = self.focus.row.min(cards - 1);
        let text = col.cards[i].text.clone();
        self.say_key(
            "projectmanagement-say-card",
            &[
                ("column", title),
                ("index", (i + 1).to_string()),
                ("total", cards.to_string()),
                ("text", text),
            ],
        );
    }

    fn say_edit(&mut self) {
        let text = self
            .edit
            .as_ref()
            .map(|e| e.text.clone())
            .unwrap_or_default();
        if text.is_empty() {
            self.say(localize::t("projectmanagement-say-insert-empty"));
        } else {
            self.say_key("projectmanagement-say-insert", &[("text", text)]);
        }
    }

    // ---- Board navigation -----------------------------------------------

    /// True when the cursor is on an empty column's placeholder rather than a
    /// real card. Insert acts, delete and copy do not: there is nothing there.
    fn on_placeholder(&self) -> bool {
        render::is_placeholder(&self.board, self.focus.col)
    }

    /// Keep the cursor on something that exists after the board changed under it.
    /// This is also the single line that keeps the archive off the board.
    /// Clamping to `visible_len` means `focus.col` can never index the archive
    /// column, which is what lets `slots`, `is_placeholder`, `focused_column_id`,
    /// `begin_new_card` and `paste` go on indexing `columns` directly.
    fn clamp_focus(&mut self) {
        if self.board.visible_len() == 0 {
            self.focus = Focus::default();
            return;
        }
        self.focus.col = self.focus.col.min(self.board.visible_len() - 1);
        // `slots` is one for an empty column, because its placeholder is a real
        // focus target: it is the only thing to stand on while adding the first
        // card.
        let n = render::slots(&self.board, self.focus.col);
        self.focus.row = self.focus.row.min(n.saturating_sub(1));
    }

    fn move_column(&mut self, delta: isize) -> bool {
        if self.board.visible_len() == 0 {
            return false;
        }
        let next = self.focus.col as isize + delta;
        // Right from the last real column says "no further" rather than stepping
        // onto the archive.
        if next < 0 || next as usize >= self.board.visible_len() {
            self.say(localize::t("projectmanagement-say-edge"));
            return true;
        }
        self.focus.col = next as usize;
        // The row is kept where it can be: walking sideways along a rank of cards
        // is the gesture, and snapping to the top every time would undo it.
        self.clamp_focus();
        self.say_focus();
        true
    }

    fn move_row(&mut self, down: bool) -> bool {
        let n = render::slots(&self.board, self.focus.col);
        let cur = self.focus.row;
        let next = if down {
            (cur + 1).min(n.saturating_sub(1))
        } else {
            cur.saturating_sub(1)
        };
        if next == cur {
            self.say(localize::t("projectmanagement-say-edge"));
            return true;
        }
        self.focus.row = next;
        self.say_focus();
        true
    }

    // ---- Board editing --------------------------------------------------

    fn focused_column_id(&self) -> Option<Id> {
        self.board.columns.get(self.focus.col).map(|c| c.id)
    }

    /// Open insert mode on the focused card.
    ///
    /// `at_end` is the only difference between `i` and `a`, exactly as it is in
    /// the app's own `handle_i` / `handle_a`: both edit the item the cursor is
    /// on, one with the caret at the start and one at the end.
    ///
    /// On an empty column's placeholder there is nothing to edit, so both start a
    /// new card instead. That mirrors the list, where an empty level is an
    /// `<input>` slot and typing into it is how the first row appears.
    fn begin_rename(&mut self, at_end: bool) {
        if self.on_placeholder() {
            self.begin_new_card(0);
            return;
        }
        let Some(card) = self
            .board
            .columns
            .get(self.focus.col)
            .and_then(|c| c.cards.get(self.focus.row))
        else {
            return;
        };
        let original = card.text.clone();
        let target = EditTarget::Card(card.id);
        let caret = if at_end { original.len() } else { 0 };
        self.edit = Some(EditState {
            target,
            text: original.clone(),
            caret,
            goal_col: None,
            original,
            creating: false,
        });
        self.mode = BoardMode::Insert;
        self.say_edit();
    }

    /// Index for "a new card above this one".
    ///
    /// Zero on an empty column's placeholder: there is no card to be above, and
    /// the new one is simply the first.
    fn above(&self) -> usize {
        if self.on_placeholder() {
            0
        } else {
            self.focus.row
        }
    }

    /// Index for "a new card below this one".
    fn below(&self) -> usize {
        if self.on_placeholder() {
            0
        } else {
            self.focus.row + 1
        }
    }

    /// Make an empty card at `index` in the focused column and type into it.
    fn begin_new_card(&mut self, index: usize) {
        let Some(ci) = self
            .board
            .columns
            .get(self.focus.col)
            .map(|_| self.focus.col)
        else {
            self.error = Some(localize::t("projectmanagement-error-no-column"));
            return;
        };
        let id = self.board.mint_id();
        let at = index.min(self.board.columns[ci].cards.len());
        self.board.columns[ci].cards.insert(at, Card::new(id, ""));
        self.focus.row = at;
        self.edit = Some(EditState {
            target: EditTarget::Card(id),
            text: String::new(),
            caret: 0,
            goal_col: None,
            original: String::new(),
            creating: true,
        });
        self.mode = BoardMode::Insert;
        self.say_edit();
    }

    /// Close insert mode, keeping what was typed.
    ///
    /// A creation typed into and then left empty is a cancelled creation, not a
    /// blank card: it is removed and no timeline entry is recorded, so Ctrl+Z
    /// does not step through edits that left no trace.
    fn commit_edit_state(&mut self) {
        let Some(edit) = self.edit.take() else {
            self.mode = BoardMode::Board;
            return;
        };
        self.mode = BoardMode::Board;
        let text = edit.text.trim().to_owned();

        if edit.creating && text.is_empty() {
            match edit.target {
                EditTarget::Card(id) => self.remove_card(id),
            }
            self.clamp_focus();
            self.say(localize::t("projectmanagement-say-board"));
            self.persist();
            return;
        }

        match edit.target {
            EditTarget::Card(id) => {
                if let Some((ci, ki)) = self.board.locate_card(id) {
                    self.board.columns[ci].cards[ki].text = text.clone();
                    if edit.creating {
                        let column = self.board.columns[ci].id;
                        self.record(BoardOp::Add {
                            column,
                            index: ki,
                            card: CardData { id, text },
                        });
                    } else if text != edit.original {
                        self.record(BoardOp::Rename {
                            id,
                            before: edit.original,
                            after: text,
                        });
                    }
                }
            }
        }
        self.refresh = true;
        self.persist();
        self.say(localize::t("projectmanagement-say-board"));
    }

    /// Delete the focused card.
    ///
    /// A no-op on an empty column's placeholder: it looks like a slot so the
    /// cursor has somewhere to stand, but there is nothing behind it to remove.
    fn delete_focused(&mut self) {
        if self.on_placeholder() {
            return;
        }
        let Some(col) = self.board.columns.get(self.focus.col) else {
            return;
        };
        let Some(card) = col.cards.get(self.focus.row) else {
            return;
        };
        let text = card.text.clone();
        let op = BoardOp::Delete {
            column: col.id,
            index: self.focus.row,
            card: CardData {
                id: card.id,
                text: card.text.clone(),
            },
        };
        self.apply(&op, true);
        self.record(op);
        self.clamp_focus();
        self.say_key("projectmanagement-say-deleted", &[("text", text)]);
    }

    fn copy_focused(&mut self, cut: bool) {
        if self.on_placeholder() {
            return;
        }
        let Some(card) = self
            .board
            .columns
            .get(self.focus.col)
            .and_then(|c| c.cards.get(self.focus.row))
        else {
            return;
        };
        let text = card.text.clone();
        self.clip = Some(Clip::Card(CardData {
            id: card.id,
            text: card.text.clone(),
        }));
        if cut {
            self.delete_focused();
            self.say_key("projectmanagement-say-cut", &[("text", text)]);
        } else {
            self.say_key("projectmanagement-say-copied", &[("text", text)]);
        }
    }

    /// Paste the board clipboard after the focus.
    ///
    /// Always with a **fresh id**: ids are minted once and never reused, so a
    /// pasted copy is a new card, not a second row claiming to be the original.
    /// Without this, `locate_card` would find whichever copy came first and every
    /// later edit would land on the wrong one.
    fn paste(&mut self) {
        let Some(Clip::Card(card)) = self.clip.clone() else {
            self.error = Some(localize::t("projectmanagement-error-nothing-to-paste"));
            return;
        };
        let Some(column) = self.focused_column_id() else {
            self.error = Some(localize::t("projectmanagement-error-no-column"));
            return;
        };
        // Onto an empty column's placeholder the card becomes the first one;
        // otherwise it lands below the card the cursor is on.
        let index = if self.on_placeholder() {
            0
        } else {
            self.focus.row + 1
        };
        let op = BoardOp::Add {
            column,
            index,
            card: CardData {
                id: self.board.mint_id(),
                text: card.text.clone(),
            },
        };
        self.apply(&op, true);
        self.record(op);
        self.focus.row = index;
        self.clamp_focus();
        self.say_key("projectmanagement-say-pasted", &[("text", card.text)]);
    }

    /// Turn pasted system-clipboard text into one card per non-blank line.
    fn paste_text(&mut self, text: &str) {
        let Some(column) = self.focused_column_id() else {
            self.error = Some(localize::t("projectmanagement-error-no-column"));
            return;
        };
        let mut index = if self.on_placeholder() {
            0
        } else {
            self.focus.row + 1
        };
        let mut last = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let op = BoardOp::Add {
                column,
                index,
                card: CardData {
                    id: self.board.mint_id(),
                    text: line.to_owned(),
                },
            };
            self.apply(&op, true);
            self.record(op);
            last = line.to_owned();
            index += 1;
        }
        if !last.is_empty() {
            self.focus.row = index - 1;
            self.clamp_focus();
            self.say_key("projectmanagement-say-pasted", &[("text", last)]);
        }
    }

    // ---- Insert-mode text editing ---------------------------------------

    /// The open card's visual lines, wrapped exactly as the last frame drew it.
    fn edit_lines(&self) -> Vec<InputLine> {
        let Some(edit) = self.edit.as_ref() else {
            return Vec::new();
        };
        if self.board_cols == 0 || self.board.visible_len() == 0 {
            return input::hard_lines(&edit.text);
        }
        // The same count `render` lays out with, so a card wraps while it is
        // edited exactly as it was drawn.
        let lay = render::layout(self.board.visible_len(), self.focus.col, self.board_cols);
        render::card_lines(&edit.text, lay.width_of(self.focus.col))
    }

    /// Apply one operation of the shared text-field model to the open card, so
    /// a card edits, wraps and moves like every `<input>` in the app.
    fn edit_with(&mut self, op: impl FnOnce(&mut InputState, &[InputLine])) {
        let lines = self.edit_lines();
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        let mut field = InputState {
            text: std::mem::take(&mut edit.text),
            caret: edit.caret,
            anchor: None,
            goal_col: edit.goal_col,
        };
        op(&mut field, &lines);
        edit.text = field.text;
        edit.caret = field.caret;
        edit.goal_col = field.goal_col;
    }

    fn insert_text(&mut self, s: &str) {
        self.edit_with(|f, _| f.insert_str(s));
    }

    fn backspace(&mut self) {
        self.edit_with(|f, _| f.backspace());
    }

    fn delete_forward(&mut self) {
        self.edit_with(|f, _| f.delete_forward());
    }

    // ---- Command handling (colon commands, list side) --------------------

    fn move_row_in_list(&mut self, down: bool, key: &str) -> bool {
        let Some(id) = self
            .labels
            .get(&self.open_column)
            .and_then(|m| m.get(key))
            .copied()
        else {
            return false;
        };
        match self.open_column {
            None => {
                // The archive is pinned last and stays there. Moving it is the
                // one reorder that would put it back on the board.
                if self.board.is_archive(id) {
                    return false;
                }
                let Some(i) = self.board.column_index(id) else {
                    return false;
                };
                let j = if down { i + 1 } else { i.wrapping_sub(1) };
                // `visible_len`, so a column cannot be pushed past the archive
                // either -- the two guards together keep it last from both sides.
                if down && j >= self.board.visible_len() || !down && i == 0 {
                    return false;
                }
                self.board.columns.swap(i, j);
            }
            Some(col) => {
                let Some(ci) = self.board.column_index(col) else {
                    return false;
                };
                let Some(i) = self.board.columns[ci].cards.iter().position(|k| k.id == id) else {
                    return false;
                };
                let j = if down { i + 1 } else { i.wrapping_sub(1) };
                if down && j >= self.board.columns[ci].cards.len() || !down && i == 0 {
                    return false;
                }
                self.board.columns[ci].cards.swap(i, j);
            }
        }
        self.persist();
        self.refresh = true;
        true
    }

    /// Move the card named by `key` to the adjacent column.
    ///
    /// The one kanban verb the generic keymap has no key for: every other edit is
    /// covered by insert, delete and cut/paste, but "this is done now" is the
    /// motion the board exists for.
    fn move_card_sideways(&mut self, right: bool, key: &str) -> bool {
        let Some(id) = self
            .labels
            .get(&self.open_column)
            .and_then(|m| m.get(key))
            .copied()
        else {
            return false;
        };
        let Some((ci, ki)) = self.board.locate_card(id) else {
            return false;
        };
        let target = if right { ci + 1 } else { ci.wrapping_sub(1) };
        // Rightward stops at the last *visible* column, so nothing is archived by
        // accident. Leftward is deliberately not bounded the same way: a card
        // filed by mistake walks back out of the archive this way, which is the
        // only route back and the reason no `unarchive` command is needed.
        if right && target >= self.board.visible_len() || !right && ci == 0 {
            return false;
        }
        let op = BoardOp::Move {
            card: id,
            from_column: self.board.columns[ci].id,
            from_index: ki,
            to_column: self.board.columns[target].id,
            to_index: self.board.columns[target].cards.len(),
        };
        self.apply(&op, true);
        self.record(op);
        true
    }

    /// Which card `archive card` acts on.
    ///
    /// Two callers of one command, and they name a card differently. From the
    /// board there is no row: the cursor is `focus`, and the `elem_key` the app
    /// passes names whatever the *list* cursor was on before `d` was pressed,
    /// which is not what the user is looking at. From the list the key is the
    /// row, resolved the way every other list command resolves one.
    fn card_to_archive(&self, elem_key: &str) -> Option<Id> {
        if self.in_dashboard {
            if self.on_placeholder() {
                return None;
            }
            return self
                .board
                .columns
                .get(self.focus.col)
                .and_then(|c| c.cards.get(self.focus.row))
                .map(|k| k.id);
        }
        self.labels
            .get(&self.open_column)
            .and_then(|m| m.get(elem_key))
            .copied()
            // A column row resolves to an id too, and a column is not a card.
            .filter(|id| self.board.locate_card(*id).is_some())
    }

    /// The archive column, made on first use.
    ///
    /// Silently, and only once a card has been found to put in it, so a command
    /// that turns out to have nothing to archive never leaves an empty column
    /// behind. A fresh board therefore has no archive until the user asks for one.
    fn ensure_archive(&mut self) -> Id {
        if let Some(id) = self.board.archive_id() {
            return id;
        }
        let id = self.board.mint_id();
        self.board.columns.push(Column::new(
            id,
            localize::t("projectmanagement-archive-title"),
        ));
        self.board.set_archive(id);
        self.board.reseat_counter();
        id
    }

    /// Move a card into the archive.
    ///
    /// Reports through the announcement channel and never through `self.error`.
    /// An error would take the app down `handle_enter_command`'s error arm, which
    /// resets the coordinate to `rest_coordinate` -- General -- without ever
    /// calling `leave_dashboard`, leaving this provider believing it is still on
    /// a board the user can no longer see.
    ///
    /// Creating the archive column is deliberately **not** part of the recorded
    /// op. Undoing the first archive returns the card and leaves the empty
    /// archive column behind, because the column belongs to the list surface,
    /// where the app records its own `Structural` entries, and a second record of
    /// it here would be a second history of the same thing.
    fn archive_card(&mut self, elem_key: &str) {
        let Some(id) = self.card_to_archive(elem_key) else {
            self.say(localize::t("projectmanagement-say-nothing-to-archive"));
            return;
        };
        let Some((ci, ki)) = self.board.locate_card(id) else {
            return;
        };
        if self.board.is_archive(self.board.columns[ci].id) {
            self.say(localize::t("projectmanagement-say-already-archived"));
            return;
        }
        let from_column = self.board.columns[ci].id;
        let text = self.board.columns[ci].cards[ki].text.clone();
        let to_column = self.ensure_archive();
        let to_index = self
            .board
            .column(to_column)
            .map(|c| c.cards.len())
            .unwrap_or(0);
        let op = BoardOp::Archive {
            card: id,
            from_column,
            from_index: ki,
            to_column,
            to_index,
        };
        self.apply(&op, true);
        self.record(op);
        // The card left the column the board cursor was in, and creating the
        // archive may have been the first column on an empty board.
        self.clamp_focus();
        if !self.in_dashboard {
            // Put the list cursor back on the column the card came from, rather
            // than letting the app's state-toggle path unwind it to the board
            // root. Only from the list: a request queued while the app is in
            // Dashboard is drained and dropped, never deferred, and the board
            // owns its own cursor anyway.
            if let Some(at) = self.board.column_index(from_column) {
                let at = at + self.lead_rows(true);
                self.navigation = Some(NavigationRequest::SelectPath(vec![at as u32]));
            }
        }
        self.say_key("projectmanagement-say-archived", &[("text", text)]);
    }
}

// ---------------------------------------------------------------------------
// Row helpers
// ---------------------------------------------------------------------------

fn raw_of(elem: &FfonElement) -> &str {
    match elem {
        FfonElement::Str(s) => s.as_str(),
        FfonElement::Obj(o) => o.key.as_str(),
    }
}

/// The id of a row, read from the prefix before `<input>`.
///
/// Scoped to the prefix on purpose: a user who types `<id>9</id>` into a card
/// has it escaped on the way in, but reading the whole string would still be one
/// unescaping bug away from letting a card claim another card's identity.
fn row_id(raw: &str) -> Option<Id> {
    let prefix = match raw.find("<input>") {
        Some(at) => &raw[..at],
        None => raw,
    };
    tags::extract_id(prefix).and_then(|s| s.parse().ok())
}

/// A column title as it should be stored.
///
/// A trailing colon is the list's syntax for "this row is an object", not part of
/// the name: the app strips it the same way when a typed line creates an `Obj`
/// (`state::strip_trailing_colon`). Keeping it would store the punctuation and
/// then show it on the board, where there is no object convention to explain it.
fn column_title(text: &str) -> String {
    text.trim_end().trim_end_matches(':').trim_end().to_owned()
}

/// The text of a row, with the `<id>` prefix and `<input>` wrapper removed and
/// any escaping undone.
fn row_text(raw: &str) -> String {
    match tags::extract_input(raw) {
        Some(inner) => escape::unescape(&inner),
        None => tags::strip_display(raw),
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

impl Plugin for ProjectManagementProvider {
    fn new() -> Self {
        ProjectManagementProvider::with_host(Box::new(PluginHost::new()))
    }

    fn describe(&self) -> Descriptor {
        Descriptor {
            name: "projectmanagement".to_owned(),
            display_name: localize::t("projectmanagement-display-name"),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            supports_structural_edit: true,
            dashboard_kind: DashboardKind::Interactive,
            // The board's edits go onto the app's timeline as `ProviderOp`
            // entries, so Ctrl+Z belongs to the app here rather than being
            // forwarded as a keystroke this plugin would have to reimplement
            // against a second, divergent undo stack.
            dashboard_uses_app_undo: true,
            ..Default::default()
        }
    }

    /// Pick up the sync switch as the user left it, quietly: the "needs a
    /// subscription" notice is for the moment they switch it on.
    fn init(&mut self) {
        let on = sicompass_sdk::plugin::host::get_setting(cloud::ENABLE_KEY);
        self.cloud.restore_enabled(on.as_deref() == Some("true"));
    }

    fn fetch(&mut self) -> Vec<FfonElement> {
        self.ensure_loaded();
        self.level_children()
    }

    /// Every frame: start a sync once one is due, and hand over whatever
    /// needs saying or doing.
    fn poll(&mut self) -> PollResult {
        for (id, result) in self.host.finished() {
            self.task_done(id, result);
        }
        self.cloud.tick(&*self.host);
        let needs_refresh = self.needs_refresh();
        self.clear_needs_refresh();
        PollResult {
            at_root: self.at_root(),
            needs_refresh,
            is_busy: self.cloud.is_busy(),
            error: self.take_error(),
            announcement: self.take_announcement(),
            dashboard_request: self.take_dashboard_request(),
            navigation_request: self.take_navigation_request(),
            structural_edit_here: true,
            dashboard_here: true,
            ..Default::default()
        }
    }

    fn sync_ffon_body_children(&mut self, children: &[FfonElement]) {
        self.ensure_loaded();
        // The meta level holds rendered lines, not cards.
        if self.in_meta {
            return;
        }
        self.reconcile(children);
    }

    fn commit_edit(&mut self, old: &str, new: &str) -> bool {
        if self.load_failed {
            self.error = Some(localize::t("projectmanagement-error-unreadable"));
            return false;
        }
        if old == Self::meta_label() {
            self.error = Some(localize::t("projectmanagement-error-meta-readonly"));
            return false;
        }
        // Remembered rather than applied: the app has not yet handed back the
        // list this row belongs to, and the row itself still carries its blank
        // placeholder. `reconcile` picks this up when it does.
        self.pending_create = Some(row_text(new));
        true
    }

    fn delete_item(&mut self, name: &str) -> bool {
        // The veto on the FFON delete path. The trait default is `false`, which
        // reads as "always refuse", so a capability plugin has to answer.
        if self.load_failed {
            self.error = Some(localize::t("projectmanagement-error-unreadable"));
            return false;
        }
        if name == Self::meta_label() {
            self.error = Some(localize::t("projectmanagement-error-meta-undeletable"));
            return false;
        }
        // The sync row is the switch's, in settings, not a column.
        if cloud::is_row(name) {
            self.error = Some(localize::t("projectmanagement-error-cloud-row-undeletable"));
            return false;
        }
        true
    }

    fn push_path(&mut self, segment: &str) {
        // The meta holds lines, nothing to descend into.
        if self.in_meta {
            return;
        }
        // Its own label first: no column can carry it, because every
        // translation ends in a colon and `column_title` strips one.
        if segment == Self::meta_label() {
            self.in_meta = true;
            self.sync_rendered_path();
            return;
        }
        let column = if self.open_column.is_none() {
            self.labels.get(&None).and_then(|m| m.get(segment)).copied()
        } else {
            None
        };
        if column.is_none() && segment == "m" {
            self.in_meta = true;
            self.sync_rendered_path();
            return;
        }
        // Cards are leaves, so only the root level descends. Without this guard a
        // stale label from the card level could push a second segment and leave
        // the provider a level deeper than the cursor.
        if self.open_column.is_some() {
            return;
        }
        let resolved = column.or_else(|| segment.strip_prefix("c").and_then(|s| s.parse().ok()));
        if let Some(id) = resolved {
            self.open_column = Some(id);
            self.sync_rendered_path();
        }
    }

    fn pop_path(&mut self) {
        if self.in_meta {
            self.in_meta = false;
        } else {
            self.open_column = None;
        }
        self.sync_rendered_path();
    }

    fn current_path(&self) -> &str {
        &self.rendered_path
    }

    /// Accepts both forms the app hands back: the `c<id>` token
    /// [`Self::current_path`] renders, and a column's display text.
    ///
    /// The app derives the second one from the cursor —
    /// `sync_inmemory_provider_path_to_cursor` does it after every search jump,
    /// out of the display text of each ancestor row. Reading only the token
    /// form closed the open column, so the next edit re-fetched the board root
    /// over the column's listing and the cursor fell to its first row. Labels
    /// resolve the same way [`Self::push_path`] resolves them.
    fn set_current_path(&mut self, path: &str) {
        let mut seg = path.trim_start_matches('/');
        // A trailing list meta, as its token (`/c3/m`) or its label.
        let meta_label = Self::meta_label();
        let mut in_meta = false;
        for meta in ["m", meta_label.as_str()] {
            if seg == meta {
                seg = "";
                in_meta = true;
                break;
            }
            if let Some(rest) = seg.strip_suffix(meta).and_then(|r| r.strip_suffix('/')) {
                seg = rest;
                in_meta = true;
                break;
            }
        }
        if seg.is_empty() {
            self.open_column = None;
            self.in_meta = in_meta;
            self.sync_rendered_path();
            return;
        }
        // The label first, the token second — same order as `push_path`, and a
        // column may legitimately be titled `c3`. A segment that names neither
        // leaves the open column alone: only `"/"` means the board root, and
        // closing the column on a name we merely failed to recognise is the
        // very desync this method exists to avoid.
        let resolved = self
            .labels
            .get(&None)
            .and_then(|m| m.get(seg))
            .copied()
            .or_else(|| seg.strip_prefix('c').and_then(|s| s.parse().ok()));
        if let Some(id) = resolved {
            self.open_column = Some(id);
            self.in_meta = in_meta;
            self.sync_rendered_path();
        }
    }

    /// The children of the level the cursor is on, so a commit refreshes just
    /// that list. Without this the app falls back to rebuilding the provider
    /// root, which misroutes a descended path and leaves the level empty.
    fn fetch_subtree_children(&mut self) -> Option<Vec<FfonElement>> {
        if self.open_column.is_none() && !self.in_meta {
            return None;
        }
        self.ensure_loaded();
        Some(self.level_children())
    }

    /// The column row the cursor descended through, rendered exactly as
    /// `level_children` renders it on the board root.
    ///
    /// Two callers. `refresh_subtree_parent` re-keys the parent Obj with it, so
    /// renaming a column updates the column row and not only its cards. And
    /// `deep_rebuild_provider_tree` uses it to find that row when restoring a
    /// tab: it walks a saved path of *tokens* but matches rows by their display
    /// text, and no column is titled `c3`, so without this the descent stopped
    /// at the root and the tab reopened with the column closed.
    fn fetch_subtree_parent_key(&mut self) -> Option<String> {
        if self.in_meta {
            return Some(Self::meta_label());
        }
        let col = self.open_column?;
        self.ensure_loaded();
        self.board
            .column(col)
            .map(|c| Self::row_label(c.id, &c.title))
    }

    /// The sync switch. The host passes on only this plugin's own settings.
    fn on_setting_change(&mut self, key: &str, value: &str) {
        self.cloud.on_setting_change(key, value, &*self.host);
    }

    fn take_timeline_entries(&mut self) -> Vec<ProviderOp> {
        std::mem::take(&mut self.timeline)
    }

    fn undo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        if let Some((op, label)) = decode_op(entry) {
            self.apply(&op, false);
            self.clamp_focus();
            self.say_key("projectmanagement-say-undone", &[("what", label)]);
        }
        Ok(())
    }

    fn redo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        if let Some((op, label)) = decode_op(entry) {
            self.apply(&op, true);
            self.clamp_focus();
            self.say_key("projectmanagement-say-redone", &[("what", label)]);
        }
        Ok(())
    }

    fn commands(&self) -> Vec<String> {
        // On the board, only the archive. The four move verbs act on the list
        // cursor, which is stale here, and each returns a `FfonElement` on
        // success -- which sends the app down the splice-a-row-and-start-typing
        // path, resetting its coordinate to General without ever calling
        // `leave_dashboard`. The board would be gone and this provider would not
        // know. The motions themselves are not missing from the board: it has the
        // arrows and `h j k l` already.
        if self.in_dashboard {
            return vec![CMD_ARCHIVE.to_owned()];
        }
        let mut out = vec![
            CMD_MOVE_UP.to_owned(),
            CMD_MOVE_DOWN.to_owned(),
            CMD_MOVE_LEFT.to_owned(),
            CMD_MOVE_RIGHT.to_owned(),
            CMD_ARCHIVE.to_owned(),
        ];
        // Offered only when cloud sync is on: an inert command in the
        // palette is noise.
        if self.cloud.is_enabled() {
            out.push(CMD_SYNC_NOW.to_owned());
        }
        out
    }

    fn command_label(&self, cmd: &str) -> String {
        match cmd {
            CMD_MOVE_UP => localize::t("projectmanagement-cmd-move-up"),
            CMD_MOVE_DOWN => localize::t("projectmanagement-cmd-move-down"),
            CMD_MOVE_LEFT => localize::t("projectmanagement-cmd-move-left"),
            CMD_MOVE_RIGHT => localize::t("projectmanagement-cmd-move-right"),
            CMD_ARCHIVE => localize::t("projectmanagement-cmd-archive-card"),
            CMD_SYNC_NOW => localize::t("projectmanagement-cmd-sync-now"),
            other => other.to_owned(),
        }
    }

    fn handle_command(
        &mut self,
        cmd: &str,
        elem_key: &str,
        _elem_type: i32,
    ) -> Result<Option<FfonElement>, String> {
        self.ensure_loaded();
        // Always `None`, never an error. Both of the app's other return paths
        // reset the coordinate to `rest_coordinate` -- General for this provider
        // -- without calling `leave_dashboard`, which would strand the board.
        // `None` with no error and no secondary list takes the state-toggle arm
        // instead, which returns to whichever mode the palette was opened from.
        if cmd == CMD_ARCHIVE {
            self.archive_card(elem_key);
            return Ok(None);
        }
        if cmd == CMD_SYNC_NOW {
            self.cloud.start_sync(&*self.host);
            return Ok(None);
        }
        let ok = match cmd {
            CMD_MOVE_UP => self.move_row_in_list(false, elem_key),
            CMD_MOVE_DOWN => self.move_row_in_list(true, elem_key),
            CMD_MOVE_LEFT => self.move_card_sideways(false, elem_key),
            CMD_MOVE_RIGHT => self.move_card_sideways(true, elem_key),
            _ => false,
        };
        Ok(ok.then(|| FfonElement::new_str("")))
    }

    // ---- The board ------------------------------------------------------

    fn set_dashboard_palette(&mut self, palette: sicompass_sdk::plugin::Palette) {
        self.palette = to_sdk_palette(palette);
    }

    fn set_dashboard_entry(&mut self, path: &[u32]) {
        self.entry_path = path.iter().map(|&i| i as usize).collect();
    }

    fn enter_dashboard(&mut self) {
        self.ensure_loaded();
        self.mode = BoardMode::Board;
        self.in_dashboard = true;
        self.edit = None;
        self.suppress_text = None;
        // Open on exactly what the list cursor was standing on, so `d` continues
        // the user's train of thought. The mirror of what `leave_dashboard`
        // queues, and it has to come from the app: `current_path()` names the
        // column the user descended into, but nothing tells this provider which
        // row of it the cursor is on, because moving within a level never calls
        // `push_path`.
        //
        // A title has no card index, so the board opens on that column's first
        // card. Anything else is left where it was, and so is a row above the
        // columns (the cloud row, the list meta): the indices are the app's,
        // counted in `fetch()` rows, and those rows are not on the board.
        let lead = self.lead_rows(true);
        match std::mem::take(&mut self.entry_path).as_slice() {
            [col] if *col >= lead => {
                self.focus = Focus {
                    col: col - lead,
                    row: 0,
                }
            }
            [col, card, ..] if *col >= lead => {
                self.focus = Focus {
                    col: col - lead,
                    row: card.saturating_sub(self.lead_rows(false)),
                }
            }
            _ => {}
        }
        self.clamp_focus();
        self.say_focus();
    }

    fn leave_dashboard(&mut self) {
        // An open edit is kept rather than discarded: leaving is not a cancel,
        // and the text is one Ctrl+Z from being reverted anyway.
        if self.edit.is_some() {
            self.commit_edit_state();
        }
        self.mode = BoardMode::Board;
        self.in_dashboard = false;
        self.suppress_text = None;
        self.refresh = true;
        // Put the list cursor on the card the board was showing. Entering already
        // follows the list — `enter_dashboard` opens on the column the cursor was
        // in — and without this the round trip is one-way: Escape drops the user
        // back wherever they pressed `d`, which after a few minutes of arranging
        // cards is nowhere near what they were last looking at.
        //
        // Skipped for a placeholder: there is no card to land on, so the column
        // row itself is as close as the list can get.
        if !self.on_placeholder() && self.board.columns.get(self.focus.col).is_some() {
            self.navigation = Some(NavigationRequest::SelectPath(vec![
                (self.focus.col + self.lead_rows(true)) as u32,
                (self.focus.row + self.lead_rows(false)) as u32,
            ]));
        }
    }

    fn dashboard_render(&mut self, cols: u16, rows: u16) -> sicompass_sdk::plugin::Frame {
        self.render_frame(cols, rows).into()
    }

    fn dashboard_key(&mut self, key: Key) -> bool {
        self.ensure_loaded();
        match self.mode {
            BoardMode::Board => self.board_key(key),
            BoardMode::Insert => self.insert_key(key),
        }
    }

    fn dashboard_text(&mut self, text: &str) {
        // The duplicate-keystroke guard. SDL fires KEYDOWN before TEXTINPUT, so
        // the `a` or `i` that opened insert mode arrives here immediately after
        // as text. `lib_terminal` sidesteps this by never encoding an unmodified
        // `Char`; this provider cannot, because those letters are its commands.
        if let Some(pending) = self.suppress_text.take()
            && pending == text
        {
            return;
        }
        if self.mode != BoardMode::Insert {
            return;
        }
        self.insert_text(text);
    }

    fn dashboard_paste(&mut self, text: &str) {
        self.ensure_loaded();
        match self.mode {
            // Inside a card, a paste is text: newlines would make one card into
            // several mid-word, so they become spaces.
            BoardMode::Insert => {
                let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
                self.insert_text(&flat);
            }
            BoardMode::Board => self.paste_text(text),
        }
    }
}

impl ProjectManagementProvider {
    /// Keys in board mode.
    ///
    /// The bare keys act on what is focused; the Ctrl keys act on the list the
    /// focus is in. On a card those two coincide and Ctrl+A is an alias for `a`;
    /// on a column head they differ, which is what makes columns creatable from
    /// the board at all.
    fn board_key(&mut self, key: Key) -> bool {
        use Keysym as K;
        let ctrl = key.ctrl;
        match key.sym {
            K::Left => self.move_column(-1),
            K::Right => self.move_column(1),
            K::Up => self.move_row(false),
            K::Down => self.move_row(true),
            K::Escape => {
                self.dashboard_request = Some(DashboardRequest::Leave);
                true
            }
            K::Delete => {
                self.delete_focused();
                true
            }
            K::Ch('h') if !ctrl => self.move_column(-1),
            K::Ch('l') if !ctrl => self.move_column(1),
            K::Ch('k') if !ctrl => self.move_row(false),
            K::Ch('j') if !ctrl => self.move_row(true),
            // `i` and `a` edit the focused card, caret at the start or the end.
            // The same meaning they carry everywhere else in the app. On an empty
            // column's placeholder there is nothing to edit, so they start the
            // first card instead — the list behaves the same way, because an
            // empty level there *is* an `<input>` slot.
            K::Ch('i') if !ctrl => {
                self.suppress_text = Some("i".to_owned());
                self.begin_rename(false);
                true
            }
            K::Ch('a') if !ctrl => {
                self.suppress_text = Some("a".to_owned());
                self.begin_rename(true);
                true
            }
            // `o` and `O` open a new card below or above, the way they open a
            // new line in a vim-shaped editor. Ctrl+I and Ctrl+A are the app's
            // own insert-before and append-after, and on a board of cards those
            // mean the same two things — kept as aliases so the keys that work
            // in the list keep working here.
            K::Ch('o') if !ctrl && !key.shift => {
                self.suppress_text = Some("o".to_owned());
                self.begin_new_card(self.below());
                true
            }
            K::Ch('o') if !ctrl && key.shift => {
                self.suppress_text = Some("O".to_owned());
                self.begin_new_card(self.above());
                true
            }
            K::Ch('i') if ctrl => {
                self.begin_new_card(self.above());
                true
            }
            K::Ch('a') if ctrl => {
                self.begin_new_card(self.below());
                true
            }
            K::Ch('d') if ctrl => {
                self.delete_focused();
                true
            }
            K::Ch('x') if ctrl => {
                self.copy_focused(true);
                true
            }
            K::Ch('c') if ctrl => {
                self.copy_focused(false);
                true
            }
            K::Ch('v') if ctrl && !key.shift => {
                self.paste();
                true
            }
            _ => false,
        }
    }

    /// Keys in insert mode. Printable characters arrive through `dashboard_text`.
    fn insert_key(&mut self, key: Key) -> bool {
        use Keysym as K;
        match key.sym {
            // Escape keeps what was typed rather than discarding it, matching
            // the app's own Insert to General transition. Anyone who wanted the
            // old text back is one Ctrl+Z away, and that is undoable in turn.
            // Ctrl+Enter is a new line, as in every `<input>` in the app.
            K::Enter if key.ctrl => {
                self.insert_text("\n");
                true
            }
            K::Enter | K::Escape => {
                self.commit_edit_state();
                true
            }
            K::Backspace => {
                self.backspace();
                true
            }
            K::Delete => {
                self.delete_forward();
                true
            }
            K::Left => {
                self.edit_with(|f, _| f.left());
                true
            }
            K::Right => {
                self.edit_with(|f, _| f.right());
                true
            }
            // Home/End keep to the line the caret is on, split at newlines, the
            // same as the app's own Home/End.
            K::Home => {
                self.edit_with(|f, _| {
                    let lines = input::hard_lines(&f.text);
                    f.home(&lines);
                });
                true
            }
            K::End => {
                self.edit_with(|f, _| {
                    let lines = input::hard_lines(&f.text);
                    f.end(&lines);
                });
                true
            }
            K::Up => {
                self.edit_with(|f, lines| f.up(lines, false));
                true
            }
            K::Down => {
                self.edit_with(|f, lines| f.down(lines, false));
                true
            }
            _ => false,
        }
    }
}

impl ProjectManagementProvider {
    /// A background task ([`cloud::PluginHost`] runs them on threads) ended.
    /// `poll` hands each one over, in the order they finished.
    ///
    /// A sync that merged another computer's board in writes it to disk here,
    /// and the board is read again. Nothing was saved since the sync started
    /// (the cloud checks), so nothing typed here is lost.
    pub fn task_done(&mut self, id: u64, result: Result<Vec<u8>, String>) {
        let Some(root) = self.root() else {
            return;
        };
        if self.cloud.on_task_done(id, result, &*self.host, &root) == Finished::Reload {
            // The board in memory is stale: re-read what the sync wrote. The
            // counter stays above every id this session handed out, which the
            // undo timeline may still hold.
            let floor = self.board.next_id();
            self.loaded = false;
            self.load_failed = false;
            self.ensure_loaded();
            self.board.raise_counter(floor);
            self.clamp_focus();
            self.refresh = true;
        }
    }
}

/// Read a board op back out of a timeline entry, with its localized label.
///
/// Anything that is not one of ours is ignored rather than guessed at.
fn decode_op(entry: &ProviderOp) -> Option<(BoardOp, String)> {
    let payload = sicompass_sdk::plugin::decode_one(&entry.payload)?;
    let json = payload.as_str()?;
    let op: BoardOp = serde_json::from_str(json).ok()?;
    Some((op, entry.label.clone()))
}

// ---------------------------------------------------------------------------
// The renderer's types and the plugin interface's
//
// `render` draws in the SDK's dashboard types, which is what its tests read.
// The plugin interface has its own wire types, so the palette is converted on
// the way in, and the frame on the way out (the SDK's `From`).
// ---------------------------------------------------------------------------

fn to_sdk_palette(p: sicompass_sdk::plugin::Palette) -> sicompass_sdk::DashboardPalette {
    sicompass_sdk::DashboardPalette {
        background: p.background,
        text: p.text,
        header_sep: p.header_sep,
        selected: p.selected,
        ext_search: p.ext_search,
        scroll_search: p.scroll_search,
        error: p.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sicompass_sync::row::Standing;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use tempfile::TempDir;

    /// The host as the tests set it: a clock, a standing, a token, and a log
    /// of the tasks spawned. Cloned into the provider, so a test keeps a handle.
    #[derive(Clone)]
    struct FakeHost(Rc<FakeState>);

    struct FakeState {
        now: Cell<u64>,
        standing: Cell<Standing>,
        spawned: RefCell<Vec<(String, Vec<u8>)>>,
    }

    impl FakeHost {
        fn new(standing: Standing) -> Self {
            FakeHost(Rc::new(FakeState {
                now: Cell::new(0),
                standing: Cell::new(standing),
                spawned: RefCell::new(Vec::new()),
            }))
        }

        fn advance(&self, ms: u64) {
            self.0.now.set(self.0.now.get() + ms);
        }

        fn spawned(&self) -> Vec<(String, Vec<u8>)> {
            self.0.spawned.borrow().clone()
        }
    }

    impl cloud::Host for FakeHost {
        fn now_millis(&self) -> u64 {
            self.0.now.get()
        }

        fn standing(&self) -> Standing {
            self.0.standing.get()
        }

        /// Task ids are 1, 2, 3, in spawn order.
        fn spawn(&self, task: &str, input: &[u8]) -> Result<u64, String> {
            let mut log = self.0.spawned.borrow_mut();
            log.push((task.to_owned(), input.to_vec()));
            Ok(log.len() as u64)
        }

        fn translate(&self, id: &str, args: &[(&str, String)]) -> String {
            cloud::translate(id, args)
        }
    }

    impl CloudHost for FakeHost {
        fn token(&self) -> Option<String> {
            Some("tok-42".to_owned())
        }
    }

    /// A provider backed by a real, disposable directory.
    ///
    /// Natively `root()` is nothing without one, so a forgotten one cannot
    /// reach a real board, but a test that asserts anything about the store
    /// needs somewhere real to look.
    fn provider(dir: &TempDir) -> ProjectManagementProvider {
        provider_on(dir, &FakeHost::new(Standing::Missing))
    }

    fn provider_on(dir: &TempDir, host: &FakeHost) -> ProjectManagementProvider {
        let mut p = ProjectManagementProvider::with_host(Box::new(host.clone()));
        p.set_root(dir.path().join("board"));
        p
    }

    /// A seeded board with cloud backup switched on and a known standing.
    fn seeded_with_cloud(standing: Standing) -> ProjectManagementProvider {
        seeded_on(&FakeHost::new(standing), true)
    }

    fn active_licence() -> Standing {
        Standing::Active {
            renews_in_days: 342,
        }
    }

    /// The seeded board on `host`, with the backup switch as given.
    fn seeded_on(host: &FakeHost, cloud_on: bool) -> ProjectManagementProvider {
        let mut p = seeded();
        p.host = Box::new(host.clone());
        if cloud_on {
            p.on_setting_change(cloud::ENABLE_KEY, "true");
        }
        p
    }

    /// Two columns and three cards, and no disk at all.
    fn seeded() -> ProjectManagementProvider {
        let mut p = ProjectManagementProvider::new();
        p.loaded = true;
        let mut todo = Column::new(1, "To do");
        todo.cards.push(Card::new(2, "fix login"));
        todo.cards.push(Card::new(3, "write docs"));
        let mut doing = Column::new(4, "Doing");
        doing.cards.push(Card::new(5, "kanban ui"));
        p.board.columns.push(todo);
        p.board.columns.push(doing);
        p.board.reseat_counter();
        p
    }

    /// The path the plugin asked the list cursor to land on, if any. The
    /// interface's `NavigationRequest` has no `PartialEq`, so tests compare this.
    fn nav(p: &mut ProjectManagementProvider) -> Option<Vec<u32>> {
        p.take_navigation_request().map(|r| match r {
            NavigationRequest::SelectPath(at) => at,
            NavigationRequest::EnterChildren => panic!("the board never asks to enter children"),
        })
    }

    /// What a screen reader would read: every row's display text, tags gone.
    fn labels(elems: &[FfonElement]) -> Vec<String> {
        elems.iter().map(|e| row_text(raw_of(e))).collect()
    }

    fn cards(p: &ProjectManagementProvider, col: usize) -> Vec<String> {
        p.board.columns[col]
            .cards
            .iter()
            .map(|c| c.text.clone())
            .collect()
    }

    fn key(k: Keysym) -> Key {
        Key {
            sym: k,
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    fn shift(k: Keysym) -> Key {
        Key {
            sym: k,
            ctrl: false,
            shift: true,
            alt: false,
        }
    }

    fn ctrl(k: Keysym) -> Key {
        Key {
            sym: k,
            ctrl: true,
            shift: false,
            alt: false,
        }
    }

    /// Descend into a column the way the app does: render the level, then push
    /// the label the user was looking at. `push_path` resolves a *displayed
    /// label*, so a level that was never rendered has no labels to resolve
    /// against.
    fn descend(p: &mut ProjectManagementProvider, title: &str) {
        let _ = p.fetch();
        p.push_path(title);
    }

    /// Open the board and type a card at the cursor.
    fn add_card(p: &mut ProjectManagementProvider, text: &str) {
        p.dashboard_key(key(Keysym::Ch('o')));
        p.dashboard_text(text);
        p.dashboard_key(key(Keysym::Enter));
    }

    // ---- The list surface ----------------------------------------------

    #[test]
    fn the_root_lists_the_columns_and_a_column_lists_its_cards() {
        let mut p = seeded();
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "To do", "Doing"]);
        p.push_path("To do");
        assert_eq!(
            labels(&p.fetch()),
            vec![meta().as_str(), "fix login", "write docs"]
        );
        p.pop_path();
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "To do", "Doing"]);
    }

    #[test]
    fn a_column_is_a_branch_and_a_card_is_a_leaf() {
        // Obj versus Str *is* the depth cap: the app descends into an Obj and
        // cannot descend into a Str, so a card has nowhere to grow children.
        let mut p = seeded();
        assert!(p.fetch().iter().all(|e| e.is_obj()), "columns must be Obj");
        p.push_path("To do");
        let rows = p.fetch();
        assert!(matches!(&rows[0], FfonElement::Obj(o) if o.key == meta()));
        assert!(rows[1..].iter().all(|e| e.is_str()), "cards must be Str");
    }

    #[test]
    fn a_card_cannot_be_descended_into() {
        let mut p = seeded();
        descend(&mut p, "To do");
        let before = p.current_path().to_owned();
        p.push_path("fix login");
        assert_eq!(p.current_path(), before, "a card is a leaf");
    }

    #[test]
    fn an_empty_level_renders_a_placeholder_rather_than_nothing() {
        let mut p = ProjectManagementProvider::new();
        p.loaded = true;
        assert_eq!(p.fetch().len(), 2);
        assert!(labels(&p.fetch())[1].contains("no columns"));
    }

    #[test]
    fn the_placeholder_never_becomes_a_real_column() {
        let mut p = ProjectManagementProvider::new();
        p.loaded = true;
        let placeholder = p.fetch();
        p.sync_ffon_body_children(&placeholder);
        assert!(p.board.columns.is_empty());
    }

    #[test]
    fn a_row_with_no_id_becomes_a_new_column() {
        let mut p = seeded();
        let mut rows = p.fetch();
        rows.push(FfonElement::new_obj(tags::format_input("Done")));
        p.sync_ffon_body_children(&rows);
        assert_eq!(
            p.board
                .columns
                .iter()
                .map(|c| c.title.as_str())
                .collect::<Vec<_>>(),
            vec!["To do", "Doing", "Done"]
        );
    }

    #[test]
    fn a_removed_row_deletes_its_card() {
        let mut p = seeded();
        descend(&mut p, "To do");
        let mut rows = p.fetch();
        rows.remove(1); // row 0 is the list meta
        p.sync_ffon_body_children(&rows);
        assert_eq!(cards(&p, 0), vec!["write docs"]);
    }

    #[test]
    fn a_trailing_colon_is_object_syntax_and_is_not_part_of_the_name() {
        // Typing a trailing colon in the list is how a row becomes an `Obj`, and
        // the app strips it before storing the key. Keeping it would store the
        // punctuation and then show it on the board, where nothing explains it.
        let mut p = seeded();
        let mut rows = p.fetch();
        rows.push(FfonElement::new_obj(tags::format_input("Done:")));
        p.sync_ffon_body_children(&rows);
        assert_eq!(p.board.columns[2].title, "Done");
    }

    #[test]
    fn a_colon_already_on_disk_is_cleaned_up_on_load() {
        // Stripping only on write leaves every title written before that fix
        // still carrying one, and the board goes on showing it until the user
        // happens to rename the column.
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("board");
        let mut stale = crate::board::Board::new();
        stale.columns.push(Column::new(1, "To do:"));
        crate::store::save_board(&root, &stale).unwrap();

        let mut p = provider(&dir);
        p.ensure_loaded();
        assert_eq!(p.board.columns[0].title, "To do");
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "To do"]);
    }

    #[test]
    fn a_colon_inside_a_title_is_left_alone() {
        let mut p = seeded();
        let mut rows = p.fetch();
        rows.push(FfonElement::new_obj(tags::format_input("Q4: roadmap")));
        p.sync_ffon_body_children(&rows);
        assert_eq!(p.board.columns[2].title, "Q4: roadmap");
    }

    #[test]
    fn a_card_keeps_its_colon() {
        // Only a column is an object, so only a column title carries the syntax.
        let mut p = seeded();
        descend(&mut p, "To do");
        let mut rows = p.fetch();
        rows.push(FfonElement::Str(tags::format_input("note: check this")));
        p.sync_ffon_body_children(&rows);
        assert_eq!(cards(&p, 0)[2], "note: check this");
    }

    #[test]
    fn a_rename_keeps_the_id_so_two_identical_titles_stay_apart() {
        // The reason rows carry an `<id>` at all: `commit_edit(old, new)` cannot
        // tell two identically titled siblings apart, because `old` is only the
        // previous display text.
        let mut p = seeded();
        let rows = vec![
            FfonElement::new_obj(ProjectManagementProvider::row_label(1, "Backlog")),
            FfonElement::new_obj(ProjectManagementProvider::row_label(4, "Backlog")),
        ];
        p.sync_ffon_body_children(&rows);
        assert_eq!(p.board.columns[0].id, 1);
        assert_eq!(p.board.columns[1].id, 4);
        assert_eq!(p.board.columns[1].cards[0].text, "kanban ui");
    }

    #[test]
    fn a_list_is_identified_from_its_row_ids_not_from_the_cursor() {
        // Undo hands back a list from wherever the reversed edit happened, and
        // does not move the provider's path to match.
        let mut p = seeded();
        assert!(p.at_root(), "cursor is at the root");
        let rows = vec![FfonElement::Str(ProjectManagementProvider::row_label(
            5,
            "kanban ui, renamed",
        ))];
        p.sync_ffon_body_children(&rows);
        assert_eq!(p.board.columns[1].cards[0].text, "kanban ui, renamed");
        assert_eq!(cards(&p, 0).len(), 2, "the other column is untouched");
    }

    #[test]
    fn a_card_moved_between_columns_does_not_exist_twice() {
        let mut p = seeded();
        descend(&mut p, "Doing");
        let rows = vec![
            FfonElement::Str(ProjectManagementProvider::row_label(5, "kanban ui")),
            FfonElement::Str(ProjectManagementProvider::row_label(2, "fix login")),
        ];
        p.sync_ffon_body_children(&rows);
        assert_eq!(cards(&p, 1).len(), 2);
        assert_eq!(cards(&p, 0).len(), 1);
        assert_eq!(p.board.locate_card(2), Some((1, 1)));
    }

    #[test]
    fn card_text_that_looks_like_a_tag_stays_text() {
        let mut p = seeded();
        p.board.columns[0].cards[0].text = "<button>submit</button>Send".to_owned();
        descend(&mut p, "To do");
        let rows = p.fetch();
        assert!(
            !tags::has_button(raw_of(&rows[1])),
            "a card must not forge a button"
        );
        assert_eq!(labels(&rows)[1], "<button>submit</button>Send");
    }

    #[test]
    fn a_delete_is_refused_while_the_board_could_not_be_read() {
        let mut p = seeded();
        p.load_failed = true;
        assert!(!p.delete_item("anything"));
        assert!(p.take_error().is_some());
    }

    #[test]
    fn nothing_is_written_while_the_board_could_not_be_read() {
        let dir = TempDir::new().unwrap();
        let mut p = provider(&dir);
        p.load_failed = true;
        p.loaded = true;
        p.board.columns.push(Column::new(1, "ghost"));
        p.persist();
        assert!(
            !dir.path().join("board").exists(),
            "a failed load must never lead to a write"
        );
    }

    // ---- Persistence ----------------------------------------------------

    #[test]
    fn a_board_written_by_one_provider_is_read_by_the_next() {
        let dir = TempDir::new().unwrap();
        {
            let mut p = provider(&dir);
            p.ensure_loaded();
            let mut rows = p.fetch();
            rows.push(FfonElement::new_obj(tags::format_input("To do")));
            p.sync_ffon_body_children(&rows);
        }
        let mut p = provider(&dir);
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "To do"]);
    }

    #[test]
    fn the_path_survives_a_restart() {
        let mut p = seeded();
        descend(&mut p, "Doing");
        let saved = p.current_path().to_owned();
        let mut fresh = seeded();
        fresh.set_current_path(&saved);
        assert_eq!(labels(&fresh.fetch()), vec![meta().as_str(), "kanban ui"]);
    }

    /// The app also builds a path out of the display text of the row the cursor
    /// sits under — `sync_inmemory_provider_path_to_cursor` does it after every
    /// search jump. Reading only the `c<id>` token closed the column, so a Tab
    /// search inside one left this provider on the board root while the cursor
    /// was still in the column, and the next edit re-fetched the root listing
    /// over it.
    #[test]
    fn a_path_of_display_labels_opens_the_same_column() {
        let mut p = seeded();
        descend(&mut p, "Doing");
        let walked = p.current_path().to_owned();

        let mut q = seeded();
        let _ = q.fetch();
        q.set_current_path("/Doing");
        assert_eq!(q.current_path(), walked);
        assert_eq!(labels(&q.fetch()), vec![meta().as_str(), "kanban ui"]);
    }

    /// A column may be titled `c1`, and on a path this provider rendered itself
    /// that is not what `c1` means.
    #[test]
    fn a_column_token_beats_a_column_titled_like_one() {
        let mut p = seeded();
        p.board.columns[1].title = "c1".to_owned();
        let _ = p.fetch();

        p.set_current_path("/c4");
        assert_eq!(
            labels(&p.fetch()),
            vec![meta().as_str(), "kanban ui"],
            "column 4 is `Doing`"
        );
    }

    #[test]
    fn the_root_path_closes_the_column() {
        let mut p = seeded();
        descend(&mut p, "Doing");
        p.set_current_path("/");
        assert!(p.at_root());
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "To do", "Doing"]);
    }

    /// Staying put beats falling back to the board root, which is the one place
    /// the cursor demonstrably is not.
    #[test]
    fn an_unresolvable_segment_leaves_the_column_open() {
        let mut p = seeded();
        descend(&mut p, "Doing");
        let walked = p.current_path().to_owned();

        p.set_current_path("/never rendered");
        assert_eq!(p.current_path(), walked);
    }

    #[test]
    fn the_subtree_parent_key_names_the_open_column() {
        let mut p = seeded();
        assert_eq!(
            p.fetch_subtree_parent_key(),
            None,
            "no column is open on the board root"
        );

        descend(&mut p, "Doing");
        assert_eq!(
            p.fetch_subtree_parent_key(),
            Some(ProjectManagementProvider::row_label(4, "Doing")),
            "must read exactly as the board root renders it"
        );
    }

    // ---- Only cards are focusable ---------------------------------------

    #[test]
    fn the_cursor_starts_on_a_card_not_on_a_head() {
        let mut p = seeded();
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 0, row: 0 });
        assert!(!p.on_placeholder());
    }

    #[test]
    fn up_from_the_first_card_stays_on_it() {
        // There is no head to land on any more, so the top card is the top.
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Up));
        assert_eq!(p.focus, Focus { col: 0, row: 0 });
    }

    #[test]
    fn arrows_move_between_columns_and_within_one() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Down));
        assert_eq!(p.focus, Focus { col: 0, row: 1 });
        p.dashboard_key(key(Keysym::Down));
        assert_eq!(
            p.focus,
            Focus { col: 0, row: 1 },
            "clamped at the last card"
        );
        p.dashboard_key(key(Keysym::Right));
        assert_eq!(p.focus, Focus { col: 1, row: 0 }, "Doing holds one card");
    }

    #[test]
    fn hjkl_move_the_same_way_the_arrows_do() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('j')));
        assert_eq!(p.focus.row, 1);
        p.dashboard_key(key(Keysym::Ch('l')));
        assert_eq!(p.focus.col, 1);
        p.dashboard_key(key(Keysym::Ch('h')));
        assert_eq!(p.focus.col, 0);
        p.dashboard_key(key(Keysym::Ch('k')));
        assert_eq!(p.focus.row, 0);
    }

    #[test]
    fn an_empty_column_still_holds_the_cursor_on_its_placeholder() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Right));
        p.dashboard_key(key(Keysym::Right));
        assert_eq!(p.focus, Focus { col: 2, row: 0 });
        assert!(p.on_placeholder());
    }

    #[test]
    fn the_board_opens_on_the_card_the_list_cursor_was_on() {
        let mut p = seeded();
        descend(&mut p, "To do");
        // The cursor is on the second card of the first column (each list
        // opens with its meta row, so these are `fetch()` rows 1 and 2).
        p.set_dashboard_entry(&[1, 2]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 0, row: 1 });
    }

    #[test]
    fn from_a_column_title_the_board_opens_on_that_columns_first_card() {
        let mut p = seeded();
        // The cursor is on the second column's title, at the provider root,
        // below the list meta.
        p.set_dashboard_entry(&[2]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 1, row: 0 });
    }

    #[test]
    fn entering_and_leaving_are_mirror_images() {
        // Whatever the board was showing is where the list lands, and whatever
        // the list was on is where the board opens.
        let mut p = seeded();
        p.set_dashboard_entry(&[1, 2]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 0, row: 1 });
        p.leave_dashboard();
        assert_eq!(nav(&mut p), Some(vec![1, 2]));
    }

    #[test]
    fn an_entry_path_past_the_end_of_the_board_is_clamped() {
        // The list and the board can disagree after an edit in another tab.
        let mut p = seeded();
        p.set_dashboard_entry(&[9, 9]);
        p.enter_dashboard();
        assert_eq!(
            p.focus,
            Focus { col: 1, row: 0 },
            "clamped to something real"
        );
    }

    #[test]
    fn entering_from_the_provider_root_with_no_path_keeps_the_cursor() {
        let mut p = seeded();
        p.focus = Focus { col: 1, row: 0 };
        p.set_dashboard_entry(&[]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 1, row: 0 });
    }

    #[test]
    fn every_focus_move_says_where_it_landed() {
        let mut p = seeded();
        p.enter_dashboard();
        let _ = p.take_announcement();
        p.dashboard_key(key(Keysym::Down));
        let said = p.take_announcement().expect("a move must be announced");
        assert!(said.contains("write docs"), "got {said:?}");
        assert!(said.contains("To do"), "got {said:?}");
    }

    #[test]
    fn an_empty_columns_placeholder_announces_the_column_as_empty() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.focus = Focus { col: 2, row: 0 };
        p.say_focus();
        let said = p.take_announcement().unwrap();
        assert!(said.contains("Done"), "got {said:?}");
        // Compared against the resolved key rather than the English word: the
        // Fluent localizer is process-global, so a test elsewhere switching the
        // locale would otherwise make this one fail for the wrong reason.
        let mut a = localize::Args::new();
        a.set("index", "3");
        a.set("total", "3");
        a.set("title", "Done");
        assert_eq!(
            said,
            localize::t_args("projectmanagement-say-column-empty", &a),
            "got {said:?}"
        );
    }

    // ---- Insert mode ----------------------------------------------------

    #[test]
    fn i_puts_the_caret_at_the_start_and_a_puts_it_at_the_end() {
        for (k, want) in [('i', 0usize), ('a', 9usize)] {
            let mut p = seeded();
            p.enter_dashboard();
            p.dashboard_key(key(Keysym::Ch(k)));
            let edit = p.edit.as_ref().expect("insert mode should be open");
            assert_eq!(edit.text, "fix login");
            assert_eq!(edit.caret, want, "`{k}` caret");
            assert!(!edit.creating, "`{k}` must not create a card");
        }
    }

    #[test]
    fn ctrl_enter_adds_a_line_that_up_and_down_cross_and_enter_commits() {
        let mut p = seeded();
        p.enter_dashboard();
        let _ = p.render_frame(80, 30);
        p.dashboard_key(key(Keysym::Ch('a')));
        p.dashboard_key(ctrl(Keysym::Enter));
        p.dashboard_text("ship");
        let len = "fix login\nship".len();
        {
            let edit = p.edit.as_ref().expect("Ctrl+Enter must not commit");
            assert_eq!(edit.text, "fix login\nship");
            assert_eq!(edit.caret, len);
        }
        // "ship" starts flush, like a new line in any other field in the app,
        // so its end is column 4 and Up lands on column 4 of the line above.
        p.dashboard_key(key(Keysym::Up));
        assert_eq!(p.edit.as_ref().unwrap().caret, "fix ".len());
        p.dashboard_key(key(Keysym::Down));
        assert_eq!(p.edit.as_ref().unwrap().caret, len);
        p.dashboard_key(key(Keysym::Down));
        assert_eq!(p.edit.as_ref().unwrap().caret, len, "Down on the last line");
        p.dashboard_key(key(Keysym::Up));
        p.dashboard_key(key(Keysym::Up));
        assert_eq!(p.edit.as_ref().unwrap().caret, 0, "Up on the first line");
        p.dashboard_key(key(Keysym::Enter));
        assert!(p.edit.is_none(), "Enter commits");
        assert_eq!(p.board.columns[0].cards[0].text, "fix login\nship");
    }

    #[test]
    fn up_and_down_walk_the_wrapped_lines_of_a_long_card() {
        let mut p = seeded();
        p.enter_dashboard();
        let _ = p.render_frame(80, 30);
        p.dashboard_key(key(Keysym::Ch('a')));
        let lay = render::layout(p.board.columns.len(), p.focus.col, 80);
        let width = lay.width_of(p.focus.col);
        p.dashboard_text(&" word".repeat(width as usize));
        let text = p.edit.as_ref().unwrap().text.clone();
        let lines = render::card_lines(&text, width);
        assert!(lines.len() >= 3, "the card must wrap: {lines:?}");
        p.dashboard_key(key(Keysym::Up));
        let caret = p.edit.as_ref().unwrap().caret;
        assert_eq!(input::line_index(&lines, caret), lines.len() - 2);
        p.dashboard_key(key(Keysym::Down));
        let caret = p.edit.as_ref().unwrap().caret;
        assert_eq!(input::line_index(&lines, caret), lines.len() - 1);
        p.dashboard_key(key(Keysym::Down));
        assert_eq!(p.edit.as_ref().unwrap().caret, text.len());
    }

    #[test]
    fn neither_i_nor_a_adds_a_card_to_a_column_that_has_some() {
        for k in ['i', 'a'] {
            let mut p = seeded();
            p.enter_dashboard();
            let before = p.board.card_count();
            p.dashboard_key(key(Keysym::Ch(k)));
            p.dashboard_key(key(Keysym::Escape));
            assert_eq!(p.board.card_count(), before, "`{k}` must not create");
        }
    }

    #[test]
    fn o_opens_insert_mode_and_does_not_type_its_own_letter() {
        // SDL fires KEYDOWN before TEXTINPUT, so the letter that opened insert
        // mode arrives again as text. Without the guard the card reads "ox".
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('o')));
        p.dashboard_text("o");
        p.dashboard_text("x");
        assert_eq!(p.edit.as_ref().unwrap().text, "x");
    }

    #[test]
    fn the_guard_never_eats_a_real_letter_later_on() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('o')));
        // The duplicate never arrives (a different keyboard path, say), and the
        // user types the same letter on purpose two keystrokes later.
        p.dashboard_text("b");
        p.dashboard_text("o");
        assert_eq!(p.edit.as_ref().unwrap().text, "bo");
    }

    #[test]
    fn o_opens_a_card_below_and_shift_o_above() {
        let mut p = seeded();
        p.enter_dashboard();
        add_card(&mut p, "below");
        p.focus = Focus { col: 0, row: 0 };
        p.dashboard_key(shift(Keysym::Ch('o')));
        p.dashboard_text("above");
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(
            cards(&p, 0),
            vec!["above", "fix login", "below", "write docs"]
        );
    }

    #[test]
    fn ctrl_i_and_ctrl_a_insert_before_and_after_like_they_do_in_the_list() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('a')));
        p.dashboard_text("after");
        p.dashboard_key(key(Keysym::Enter));
        p.focus = Focus { col: 0, row: 0 };
        p.dashboard_key(ctrl(Keysym::Ch('i')));
        p.dashboard_text("before");
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(
            cards(&p, 0),
            vec!["before", "fix login", "after", "write docs"]
        );
    }

    // ---- The empty-column placeholder -----------------------------------

    #[test]
    fn ctrl_a_on_the_placeholder_puts_the_first_card_in_the_column() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.focus = Focus { col: 2, row: 0 };
        p.dashboard_key(ctrl(Keysym::Ch('a')));
        p.dashboard_text("shipped");
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(cards(&p, 2), vec!["shipped"]);
    }

    #[test]
    fn every_insert_key_starts_the_first_card_on_a_placeholder() {
        // `i` and `a` have nothing to edit there, so they create rather than
        // being dead keys — the list behaves the same way, because an empty level
        // there *is* an `<input>` slot.
        for k in [
            key(Keysym::Ch('i')),
            key(Keysym::Ch('a')),
            key(Keysym::Ch('o')),
            shift(Keysym::Ch('o')),
            ctrl(Keysym::Ch('i')),
            ctrl(Keysym::Ch('a')),
        ] {
            let mut p = seeded();
            p.board.columns.push(Column::new(9, "Done"));
            p.enter_dashboard();
            p.focus = Focus { col: 2, row: 0 };
            p.dashboard_key(k);
            p.dashboard_text("first");
            p.dashboard_key(key(Keysym::Enter));
            assert_eq!(cards(&p, 2), vec!["first"], "for {k:?}");
        }
    }

    #[test]
    fn delete_on_a_placeholder_does_nothing() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.focus = Focus { col: 2, row: 0 };
        p.dashboard_key(ctrl(Keysym::Ch('d')));
        assert_eq!(p.board.columns.len(), 3, "the column must survive");
        assert!(p.take_timeline_entries().is_empty());
    }

    #[test]
    fn pasting_onto_a_placeholder_puts_the_card_in_the_empty_column() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('c')));
        p.focus = Focus { col: 2, row: 0 };
        p.dashboard_key(ctrl(Keysym::Ch('v')));
        assert_eq!(cards(&p, 2), vec!["fix login"]);
    }

    // ---- Columns are the list's job -------------------------------------

    #[test]
    fn the_board_never_touches_a_column() {
        // Only cards are focusable, so no board gesture can reach a column. This
        // is the guard on that: whatever the keys do, the column list is the same
        // list afterwards.
        let mut p = seeded();
        p.enter_dashboard();
        let before: Vec<(Id, String)> = p
            .board
            .columns
            .iter()
            .map(|c| (c.id, c.title.clone()))
            .collect();
        for k in [
            key(Keysym::Ch('i')),
            key(Keysym::Ch('a')),
            key(Keysym::Ch('o')),
            shift(Keysym::Ch('o')),
            ctrl(Keysym::Ch('i')),
            ctrl(Keysym::Ch('a')),
            ctrl(Keysym::Ch('d')),
            ctrl(Keysym::Ch('x')),
            ctrl(Keysym::Ch('c')),
            ctrl(Keysym::Ch('v')),
            key(Keysym::Delete),
        ] {
            p.dashboard_key(k);
            p.dashboard_text("x");
            p.dashboard_key(key(Keysym::Enter));
        }
        let after: Vec<(Id, String)> = p
            .board
            .columns
            .iter()
            .map(|c| (c.id, c.title.clone()))
            .collect();
        assert_eq!(
            before, after,
            "the board must not create, rename or delete a column"
        );
    }

    // ---- Editing --------------------------------------------------------

    #[test]
    fn a_creation_typed_into_and_left_empty_is_cancelled_not_kept() {
        let mut p = seeded();
        p.enter_dashboard();
        let before = p.board.card_count();
        p.dashboard_key(key(Keysym::Ch('o')));
        p.dashboard_key(key(Keysym::Escape));
        assert_eq!(p.board.card_count(), before);
        assert!(
            p.take_timeline_entries().is_empty(),
            "an edit that left no trace must leave no undo step"
        );
    }

    #[test]
    fn escape_in_insert_returns_to_the_board_and_does_not_leave_it() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('i')));
        assert_eq!(p.mode, BoardMode::Insert);
        p.dashboard_key(key(Keysym::Escape));
        assert_eq!(p.mode, BoardMode::Board);
        assert_eq!(
            p.take_dashboard_request(),
            None,
            "escape must not also leave"
        );
    }

    #[test]
    fn escape_on_the_board_asks_the_app_to_leave() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Escape));
        assert_eq!(p.take_dashboard_request(), Some(DashboardRequest::Leave));
        assert_eq!(p.take_dashboard_request(), None, "two-call semantics");
    }

    #[test]
    fn the_caret_moves_and_edits_land_where_it_is() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_key(key(Keysym::Ch('i')));
        p.dashboard_text("new ");
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(cards(&p, 1), vec!["new kanban ui"]);
    }

    #[test]
    fn editing_multibyte_text_does_not_split_a_character() {
        let mut p = seeded();
        p.board.columns[1].cards[0].text = "héllo wörld".to_owned();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_key(key(Keysym::Ch('a')));
        p.dashboard_key(key(Keysym::Backspace));
        p.dashboard_key(key(Keysym::Home));
        p.dashboard_key(key(Keysym::Delete));
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(cards(&p, 1), vec!["éllo wörl"]);
    }

    #[test]
    fn ctrl_d_and_delete_both_remove_the_focused_card() {
        for k in [ctrl(Keysym::Ch('d')), key(Keysym::Delete)] {
            let mut p = seeded();
            p.enter_dashboard();
            p.dashboard_key(k);
            assert_eq!(cards(&p, 0), vec!["write docs"]);
        }
    }

    #[test]
    fn cut_then_paste_moves_a_card_to_another_column() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('x')));
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_key(ctrl(Keysym::Ch('v')));
        assert_eq!(cards(&p, 0), vec!["write docs"]);
        assert_eq!(cards(&p, 1), vec!["kanban ui", "fix login"]);
    }

    #[test]
    fn copy_then_paste_duplicates_with_a_different_id() {
        // Ids are minted once and never reused. A pasted copy sharing the
        // original's id would make `locate_card` find whichever came first, and
        // every later edit would land on the wrong card.
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('c')));
        p.dashboard_key(ctrl(Keysym::Ch('v')));
        let c = &p.board.columns[0].cards;
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].text, c[1].text);
        assert_ne!(c[0].id, c[1].id);
    }

    #[test]
    fn pasting_with_an_empty_clipboard_says_so_instead_of_doing_nothing() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('v')));
        assert!(p.take_error().is_some());
        assert_eq!(p.board.card_count(), 3);
    }

    #[test]
    fn clipboard_text_becomes_one_card_per_line() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_paste("alpha\n\n  beta  \ngamma\n");
        assert_eq!(cards(&p, 1), vec!["kanban ui", "alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_paste_inside_a_card_stays_one_card() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_key(key(Keysym::Ch('i')));
        p.dashboard_paste("one\ntwo ");
        p.dashboard_key(key(Keysym::Enter));
        assert_eq!(cards(&p, 1), vec!["one twokanban ui"]);
    }

    // ---- Undo -----------------------------------------------------------

    /// Every board edit is one entry, and feeding it back reverses it exactly.
    ///
    /// Columns only, deliberately: the id counter is *not* part of what an undo
    /// restores. It only ever moves forward, so an id handed out once is never
    /// handed out again even after the card holding it is undone away. See
    /// `Board::reseat_counter`.
    fn round_trips(mut p: ProjectManagementProvider, act: impl Fn(&mut ProjectManagementProvider)) {
        p.enter_dashboard();
        let before = p.board.columns.clone();
        act(&mut p);
        let after = p.board.columns.clone();
        assert_ne!(before, after, "the action must actually change the board");

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1, "one edit, one undo step");

        p.undo(&entries[0]).unwrap();
        assert_eq!(
            p.board.columns, before,
            "undo must restore the board exactly"
        );

        p.redo(&entries[0]).unwrap();
        assert_eq!(
            p.board.columns, after,
            "redo must be the undo's exact inverse"
        );
    }

    #[test]
    fn adding_a_card_round_trips() {
        round_trips(seeded(), |p| add_card(p, "ship it"));
    }

    #[test]
    fn adding_the_first_card_to_an_empty_column_round_trips() {
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.board.reseat_counter();
        round_trips(p, |p| {
            p.focus = Focus { col: 2, row: 0 };
            add_card(p, "shipped");
        });
    }

    #[test]
    fn deleting_a_card_round_trips() {
        round_trips(seeded(), |p| {
            p.focus = Focus { col: 0, row: 1 };
            p.dashboard_key(ctrl(Keysym::Ch('d')));
        });
    }

    #[test]
    fn renaming_a_card_round_trips() {
        round_trips(seeded(), |p| {
            p.dashboard_key(key(Keysym::Ch('a')));
            p.dashboard_text(" now");
            p.dashboard_key(key(Keysym::Enter));
        });
    }

    #[test]
    fn pasting_a_card_round_trips() {
        let mut p = seeded();
        p.clip = Some(Clip::Card(CardData {
            id: 99,
            text: "from the clipboard".to_owned(),
        }));
        round_trips(p, |p| {
            p.focus = Focus { col: 1, row: 0 };
            p.dashboard_key(ctrl(Keysym::Ch('v')));
        });
    }

    #[test]
    fn a_rename_to_the_same_text_records_nothing() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('a')));
        p.dashboard_key(key(Keysym::Enter));
        assert!(p.take_timeline_entries().is_empty());
    }

    #[test]
    fn moving_a_card_sideways_round_trips() {
        let mut p = seeded();
        descend(&mut p, "To do");
        let rows = p.fetch();
        let key_label = raw_of(&rows[1]).to_owned();
        let before = p.board.clone();
        assert!(p.move_card_sideways(true, &key_label));
        let after = p.board.clone();
        assert_eq!(cards(&p, 1).len(), 2);

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        p.undo(&entries[0]).unwrap();
        assert_eq!(p.board.columns, before.columns);
        p.redo(&entries[0]).unwrap();
        assert_eq!(p.board.columns, after.columns);
    }

    #[test]
    fn an_undone_card_does_not_hand_its_id_to_the_next_one() {
        // Undo shrinks the board; a redo still holds the card it removed. If the
        // counter followed the board down, the next new card would take that id
        // and the redo would insert a duplicate identity.
        let mut p = seeded();
        p.enter_dashboard();
        add_card(&mut p, "first");
        let first_id = p.board.columns[0].cards[1].id;
        let entries = p.take_timeline_entries();
        p.undo(&entries[0]).unwrap();

        add_card(&mut p, "second");
        let second_id = p.board.columns[0].cards[1].id;
        assert_ne!(first_id, second_id, "an id must never be handed out twice");
    }

    #[test]
    fn an_entry_from_another_provider_is_ignored_rather_than_guessed_at() {
        // The tab's timeline also carries the app's own `Structural` entries for
        // the list surface, and undo hands this provider whatever it recorded.
        let mut p = seeded();
        let before = p.board.clone();
        let foreign = ProviderOp {
            command: "something-else".to_owned(),
            payload: sicompass_sdk::plugin::encode_one(&FfonElement::Str(
                "not our json".to_owned(),
            )),
            label: "x".to_owned(),
        };
        p.undo(&foreign).unwrap();
        assert_eq!(p.board.columns, before.columns);
    }

    #[test]
    fn an_undo_says_what_it_undid() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(ctrl(Keysym::Ch('d')));
        let entries = p.take_timeline_entries();
        let _ = p.take_announcement();
        p.undo(&entries[0]).unwrap();
        let said = p.take_announcement().expect("undo must be announced");
        assert!(
            said.contains(&localize::t("projectmanagement-op-delete-card")),
            "got {said:?}"
        );
    }

    #[test]
    fn every_board_op_resolves_to_a_real_label() {
        // `record` builds the key as `projectmanagement-op-{command}`, so a command whose label
        // is missing renders the key itself into the undo history.
        for command in ["add-card", "delete-card", "rename-card", "move-card"] {
            let key = format!("projectmanagement-op-{command}");
            assert_ne!(localize::t(&key), key, "{key} has no label");
        }
    }

    #[test]
    fn a_board_edit_asks_the_list_to_refresh() {
        // Without this the list view still shows the pre-edit board when the
        // user leaves the dashboard.
        let mut p = seeded();
        p.enter_dashboard();
        p.clear_needs_refresh();
        p.dashboard_key(ctrl(Keysym::Ch('d')));
        assert!(p.needs_refresh());
    }

    #[test]
    fn a_board_edit_reaches_the_disk() {
        let dir = TempDir::new().unwrap();
        let mut p = provider(&dir);
        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.enter_dashboard();
        add_card(&mut p, "ship it");

        let mut fresh = provider(&dir);
        descend(&mut fresh, "To do");
        assert_eq!(labels(&fresh.fetch()), vec![meta().as_str(), "ship it"]);
    }

    #[test]
    fn leaving_the_board_asks_for_the_cursor_to_follow_the_focused_card() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.leave_dashboard();
        assert_eq!(nav(&mut p), Some(vec![2, 1]));
        assert_eq!(nav(&mut p), None, "two-call semantics");
    }

    #[test]
    fn leaving_an_empty_columns_slot_asks_for_nothing() {
        // There is no card to land on, so the column row the list already shows
        // is as close as it gets.
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.focus = Focus { col: 2, row: 0 };
        p.leave_dashboard();
        assert_eq!(nav(&mut p), None);
    }

    #[test]
    fn leaving_mid_edit_keeps_the_text_and_still_follows_the_card() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Ch('o')));
        p.dashboard_text("half typed");
        p.leave_dashboard();
        assert_eq!(cards(&p, 0)[1], "half typed", "leaving is not a cancel");
        assert_eq!(nav(&mut p), Some(vec![1, 2]));
    }

    #[test]
    fn the_board_declares_it_wants_the_apps_undo() {
        let d = ProjectManagementProvider::new().describe();
        assert!(d.dashboard_uses_app_undo);
        assert!(matches!(d.dashboard_kind, DashboardKind::Interactive));
    }

    // ---- The archive ----------------------------------------------------

    /// Archive whatever `elem_key` names, through the same entry point the app
    /// uses. `elem_key` is ignored on the board, which is the point of it.
    fn archive(p: &mut ProjectManagementProvider, elem_key: &str) {
        let out = p
            .handle_command(CMD_ARCHIVE, elem_key, 0)
            .unwrap_or_else(|err| panic!("and must never set an error: {err}"));
        assert!(out.is_none(), "the archive command must return no element");
    }

    /// The row label the list cursor would be on, for a card in a column.
    fn row_key(p: &mut ProjectManagementProvider, title: &str, card: &str) -> String {
        descend(p, title);
        let rows = p.fetch();
        let at = labels(&rows).iter().position(|l| l == card).expect(card);
        raw_of(&rows[at]).to_owned()
    }

    /// Every character the board actually draws.
    fn board_text(p: &mut ProjectManagementProvider) -> String {
        let f = p.render_frame(120, 40);
        (0..f.rows)
            .map(|y| (0..f.cols).map(|x| f.cell(x, y).ch).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_archive_never_appears_on_the_board() {
        // The headline requirement: hidden in dashboard mode, and it must stay
        // hidden however many columns are on the board.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        let drawn = board_text(&mut p);
        assert!(
            !drawn.contains("Archive"),
            "the archive must not be drawn: {drawn}"
        );
        assert!(drawn.contains("To do") && drawn.contains("Doing"));
        assert_eq!(p.board.visible_len(), 2);
        assert_eq!(p.board.columns.len(), 3, "it exists, it is just not shown");
    }

    #[test]
    fn the_archive_does_appear_in_the_list() {
        // The other half: visible in general mode, and descendable, so archived
        // cards stay readable and can be moved back by hand.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();
        assert!(labels(&p.fetch()).iter().any(|l| l == "Archive"));

        descend(&mut p, "Archive");
        assert_eq!(labels(&p.fetch()), vec![meta().as_str(), "fix login"]);
    }

    #[test]
    fn archiving_from_the_board_takes_the_focused_card_not_the_list_cursor() {
        // In the dashboard the `elem_key` the app passes names whatever the list
        // cursor was on before `d` was pressed. Acting on it would archive a card
        // the user is not looking at.
        let mut p = seeded();
        let stale = row_key(&mut p, "To do", "write docs");
        p.pop_path();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 }; // "kanban ui", in Doing
        archive(&mut p, &stale);
        assert_eq!(cards(&p, 1), Vec::<String>::new());
        assert_eq!(cards(&p, 0), vec!["fix login", "write docs"]);
        assert_eq!(cards(&p, 2), vec!["kanban ui"]);
    }

    #[test]
    fn archiving_from_the_list_takes_the_row_under_the_cursor() {
        let mut p = seeded();
        let key = row_key(&mut p, "To do", "write docs");
        archive(&mut p, &key);
        assert_eq!(cards(&p, 0), vec!["fix login"]);
        assert_eq!(cards(&p, 2), vec!["write docs"]);
    }

    #[test]
    fn archiving_from_the_list_leaves_the_cursor_on_the_column_it_came_from() {
        let mut p = seeded();
        let key = row_key(&mut p, "To do", "write docs");
        archive(&mut p, &key);
        assert_eq!(
            nav(&mut p),
            Some(vec![1]),
            "without this the app unwinds the cursor to the board root"
        );
    }

    #[test]
    fn archiving_from_the_board_asks_for_no_navigation() {
        // A request queued while the app is in Dashboard is drained and dropped,
        // never deferred, and the board owns its own cursor anyway.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        assert_eq!(nav(&mut p), None);
    }

    #[test]
    fn undo_puts_an_archived_card_back_where_it_was() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 0, row: 1 }; // "write docs", second in To do
        archive(&mut p, "");
        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].command == "archive-card",
            "the undo screen must not call this a move: {:?}",
            entries[0]
        );

        p.undo(&entries[0]).unwrap();
        assert_eq!(
            cards(&p, 0),
            vec!["fix login", "write docs"],
            "back in its own column, at its own index"
        );
        assert_eq!(cards(&p, 2), Vec::<String>::new());

        p.redo(&entries[0]).unwrap();
        assert_eq!(cards(&p, 0), vec!["fix login"]);
        assert_eq!(cards(&p, 2), vec!["write docs"]);
    }

    #[test]
    fn undo_leaves_the_archive_column_standing() {
        // Deliberate, and documented on `archive_card`: the column belongs to the
        // list surface, where the app records its own structural entries.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        let entries = p.take_timeline_entries();
        p.undo(&entries[0]).unwrap();
        assert!(p.board.archive_id().is_some());
        assert_eq!(p.board.columns.len(), 3);
    }

    #[test]
    fn archiving_twice_makes_only_one_archive() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        archive(&mut p, "");
        assert_eq!(p.board.columns.len(), 3);
        assert_eq!(cards(&p, 2), vec!["fix login", "write docs"]);
    }

    #[test]
    fn a_renamed_archive_is_still_the_archive() {
        // Identity is the id, never the title, so the user can call it whatever
        // they like and go on archiving into it.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();
        let id = p.board.archive_id().expect("an archive");

        let rows = p.fetch();
        let renamed: Vec<FfonElement> = rows
            .iter()
            .map(|e| {
                let raw = raw_of(e);
                if row_id(raw) == Some(id) {
                    FfonElement::new_obj(ProjectManagementProvider::row_label(id, "Cold storage"))
                } else {
                    e.clone()
                }
            })
            .collect();
        p.sync_ffon_body_children(&renamed);

        assert_eq!(
            p.board.archive_id(),
            Some(id),
            "renaming is not unarchiving"
        );
        assert_eq!(p.board.column(id).unwrap().title, "Cold storage");
        assert_eq!(p.board.visible_len(), 2);

        p.enter_dashboard();
        archive(&mut p, "");
        assert_eq!(p.board.columns.len(), 3, "still one archive");
    }

    #[test]
    fn deleting_the_archive_in_the_list_clears_the_flag() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();
        let id = p.board.archive_id().expect("an archive");

        let kept: Vec<FfonElement> = p
            .fetch()
            .into_iter()
            .filter(|e| row_id(raw_of(e)) != Some(id))
            .collect();
        p.sync_ffon_body_children(&kept);

        assert_eq!(p.board.archive_id(), None);
        assert_eq!(p.board.visible_len(), 2);
    }

    #[test]
    fn a_reordered_list_puts_the_archive_back_at_the_end() {
        // `reconcile_columns` takes the app's order wholesale, so the invariant
        // has to be re-established afterwards or the archive lands on the board.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();

        let mut rows = p.fetch();
        rows.reverse(); // archive first
        p.sync_ffon_body_children(&rows);

        assert_eq!(
            p.board.columns.last().unwrap().id,
            p.board.archive_id().unwrap()
        );
        assert!(!board_text(&mut p).contains("Archive"));
    }

    #[test]
    fn the_board_says_no_columns_when_only_the_archive_is_left() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        let id = p.board.archive_id().unwrap();
        p.board.columns.retain(|c| c.id == id);
        assert!(board_text(&mut p).contains("no columns yet"));
    }

    #[test]
    fn nothing_walks_onto_the_archive_from_the_board() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.focus = Focus { col: 1, row: 0 };
        p.dashboard_key(key(Keysym::Right));
        assert_eq!(p.focus.col, 1, "right from the last real column stays put");
    }

    #[test]
    fn move_right_from_the_last_column_cannot_reach_the_archive() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        let key = row_key(&mut p, "Doing", "kanban ui");
        assert!(!p.move_card_sideways(true, &key));
        assert_eq!(cards(&p, 1), vec!["kanban ui"]);
    }

    #[test]
    fn move_left_walks_a_card_back_out_of_the_archive() {
        // The only route back, and why no `unarchive` command is needed.
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();
        let key = row_key(&mut p, "Archive", "fix login");
        assert!(p.move_card_sideways(false, &key));
        assert_eq!(cards(&p, 1), vec!["kanban ui", "fix login"]);
        assert_eq!(cards(&p, 2), Vec::<String>::new());
    }

    #[test]
    fn no_column_can_be_reordered_past_the_archive() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        p.leave_dashboard();
        p.pop_path();
        let rows = p.fetch();
        let doing = raw_of(&rows[2]).to_owned();
        let arch = raw_of(&rows[3]).to_owned();
        assert!(!p.move_row_in_list(true, &doing), "down past the archive");
        assert!(!p.move_row_in_list(false, &arch), "and the archive itself");
        assert_eq!(
            p.board.columns.last().unwrap().id,
            p.board.archive_id().unwrap()
        );
    }

    #[test]
    fn the_board_offers_only_the_archive_command() {
        // The four move verbs act on the list cursor and return an element, which
        // would drop the app out of the dashboard behind this provider's back.
        let mut p = seeded();
        assert_eq!(p.commands().len(), 5);
        p.enter_dashboard();
        assert_eq!(p.commands(), vec![CMD_ARCHIVE.to_owned()]);
        p.leave_dashboard();
        assert!(p.commands().contains(&CMD_MOVE_UP.to_owned()));
        assert!(p.commands().contains(&CMD_ARCHIVE.to_owned()));
    }

    #[test]
    fn an_empty_slot_is_said_aloud_rather_than_raised_as_an_error() {
        // An error would take the app down a path that resets its coordinate
        // without ever calling `leave_dashboard`, stranding the board.
        let mut p = seeded();
        p.board.columns.push(Column::new(9, "Done"));
        p.enter_dashboard();
        p.focus = Focus { col: 2, row: 0 }; // the placeholder
        archive(&mut p, "");
        assert_eq!(p.take_error(), None);
        assert_eq!(
            p.take_announcement(),
            Some("nothing to archive, put the cursor on a card first".to_owned())
        );
        assert_eq!(p.board.columns.len(), 3, "and no archive was made for it");
    }

    #[test]
    fn archiving_an_archived_card_says_so_and_stops() {
        let mut p = seeded();
        p.enter_dashboard();
        archive(&mut p, "");
        let _ = p.take_announcement();
        p.focus = Focus { col: 2, row: 0 };
        // `clamp_focus` keeps the board off the archive, so reach it as the list
        // would: by the row's own key.
        p.leave_dashboard();
        p.pop_path();
        let key = row_key(&mut p, "Archive", "fix login");
        archive(&mut p, &key);
        assert_eq!(p.take_announcement(), Some("already archived".to_owned()));
        assert_eq!(cards(&p, 2), vec!["fix login"], "and nothing moved");
    }

    #[test]
    fn an_archive_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let mut p = provider(&dir);
        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.board.columns[0].cards.push(Card::new(2, "fix login"));
        p.board.reseat_counter();
        p.enter_dashboard();
        archive(&mut p, "");

        let mut again = provider(&dir);
        again.ensure_loaded();
        assert_eq!(again.board.visible_len(), 1);
        assert_eq!(
            again
                .board
                .column(again.board.archive_id().unwrap())
                .unwrap()
                .cards[0]
                .text,
            "fix login"
        );
    }

    // ---- Registration and locales ---------------------------------------

    #[test]
    fn the_display_name_matches_the_factory_key_once_spaces_are_stripped() {
        // The settings panel matches a section to a provider that way; a mismatch
        // silently drops the section rather than failing anywhere visible.
        let d = ProjectManagementProvider::new().describe();
        assert_eq!(d.display_name.replace(' ', ""), d.name);
    }

    #[test]
    fn all_four_bundles_carry_the_same_keys() {
        fn keys(src: &str) -> Vec<String> {
            src.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, _)| k.trim().to_owned())
                .filter(|k| !k.is_empty() && !k.starts_with('#'))
                .collect()
        }
        let en = keys(include_str!("../locales/en-US.ftl"));
        for (name, src) in [
            ("nl-BE", include_str!("../locales/nl-BE.ftl")),
            ("fr-BE", include_str!("../locales/fr-BE.ftl")),
            ("de-BE", include_str!("../locales/de-BE.ftl")),
        ] {
            assert_eq!(keys(src), en, "{name} has drifted from en-US");
        }
    }

    // -----------------------------------------------------------------------
    // Cloud backup
    // -----------------------------------------------------------------------

    /// `localize::t` hands back the key when a string is missing, so a
    /// forgotten entry would show in Settings as `projectmanagement-checkbox-cloud-backup`.
    #[test]
    fn the_cloud_backup_checkbox_label_resolves() {
        let label = localize::t("projectmanagement-checkbox-cloud-backup");
        assert_ne!(label, "projectmanagement-checkbox-cloud-backup");
        assert!(label.contains("cloud"), "{label}");
    }

    /// Every message the backup service can show, under this plugin's
    /// prefix. A missing one would show as its id.
    #[test]
    fn every_cloud_message_resolves() {
        for id in sicompass_sync::cloud::MESSAGES {
            let id = format!("projectmanagement-{id}");
            assert_ne!(localize::t(&id), id);
        }
    }

    /// With the switch off, nothing about payment appears anywhere. A user who
    /// never asked for cloud backup should not be able to tell it exists.
    #[test]
    fn no_cloud_row_until_the_setting_is_on() {
        let mut p = seeded();
        let shown = labels(&p.fetch());
        assert!(
            !shown.iter().any(|l| l.contains("cloud")),
            "nothing about cloud backup while the switch is off: {shown:?}"
        );
    }

    #[test]
    fn the_cloud_row_leads_the_columns_level_once_switched_on() {
        let mut p = seeded_with_cloud(Standing::Missing);
        let rendered = p.fetch();
        // A plain row: it says where the user stands and links nowhere, since
        // buying and redeeming are in store, tiers.
        let FfonElement::Str(first) = &rendered[0] else {
            panic!("the cloud row is a plain row, not something to follow");
        };
        assert!(cloud::is_row(first), "{first}");
        assert!(!first.contains("<link>"), "{first}");
        // And the board is still there, below it.
        let shown = labels(&rendered);
        assert!(shown[0].contains("store, tiers"), "{shown:?}");
        assert!(shown.iter().any(|l| l == "To do"), "{shown:?}");
        assert!(shown.iter().any(|l| l == "Doing"), "{shown:?}");
    }

    #[test]
    fn the_cloud_row_wording_changes_once_it_is_paid_for() {
        let unpaid = labels(&seeded_with_cloud(Standing::Missing).fetch())[0].clone();
        let paid = labels(&seeded_with_cloud(active_licence()).fetch())[0].clone();
        assert_ne!(unpaid, paid);
        assert!(paid.contains("342"), "{paid}");
    }

    /// Cards are not a place to put a subscription notice.
    #[test]
    fn the_cloud_row_appears_on_the_columns_level_only() {
        let mut p = seeded_with_cloud(active_licence());
        p.fetch();
        p.push_path("To do");
        let inside = labels(&p.fetch());
        assert!(!inside.iter().any(|l| l.contains("cloud")), "{inside:?}");
    }

    /// An empty board with backup on must still offer its "no columns yet"
    /// line: the cloud row is not board content.
    #[test]
    fn an_empty_board_keeps_its_placeholder_beside_the_cloud_row() {
        let mut p = ProjectManagementProvider::with_host(Box::new(FakeHost::new(active_licence())));
        p.loaded = true;
        p.on_setting_change(cloud::ENABLE_KEY, "true");

        let shown = labels(&p.fetch());
        assert_eq!(shown.len(), 3, "{shown:?}");
        assert_eq!(
            shown[2],
            localize::t("projectmanagement-empty-columns"),
            "{shown:?}"
        );
    }

    /// The one that would eat a user's board. The app hands back whatever it
    /// was displaying, so a cloud row that survives `reconcile` becomes a
    /// column the moment anything else is typed.
    #[test]
    fn the_cloud_row_is_never_stored_as_a_column() {
        let mut p = seeded_with_cloud(Standing::Missing);
        let unchanged = p.fetch();
        assert!(matches!(&unchanged[0], FfonElement::Str(s) if cloud::is_row(s)));
        p.sync_ffon_body_children(&unchanged);

        let titles: Vec<String> = p.board.columns.iter().map(|c| c.title.clone()).collect();
        assert_eq!(
            titles,
            vec!["To do".to_owned(), "Doing".to_owned()],
            "{titles:?}"
        );
    }

    /// The backup row belongs to the switch in settings, not to the board.
    #[test]
    fn the_cloud_row_cannot_be_deleted() {
        let mut p = seeded_with_cloud(active_licence());
        let row = raw_of(&p.fetch()[0]).to_owned();
        assert!(!p.delete_item(&row));
        assert!(p.take_error().is_some());
    }

    #[test]
    fn switching_on_without_a_subscription_is_announced() {
        let mut p = seeded_with_cloud(Standing::Missing);
        let spoken = p
            .take_announcement()
            .expect("the screen reader must say why");
        assert!(spoken.contains("Sicompass Cloud"), "{spoken}");
    }

    #[test]
    fn the_sync_command_is_offered_only_with_sync_on() {
        let p = seeded();
        assert!(!p.commands().contains(&CMD_SYNC_NOW.to_owned()));
        let p = seeded_with_cloud(active_licence());
        assert!(p.commands().contains(&CMD_SYNC_NOW.to_owned()));
    }

    #[test]
    fn sync_now_starts_a_sync_at_once() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(active_licence());
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.handle_command(CMD_SYNC_NOW, "", 0).unwrap();
        assert_eq!(
            host.spawned(),
            vec![(cloud::TASK_SYNC.to_owned(), Vec::new())]
        );
    }

    /// What a finished sync reports, as the task hands it back.
    fn outcome(o: &sicompass_sync::sync::Outcome) -> Result<Vec<u8>, String> {
        Ok(serde_json::to_vec(o).unwrap())
    }

    /// The board another computer would have, as a sync hands it over.
    fn merged_board(columns: &[&str]) -> sicompass_sync::sync::Outcome {
        let elsewhere = TempDir::new().unwrap();
        let mut other = provider(&elsewhere);
        other.ensure_loaded();
        for (i, title) in columns.iter().enumerate() {
            other.board.columns.push(Column::new(i as Id + 1, *title));
        }
        other.persist();
        let files = sicompass_sync::snapshot::read_store(&elsewhere.path().join("board"), "kanban")
            .unwrap()
            .files;
        sicompass_sync::sync::Outcome::Apply {
            files,
            hash: "h-merged".to_owned(),
            updated_at: Some(1),
            conflicts: 0,
        }
    }

    /// What a sync merged in is written, the board is read again, and that is
    /// said. On a new computer this is how the board comes back.
    #[test]
    fn a_merge_from_another_computer_reloads_the_board() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(active_licence());
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.fetch();
        p.take_announcement();
        p.poll();
        assert_eq!(
            host.spawned(),
            vec![(cloud::TASK_SYNC.to_owned(), Vec::new())]
        );

        p.task_done(1, outcome(&merged_board(&["From the cloud"])));
        let poll = p.poll();
        assert!(poll.needs_refresh);
        assert_eq!(
            poll.announcement,
            Some(localize::t("projectmanagement-sync-pulled"))
        );
        assert!(labels(&p.fetch()).contains(&"From the cloud".to_owned()));
    }

    /// A merge computed before the user typed must not land over what they
    /// typed: it is dropped, and the next sync merges again, edit included.
    #[test]
    fn a_merge_does_not_overwrite_an_edit_made_meanwhile() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(active_licence());
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.fetch();
        p.poll();
        p.board.columns.push(Column::new(1, "Typed here"));
        p.persist();

        p.task_done(1, outcome(&merged_board(&["From the cloud"])));
        let shown = labels(&p.fetch());
        assert!(shown.contains(&"Typed here".to_owned()), "{shown:?}");
        assert!(!shown.contains(&"From the cloud".to_owned()), "{shown:?}");
    }

    /// Ids handed out before a merge stay handed out: the undo timeline may
    /// still hold one for a deleted card.
    #[test]
    fn the_counter_stays_above_ids_handed_out_before_a_merge() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(active_licence());
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.ensure_loaded();
        for _ in 0..5 {
            let id = p.board.mint_id();
            p.board.columns.push(Column::new(id, format!("c{id}")));
        }
        p.persist();
        p.poll();
        let floor = p.board.next_id();
        p.task_done(1, outcome(&merged_board(&["x"])));
        assert_eq!(p.board.columns.len(), 1, "the merge landed");
        assert!(p.board.next_id() >= floor);
    }

    /// A sync runs at start-up, to pick up what another computer changed,
    /// then a while after the board goes quiet, never on the UI.
    #[test]
    fn a_sync_runs_at_start_up_and_once_the_board_is_quiet() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(Standing::Grace { days_left: 2 });
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        assert!(p.poll().is_busy);
        assert_eq!(
            host.spawned(),
            vec![(cloud::TASK_SYNC.to_owned(), Vec::new())]
        );
        p.task_done(1, outcome(&sicompass_sync::sync::Outcome::UpToDate));

        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.persist();
        host.advance(1_000);
        p.poll();
        assert_eq!(host.spawned().len(), 1, "still arranging");

        host.advance(sicompass_sync::debounce::DEBOUNCE_MS);
        assert!(p.poll().is_busy);
        assert_eq!(host.spawned().len(), 2);
    }

    /// The paywall is on the service: without a subscription the board is
    /// still saved, and only the sync does not happen.
    #[test]
    fn nothing_is_synced_without_a_subscription() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(Standing::Expired { days_ago: 30 });
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.persist();
        host.advance(sicompass_sync::debounce::DEBOUNCE_MS + 1);
        p.poll();
        assert!(host.spawned().is_empty());
        assert!(store::load_board(&dir.path().join("board")).is_some_and(|b| b.columns.len() == 1));
    }

    /// The board's poll carries the requests the dashboard used to hand over
    /// one by one: leaving, and where the list cursor should land.
    #[test]
    fn poll_hands_over_the_dashboards_requests() {
        let mut p = seeded();
        p.enter_dashboard();
        p.dashboard_key(key(Keysym::Escape));
        let poll = p.poll();
        assert!(matches!(
            poll.dashboard_request,
            Some(DashboardRequest::Leave)
        ));
        p.leave_dashboard();
        assert!(matches!(
            p.poll().navigation_request,
            Some(NavigationRequest::SelectPath(ref at)) if at == &[1, 1]
        ));
    }

    /// The frame crosses to the host cell for cell.
    #[test]
    fn the_frame_crosses_the_plugin_interface_unchanged() {
        let mut p = seeded();
        p.enter_dashboard();
        let ours = p.render_frame(60, 12);
        let theirs = p.dashboard_render(60, 12);
        assert_eq!((theirs.cols, theirs.rows), (ours.cols, ours.rows));
        assert_eq!(theirs.cells.len(), ours.cells.len());
        assert!(
            ours.cells
                .iter()
                .zip(&theirs.cells)
                .all(|(a, b)| a.ch == b.ch && a.fg == b.fg && a.bg == b.bg)
        );
        assert_eq!(theirs.cursor, ours.cursor);
        assert_eq!(theirs.half_gap_rows, ours.half_gap_rows);
    }

    // ---- The list meta --------------------------------------------------

    fn meta() -> String {
        localize::t("projectmanagement-list-meta")
    }

    /// Into the list meta of the level the cursor is on, as the app goes:
    /// render, then push the label.
    fn enter_meta(p: &mut ProjectManagementProvider) -> Vec<String> {
        let _ = p.fetch();
        p.push_path(&meta());
        labels(&p.fetch())
    }

    #[test]
    fn every_list_opens_with_its_list_meta() {
        let mut p = seeded();
        let root = p.fetch();
        assert!(matches!(&root[0], FfonElement::Obj(o) if o.key == meta()));
        descend(&mut p, "To do");
        let cards = p.fetch();
        assert!(matches!(&cards[0], FfonElement::Obj(o) if o.key == meta()));
        assert!(cards[1..].iter().all(|e| e.is_str()), "cards stay Str");
    }

    #[test]
    fn with_sync_on_the_meta_comes_right_below_the_cloud_row() {
        let mut p = seeded_with_cloud(active_licence());
        let rows = p.fetch();
        assert!(cloud::is_row(raw_of(&rows[0])));
        assert_eq!(raw_of(&rows[1]), meta());
    }

    /// The board's root hash at the top, a column's own hash inside it: one
    /// glance says whether anything below has changed.
    #[test]
    fn the_meta_shows_the_board_hash_at_the_top_and_the_column_hash_inside() {
        let mut p = seeded();
        let top = enter_meta(&mut p);
        assert!(
            top.iter().any(|l| l.contains(&p.board.root_hash_hex())),
            "{top:?}"
        );
        p.pop_path();
        assert!(p.at_root(), "out of the meta, back on the columns");

        descend(&mut p, "Doing");
        let inside = enter_meta(&mut p);
        assert!(
            inside
                .iter()
                .any(|l| l.contains(&p.board.columns[1].hash_hex())),
            "{inside:?}"
        );
        p.pop_path();
        assert_eq!(
            p.current_path(),
            "/c4",
            "out of the meta, still in the column"
        );
    }

    /// The one that would eat a board: the app hands back what it displayed,
    /// meta row included.
    #[test]
    fn the_meta_row_is_never_stored() {
        let mut p = seeded_with_cloud(Standing::Missing);
        let rows = p.fetch();
        p.sync_ffon_body_children(&rows);
        let titles: Vec<String> = p.board.columns.iter().map(|c| c.title.clone()).collect();
        assert_eq!(titles, vec!["To do", "Doing"]);

        descend(&mut p, "To do");
        let rows = p.fetch();
        p.sync_ffon_body_children(&rows);
        assert_eq!(cards(&p, 0), vec!["fix login", "write docs"]);
    }

    #[test]
    fn the_meta_row_cannot_be_deleted_or_edited() {
        let mut p = seeded();
        assert!(!p.delete_item(&meta()));
        assert!(p.take_error().is_some());
        assert!(!p.commit_edit(&meta(), "renamed"));
        assert!(p.take_error().is_some());
    }

    /// The lines inside the meta are rendered, and an edit handed back from
    /// there must not reach the board.
    #[test]
    fn nothing_inside_the_meta_reaches_the_board() {
        let mut p = seeded();
        enter_meta(&mut p);
        let before = p.board.clone();
        let lines = p.fetch();
        p.sync_ffon_body_children(&lines);
        assert_eq!(p.board, before);
    }

    #[test]
    fn the_meta_paths_round_trip_in_both_forms() {
        let mut p = seeded();
        enter_meta(&mut p);
        assert_eq!(p.current_path(), "/m");
        assert!(!p.at_root());

        let mut q = seeded();
        q.set_current_path("/m");
        assert_eq!(q.current_path(), "/m");
        assert_eq!(q.fetch().len(), 1, "the hash line");

        let mut q = seeded();
        q.set_current_path("/c4/m");
        assert_eq!(q.current_path(), "/c4/m");
        q.pop_path();
        assert_eq!(labels(&q.fetch()), vec![meta(), "kanban ui".to_owned()]);

        let mut q = seeded();
        let _ = q.fetch();
        q.set_current_path(&format!("/Doing/{}", meta()));
        assert_eq!(q.current_path(), "/c4/m");
    }

    /// The app counts the dashboard's entry path and `SelectPath` in `fetch()`
    /// rows, the cloud row and the list meta included.
    #[test]
    fn the_board_skips_the_rows_above_the_columns_both_ways() {
        let mut p = seeded_with_cloud(active_licence());
        // Cloud row, meta, To do, Doing: the cursor on "Doing", second card
        // row (its meta first).
        p.set_dashboard_entry(&[3, 1]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 1, row: 0 });
        p.leave_dashboard();
        assert_eq!(nav(&mut p), Some(vec![3, 1]));
    }

    #[test]
    fn entering_the_board_from_the_meta_row_leaves_the_focus_alone() {
        let mut p = seeded();
        p.enter_dashboard();
        p.focus = Focus { col: 1, row: 0 };
        p.leave_dashboard();
        let _ = nav(&mut p);
        p.set_dashboard_entry(&[0]);
        p.enter_dashboard();
        assert_eq!(p.focus, Focus { col: 1, row: 0 });
    }

    /// The list meta says whether its list is as it was at the last sync,
    /// from the same Merkle hash it shows.
    #[test]
    fn the_meta_says_whether_the_list_is_synced() {
        let dir = TempDir::new().unwrap();
        let host = FakeHost::new(active_licence());
        let mut p = provider_on(&dir, &host);
        p.on_setting_change(cloud::ENABLE_KEY, "true");
        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.persist();
        let status = |p: &mut ProjectManagementProvider| {
            let lines = enter_meta(p);
            p.pop_path();
            lines
        };
        assert!(status(&mut p).contains(&localize::t("projectmanagement-sync-status-new")));

        let root = dir.path().join("board");
        let files = sicompass_sync::snapshot::read_store(&root, "kanban")
            .unwrap()
            .files;
        sicompass_sync::sync::Base {
            hash: "h".to_owned(),
            updated_at: None,
            files,
        }
        .save(&root)
        .unwrap();
        p.cloud.load_base(&root);
        assert!(status(&mut p).contains(&localize::t("projectmanagement-sync-status-synced")));

        p.board.columns[0].title = "To do soon".to_owned();
        p.persist();
        assert!(status(&mut p).contains(&localize::t("projectmanagement-sync-status-changed")));
    }

    #[test]
    fn no_sync_line_in_the_meta_while_sync_is_off() {
        let mut p = seeded();
        let lines = enter_meta(&mut p);
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    /// The task, against a server that holds nothing yet: it asks the head,
    /// then uploads the board under its server name, `kanban`.
    #[test]
    fn the_sync_task_uploads_the_board_to_its_server() {
        let dir = TempDir::new().unwrap();
        let mut p = provider(&dir);
        p.ensure_loaded();
        p.board.columns.push(Column::new(1, "To do"));
        p.persist();

        let sent = std::cell::RefCell::new(Vec::new());
        let send = |r: &sicompass_sync::protocol::Request| {
            sent.borrow_mut().push(r.clone());
            let body = if r.method == "GET" {
                br#"{"hash":null,"updated_at":null,"now":1}"#.to_vec()
            } else {
                br#"{"stored":true,"updated_at":1}"#.to_vec()
            };
            Ok(sicompass_sync::protocol::Response { status: 200, body })
        };
        sicompass_sync::cloud::run_sync(
            &cloud::SERVICE,
            &dir.path().join("board"),
            Some("tok".to_owned()),
            &send,
        )
        .unwrap();
        let sent = sent.borrow();
        assert_eq!(
            sent[0].url,
            "https://store.sicompass.org/plugins/kanban/head"
        );
        assert_eq!(sent[1].method, "PUT");
        assert_eq!(sent[1].url, "https://store.sicompass.org/plugins/kanban");
    }
}

/// The tutorial's paragraphs about this plugin are the plugin's own:
/// `projectmanagement-tutorial`, then `projectmanagement-tutorial-2` and so on. The tutorial reads them
/// from the installed `locales/*.ftl`, so every language needs the same ones.
#[cfg(test)]
mod tutorial_text_tests {
    const LOCALES: [(&str, &str); 4] = [
        ("en-US", include_str!("../locales/en-US.ftl")),
        ("nl-BE", include_str!("../locales/nl-BE.ftl")),
        ("fr-BE", include_str!("../locales/fr-BE.ftl")),
        ("de-BE", include_str!("../locales/de-BE.ftl")),
    ];

    /// The plugin's name, kept apart from the `-tutorial` suffix so no
    /// half-built id appears quoted in the source.
    const NAME: &str = "projectmanagement";

    fn tutorial_id(line: &str) -> Option<&str> {
        let id = line.split_once(" = ")?.0;
        let base = format!("{NAME}-tutorial");
        (id == base || id.starts_with(&format!("{base}-"))).then_some(id)
    }

    fn tutorial_ids(ftl: &str) -> Vec<&str> {
        ftl.lines().filter_map(tutorial_id).collect()
    }

    #[test]
    fn every_language_has_the_same_tutorial_leaves() {
        let en = tutorial_ids(LOCALES[0].1);
        assert_eq!(
            en,
            ["projectmanagement-tutorial", "projectmanagement-tutorial-2"],
            "en-US's tutorial leaves"
        );
        for (locale, ftl) in &LOCALES[1..] {
            assert_eq!(tutorial_ids(ftl), en, "{locale} has drifted from en-US");
        }
    }
}
