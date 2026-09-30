//! ashiato — flat mtime-sorted file picker TUI (spec: ashiato-spec.md).
//!
//! Lists every file under a project root, newest first, clustered by
//! day: Today / Yesterday, then one date header (`Wed, Aug 4`) per
//! older day — no week/month/Older buckets.
//! Space multi-selects, Enter hands the files to `--open-cmd` (default
//! `akapen`), blocks until it exits, then rescans. `/` filters
//! incrementally, `t` cycles the sort, Ctrl+h toggles hidden dirs, `d`
//! toggles directories, `y` copies paths. Without `--open-cmd`, Enter
//! prints the selected paths to stdout and exits (generic picker, fzf
//! model); with it, Enter launches the command, blocks, and rescans
//! (the akapen review-loop flow). With `--alt-open-cmd`, the `o` key runs
//! a second command in the same blocking + rescan flow (e.g. Enter =
//! akapen review, `o` = yazi).
//!
//! Away-diff (terminal focus reporting): while the terminal is
//! unfocused, external edits accumulate in a stack; the rows keep their
//! normal colors, and only the *untouched* files' times dim — the
//! touched files' times stay in the usual gray, so freshness reads as
//! contrast in the time column alone. Focus return reverts instantly
//! (no flash). Terminals without focus events keep the feature dormant.
//!
//! The root resolves from: the positional argument → herdr (`HERDR_ENV=1`
//! → `herdr worktree list` → `herdr agent list`) → the current directory.
//!
//! Git integration (git-integration-spec.md §4) is a dormant add-on:
//! inside a work tree, rows with uncommitted changes get a `+N -M`
//! marker (change scale fused with the uncommitted signal, spec 4-1)
//! and `u` filters the listing to them (4-2). Outside a repo all of
//! it is off — the exact pre-git behavior (P1). The cache strategy
//! (git is queried only when the listing changes) lives in git.rs.
//!
//! This prototype shares akapen's `highlight` module (syntect) so it
//! can later move into the akapen repo as `src/bin/ashiato.rs`.

mod clipboard;
mod config_file;
mod files;
mod git;
mod herdr;
mod highlight;
mod preview;
mod theme;

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::TimeZone;
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use termtheme::config::ThemeFlags;
use termtheme::input::{self, Input};
use termtheme::scheme::Subscription;
use termtheme::theme::ThemePair;

use crate::config_file::ConfigFile;
use crate::files::{Cluster, FileEntry, Sort, cluster_of, format_time, is_fresh, to_local};
use crate::highlight::Highlighter;
use crate::preview::{Preview, PreviewKey};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Event poll/tick cadence, ms.
const TICK_MS: u64 = 100;
/// Input events closer together than this are a "burst" (held j/k repeat,
/// fast wheel scrolls): the preview render is deferred while bursting so
/// every cursor move costs only the list draw (speed-first spec — the
/// preview catches up the moment the input pauses).
const BURST_GAP: Duration = Duration::from_millis(40);
/// Events processed per frame at most (akapen's anti-freeze pattern:
/// a burst is drained once and drawn once).
const MAX_EVENTS_PER_FRAME: usize = 64;
/// Transient footer messages live this long.
const STATUS_SECS: Duration = Duration::from_secs(4);
/// How often the listing silently refreshes for external changes.
const REFRESH_TICK: Duration = Duration::from_secs(2);
/// Command-line configuration (spec: 起動 → フラグ).
struct Config {
    /// Explicit directory argument (spec priority 1); `None` = herdr → cwd.
    dir: Option<PathBuf>,
    /// Initial sort order (`--sort mtime|ctime`; the `t` key cycles).
    sort: Sort,
    /// `--show-hidden`: show dot-files on startup.
    show_hidden: bool,
    /// `--show-dirs`: show directories on startup.
    show_dirs: bool,
    /// `--open-cmd <command>`: Enter spawns this with `{}` / `{1}`..
    /// placeholders. `None` (the default) = Enter prints the selected
    /// paths to stdout and exits (generic picker, fzf model).
    open_cmd: Option<String>,
    /// `--alt-open-cmd <command>`: the `o` key spawns this with the same
    /// `{}` / `{1}` placeholders and blocking + rescan flow as Enter's
    /// `--open-cmd`. `None` (the default) = `o` is a dead key.
    alt_open_cmd: Option<String>,
    /// `--filter <text>`: initial filter applied at startup (same
    /// matching as the `/` key; empty = no filter).
    filter: String,
    /// `--preview <on|off|auto>`: preview pane behavior (default auto).
    preview: PreviewMode,
    /// `--files`: no TUI — print the collected list to stdout and exit
    /// (the time-ordered listing as a data source for pipes/fzf).
    files: bool,
    /// `--format <path|tsv>` (with `--files`): path only, or
    /// `mtime<TAB>path` for a displayable time column.
    format: OutputFormat,
    /// `--since <today|yesterday|Nd|Nw>` (with `--files`): only entries
    /// modified at/after the cutoff.
    since: Option<Since>,
    /// `--output`: Enter prints the selected paths to stdout and exits.
    output: bool,
    /// シンタックスハイライトのテーマ。背景が dark のときと light のときの
    /// 2 本で、どちらを使うかは light/dark の判定（`--light` / `--dark`、無ければ
    /// 起動時の OSC 11 と、開いている間の配色の知らせ）が決める
    /// （`ThemePair::for_background`）。
    ///
    /// 各側は `--theme-dark` / `--theme-light` > 設定ファイルの `[theme]` >
    /// 既定。`--theme <name>` は**両側を上書きする**（どちらでもそれを使う。
    /// 1 本だったころの意味のまま）。値は syntect のテーマ名か `.tmTheme` の
    /// パス。`None` の側は既定 —— [`Highlighter::new`] が light/dark に合ったもの
    /// （`Catppuccin Mocha` / `Solarized (light)`）を選ぶ。名前が解決できない
    /// ときも同じ既定へ落ちる。
    theme: ThemePair,
    /// `--light` / `--dark`: force the TUI's light/dark mode.
    /// `None` (the default) = auto-detect the terminal background via
    /// OSC 11, falling back to dark when the terminal doesn't answer.
    /// 固定されているときは、端末の配色の知らせ（モード 2031）を無視する。
    light: Option<bool>,
}

/// What the process should do, resolved from argv.
enum Action {
    Run(Config),
    Help,
    Version,
}

/// Preview pane behavior (`--preview`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PreviewMode {
    /// Always show the preview pane.
    On,
    /// Never show it (the list takes the full width).
    Off,
    /// Show it, but hide it on narrow terminals (list takes the full
    /// width below [`PREVIEW_MIN_WIDTH`] columns).
    Auto,
}

impl PreviewMode {
    /// Parse `--preview <on|off|auto>`; `None` for unknown values.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "on" => Some(PreviewMode::On),
            "off" => Some(PreviewMode::Off),
            "auto" => Some(PreviewMode::Auto),
            _ => None,
        }
    }
}

/// `auto` hides the preview below this terminal width.
const PREVIEW_MIN_WIDTH: u16 = 80;

/// `--files` output format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputFormat {
    /// One path per line.
    Path,
    /// `YYYY-MM-DD HH:MM:SS<TAB>basename<TAB>path` — three fields so fzf
    /// can display a time column and/or the bare file name while the
    /// full path stays available for previews:
    /// `--with-nth 1..2` (time+name), `--with-nth 2` (name only),
    /// `--preview 'bat {3}'` (full path).
    Tsv,
}

/// `--since` cutoff (`--files` only): entries modified at/after this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Since {
    /// Start of today.
    Today,
    /// Start of yesterday.
    Yesterday,
    /// N days (or weeks via `Nw`) before now.
    Days(i64),
}

impl Since {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "today" => Some(Since::Today),
            "yesterday" => Some(Since::Yesterday),
            _ => {
                // The unit is the last CHARACTER, not the last byte —
                // `--since 5日` must parse-fail, not panic mid-char.
                let mut chars = s.chars();
                let unit = chars.next_back()?;
                let n: i64 = chars.as_str().parse().ok().filter(|&n| n >= 0)?;
                match unit {
                    'd' => Some(Since::Days(n)),
                    'w' => Some(Since::Days(n * 7)),
                    _ => None,
                }
            }
        }
    }

    /// The cutoff as a `SystemTime`, relative to `now` (local).
    fn cutoff(&self, now: chrono::DateTime<chrono::Local>) -> std::time::SystemTime {
        match self {
            Since::Today => {
                let midnight = now
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight exists");
                midnight_cutoff(midnight, &mut |m| chrono::Local.from_local_datetime(&m))
            }
            Since::Yesterday => {
                let midnight = (now.date_naive() - chrono::Duration::days(1))
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight exists");
                midnight_cutoff(midnight, &mut |m| chrono::Local.from_local_datetime(&m))
            }
            Since::Days(n) => {
                let dt = now - chrono::Duration::days(*n);
                dt.into()
            }
        }
    }
}

/// The timestamp of a local midnight, falling back to the closest real
/// one. `resolve` maps a naive local time to its instants: a midnight
/// the clock skips entirely (a whole-day DST jump — Pacific/Apia
/// skipped 2011-12-30) yields `None`, and a fall-back midnight yields
/// `Ambiguous` (two instants). Both fall back to the earliest existing
/// start-of-day, so the cutoff can never panic on a broken local day.
fn midnight_cutoff(
    midnight: chrono::NaiveDateTime,
    resolve: &mut impl FnMut(
        chrono::NaiveDateTime,
    ) -> chrono::LocalResult<chrono::DateTime<chrono::Local>>,
) -> std::time::SystemTime {
    let mut day = midnight;
    loop {
        match resolve(day) {
            chrono::LocalResult::Single(dt) => return dt.into(),
            chrono::LocalResult::Ambiguous(a, b) => return a.min(b).into(),
            chrono::LocalResult::None => {
                // The whole day is skipped (DST jumps move the clock at
                // most a day): step back to the previous midnight, which
                // always exists.
                day -= chrono::Duration::days(1);
            }
        }
    }
}

/// The next argument as `flag`'s value; an error when the flag is last
/// (a flag missing its value must not be silently ignored).
fn flag_value<I: Iterator<Item = String>>(it: &mut I, flag: &str) -> Result<String> {
    it.next().with_context(|| format!("{flag} requires a value"))
}

impl Config {
    /// 引数だけで解釈する（**設定ファイルは無いものとする**）。テスト用。
    #[cfg(test)]
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Action> {
        Self::parse_with_file(args, || Ok(None))
    }

    /// 引数と設定ファイルで解釈する。`config_file` は設定ファイルを読む関数で、
    /// **実ファイルを読むのは [`Config::from_env`] だけ**（テストは中身を注入する）。
    ///
    /// `config_file` は**短絡（`--help` / `--version`）とフラグの誤りの後で**
    /// 呼ぶ。壊れた設定ファイルは起動時のエラーだが、それで `--help` まで
    /// 読めなくなると直し方を調べる道が無い。
    fn parse_with_file<I, C>(args: I, config_file: C) -> Result<Action>
    where
        I: IntoIterator<Item = String>,
        C: FnOnce() -> Result<Option<ConfigFile>>,
    {
        let mut dir: Option<PathBuf> = None;
        let mut sort = Sort::MtimeDesc;
        let mut show_hidden = false;
        let mut show_dirs = false;
        let mut open_cmd: Option<String> = None;
        let mut alt_open_cmd: Option<String> = None;
        let mut filter = String::new();
        let mut preview = PreviewMode::Auto;
        let mut files = false;
        let mut format = OutputFormat::Path;
        let mut since: Option<Since> = None;
        let mut output = false;
        let mut theme: Option<String> = None;
        let mut theme_dark: Option<String> = None;
        let mut theme_light: Option<String> = None;
        let mut light: Option<bool> = None;
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(Action::Help),
                "-V" | "--version" => return Ok(Action::Version),
                "--show-hidden" => show_hidden = true,
                "--show-dirs" => show_dirs = true,
                "--output" => output = true,
                "--theme" => theme = Some(flag_value(&mut it, "--theme")?),
                "--theme-dark" => theme_dark = Some(flag_value(&mut it, "--theme-dark")?),
                "--theme-light" => theme_light = Some(flag_value(&mut it, "--theme-light")?),
                "--light" => light = Some(true),
                "--dark" => light = Some(false),
                "--sort" => {
                    let v = flag_value(&mut it, "--sort")?;
                    sort = match Sort::parse(&v) {
                        Some(s) => s,
                        None => bail!("invalid --sort value: {v} (expected mtime|ctime)"),
                    };
                }
                "--open-cmd" => open_cmd = Some(flag_value(&mut it, "--open-cmd")?),
                "--alt-open-cmd" => alt_open_cmd = Some(flag_value(&mut it, "--alt-open-cmd")?),
                "--filter" => filter = flag_value(&mut it, "--filter")?,
                "--preview" => {
                    let v = flag_value(&mut it, "--preview")?;
                    preview = match PreviewMode::parse(&v) {
                        Some(m) => m,
                        None => bail!("invalid --preview value: {v} (expected on|off|auto)"),
                    };
                }
                "--files" => files = true,
                "--format" => {
                    let v = flag_value(&mut it, "--format")?;
                    format = match v.as_str() {
                        "path" => OutputFormat::Path,
                        "tsv" => OutputFormat::Tsv,
                        _ => bail!("invalid --format value: {v} (expected path|tsv)"),
                    };
                }
                "--since" => {
                    let v = flag_value(&mut it, "--since")?;
                    since = match Since::parse(&v) {
                        Some(s) => Some(s),
                        None => bail!(
                            "invalid --since value: {v} (expected today|yesterday|Nd|Nw)"
                        ),
                    };
                }
                other if !other.starts_with('-') && dir.is_none() => {
                    dir = Some(PathBuf::from(other))
                }
                // Typos must fail loudly — a silently ignored flag looks
                // like it worked (`--outpu` would just open the TUI).
                other => bail!("unexpected argument: {other} (see --help)"),
            }
        }
        if output && open_cmd.is_some() {
            bail!("--output and --open-cmd are mutually exclusive");
        }
        if output && alt_open_cmd.is_some() {
            bail!("--output and --alt-open-cmd are mutually exclusive");
        }
        let file = config_file()?;
        // テーマ。`--theme` は両側を上書きする（1 本だったころの意味のまま、
        // `--theme-dark` / `--theme-light` より強い）。各側はフラグ > 設定
        // ファイル > 既定（`None`、[`Highlighter::new`]）。
        let theme = ThemeFlags {
            both: theme,
            dark: theme_dark,
            light: theme_light,
        }
        .over(file.as_ref().map(|f| &f.theme));
        Ok(Action::Run(Config {
            dir,
            sort,
            show_hidden,
            show_dirs,
            open_cmd,
            alt_open_cmd,
            filter,
            preview,
            files,
            format,
            since,
            output,
            theme,
            light,
        }))
    }

    fn from_env() -> Result<Action> {
        Self::parse_with_file(std::env::args().skip(1), ConfigFile::discover)
    }
}

/// Directory resolution priority (spec): explicit arg → herdr → cwd.
fn resolve_root(dir: Option<&Path>) -> Result<PathBuf> {
    let root = match dir {
        Some(p) => p.to_path_buf(),
        None => match herdr::resolve_root() {
            Some(p) => p,
            None => std::env::current_dir()?,
        },
    };
    std::fs::canonicalize(&root).with_context(|| format!("resolving {}", root.display()))
}

fn main() -> Result<()> {
    match Config::from_env()? {
        Action::Help => {
            print_line(
                "ashiato — flat mtime-sorted file picker\n\
                 \n\
                 usage: ashiato [directory] [flags]\n\
                 \n\
                 \x20 directory     project root (default: herdr workspace → cwd)\n\
                 \x20 --sort <mtime|ctime>  initial sort basis (default mtime)\n\
                 \x20 --show-hidden  show dot-directories on startup (dot-files\n\
                 \x20                   are always listed; Ctrl+h/Backspace toggles)\n\
                 \x20 --show-dirs    show directories on startup (d toggles)\n\
                 \x20 --open-cmd <command> Enter spawns this with the selected files\n\
                 \x20                   (none by default — Enter prints the paths to\n\
                 \x20                   stdout and exits; {} = all paths, {1}.. per-file)\n\
                 \x20 --alt-open-cmd <command> the `o` key spawns this (same {} placeholders,\n\
                 \x20                   blocking + rescan flow; without it `o` is a\n\
                 \x20                   dead key)\n\
                 \x20 --filter <text>   initial filter (same matching as `/`,\n\
                 \x20                   e.g. `.md` for markdown only)\n\
                 \x20 --preview <on|off|auto>  preview pane (default auto:\n\
                 \x20                   hidden below 80 columns)\n\
                 \x20 --theme <name>    syntect theme name or path to a\n\
                 \x20                   .tmTheme file, used on both light and\n\
                 \x20                   dark backgrounds (beats --theme-dark /\n\
                 \x20                   --theme-light)\n\
                 \x20 --theme-dark <name>  the theme on a dark background\n\
                 \x20                   (default Catppuccin Mocha)\n\
                 \x20 --theme-light <name> the theme on a light background\n\
                 \x20                   (default Solarized (light)). Which side\n\
                 \x20                   applies follows --light/--dark, else OSC 11\n\
                 \x20                   and the terminal's light/dark switches\n\
                 \x20 --light           force light mode (default: auto-detect\n\
                 \x20                   the terminal background via OSC 11, then\n\
                 \x20                   follow its switches while open — mode 2031)\n\
                 \x20 --dark            force dark mode (forced modes ignore the\n\
                 \x20                   switches)\n\
                 \x20 --files         no TUI: print the time-sorted listing\n\
                 \x20 --format <path|tsv>  --files output (tsv = time column)\n\
                 \x20 --since <today|yesterday|Nd|Nw>  --files cutoff\n\
                 \x20 --output       explicit output mode (same as the default;\n\
                 \x20                   exclusive with --open-cmd)\n\
                 \n\
                 config file: $XDG_CONFIG_HOME/ashiato/config.toml (else\n\
                 \x20 ~/.config/ashiato/config.toml), every key optional:\n\
                 \x20 [theme] dark / light (same values as --theme-dark /\n\
                 \x20 --theme-light). flags > config file > defaults. Unknown\n\
                 \x20 keys, wrong types and broken TOML stop at startup\n\
                 \n\
                 keys:\n\
                 \x20 j/k/arrows     move   g/G  top/bottom\n\
                 \x20 PgUp/PgDn Ctrl+u/Ctrl+d  half page\n\
                 \x20 Space          select/unselect   Enter  open (blocks, then rescan)\n\
                 \x20 o              open with --alt-open-cmd (blocks, then rescan)\n\
                 \x20 y              copy full paths to clipboard\n\
                 \x20 /              incremental filter (Enter apply, Esc clear)\n\
                 \x20 \\              toggle the filter off/on (text is kept)\n\
                 \x20 t              sort cycle: mtime↓ mtime↑ ctime↓ ctime↑\n\
                 \x20 Ctrl+h/Backspace  toggle hidden dirs   d  toggle directories\n\
                 \x20 q              quit   Esc  clear selection
                 \x20 u              toggle: only files with uncommitted changes
                 \x20                   (in a git repo; dirty rows show +N -M)"
            )?;
            Ok(())
        }
        Action::Version => {
            print_line(&format!("ashiato {VERSION}"))?;
            Ok(())
        }
        Action::Run(config) => run(config),
    }
}

/// One row of the visible (filtered, clustered) list.
enum Row {
    /// A cluster separator — never a cursor target (spec).
    Separator(Cluster),
    /// A file row; index into `App::files`.
    File(usize),
}

/// TUI state.
struct App {
    config: Config,
    /// Scan root (canonical).
    root: PathBuf,
    /// All collected entries, in the current sort order.
    files: Vec<FileEntry>,
    /// Visible rows: cluster separators + filtered files.
    visible: Vec<Row>,
    /// Cursor position: index into `visible` (always a `Row::File`).
    cursor: usize,
    /// Scroll offset: index into `visible` of the first drawn row.
    /// The wheel (`scroll_view`) moves it directly; key/mouse cursor
    /// moves sync it via [`App::sync_offset`] so the view scrolls only
    /// when the cursor crosses the window edge.
    offset: usize,
    /// Space-selected entries: indices into `files`.
    selected: BTreeSet<usize>,
    /// Incremental filter text (`/`); empty = no filter.
    filter: String,
    /// Whether the filter applies (`\` toggles; the text is kept while
    /// off, so toggling back restores the same view — vim's
    /// `:nohlsearch` model).
    filter_on: bool,
    /// `--since` cutoff (startup time filter; also applies to the TUI so
    /// `ashiato --since 1d` browses today's files with the preview).
    since: Option<Since>,
    /// True while the filter input line is open.
    filter_active: bool,
    /// Current sort order (`t` cycles).
    sort: Sort,
    show_hidden: bool,
    show_dirs: bool,
    /// 今の light/dark（起動時に決め、配色の知らせで入れ替わる —
    /// [`App::follow_scheme`]）。下の 3 つはここから導いたもの。
    light: bool,
    /// syntect highlighter shared with akapen (two-face themes).
    highlight: Highlighter,
    /// Resolved UI colors for the current light/dark mode (akapen's
    /// `--light` pattern: the same constants). [`App::set_light`] で作り直す。
    ui_selected_bg: Color,
    ui_border: Color,
    /// Preview pane cache: re-rendered only when the key changes.
    preview_cache: Option<(PreviewKey, Preview)>,
    /// Transient footer message (+ error flag → red + BEL).
    status: Option<(String, Instant, bool)>,
    /// `--output` mode: paths to print after the TUI shuts down.
    pending_output: Option<Vec<PathBuf>>,
    /// Set after resuming from a child process: the event loop skips the
    /// event drain and draws immediately (the alternate screen was blank).
    needs_immediate_redraw: bool,
    /// Last time the listing was refreshed for external changes.
    last_refresh: Instant,
    /// Refresh cadence: [`REFRESH_TICK`], backed off after a slow scan so
    /// a huge tree doesn't freeze the UI thread every tick (the scan runs
    /// synchronously in the event loop).
    refresh_every: Duration,
    /// When the last input event arrived (burst tracking, see
    /// [`mark_input`]).
    last_input: Instant,
    /// True while input events arrive faster than [`BURST_GAP`] apart
    /// (the user is mashing j/k): the preview pane shows a placeholder
    /// instead of re-rendering per key (speed first; see
    /// [`App::preview_lines`]).
    bursting: bool,
    /// When a full preview render last finished. A key arriving within
    /// [`BURST_GAP`] of it was pressed while the render was blocking the
    /// loop (a hold, not a deliberate tap) — `mark_input` counts it as a
    /// burst so a slow render cannot re-lock the hold into per-key
    /// re-renders (the stutter cascade).
    last_render_finish: Option<Instant>,
    /// Terminal keyboard focus (xterm focus reporting `CSI ? 1004h`).
    /// Terminals that don't emit focus events never flip this, so the
    /// away-diff feature stays dormant there.
    focused: bool,
    /// Files modified/added while the terminal was unfocused, in
    /// first-seen order — the "diff stack" of the away period. While
    /// away, only the *other* files' times dim (the touched times keep
    /// the normal gray); dropped on focus return.
    away_changes: Vec<PathBuf>,
    /// Git snapshot (git-integration-spec.md §4): `None` outside a
    /// work tree — no markers, `u` is a dead key, and the rows look
    /// exactly as before (P1).
    git: Option<git::GitCache>,
    /// `u` key: show only files with uncommitted changes (spec 4-2).
    uncommitted_only: bool,
    running: bool,
}

impl App {
    /// The `files` index under the cursor, if any.
    fn cursor_file_idx(&self) -> Option<usize> {
        match self.visible.get(self.cursor) {
            Some(Row::File(i)) => Some(*i),
            _ => None,
        }
    }

    fn cursor_file(&self) -> Option<&FileEntry> {
        self.cursor_file_idx().map(|i| &self.files[i])
    }

    /// The selected entries' paths. The selection is index-based, so any
    /// reorder or replacement of `files` must remap it through these.
    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .map(|&i| self.files[i].path.clone())
            .collect()
    }

    /// Re-sort `files` in place for the current sort order, keeping the
    /// selection on the same files (`t` key — without the remap the
    /// selected indices would silently point at whatever files the
    /// reorder happened to put on them).
    fn resort_files(&mut self) {
        let selected = self.selected_paths();
        files::sort_entries(&mut self.files, self.sort);
        self.selected = selected
            .iter()
            .filter_map(|p| self.files.iter().position(|f| &f.path == p))
            .collect();
    }

    /// The file (or files) Enter / y / --output operate on: the Space
    /// selection when non-empty, else the cursor file (spec).
    fn target_paths(&self) -> Vec<PathBuf> {
        if !self.selected.is_empty() {
            self.selected_paths()
        } else {
            self.cursor_file()
                .map(|e| e.path.clone())
                .into_iter()
                .collect()
        }
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now(), false));
    }

    fn flash_err(&mut self, msg: impl Into<String>) {
        use std::io::Write;
        let mut out = std::io::stderr();
        let _ = out.write_all(b"\x07");
        let _ = out.flush();
        self.status = Some((msg.into(), Instant::now(), true));
    }

    /// 端末の配色についての知らせ（モード 2031 の `CSI ? 997 ; n n`、`CSI ? 996 n`
    /// への答え、遅れて届いた OSC 11 の答え）を受けたとき。`--light` / `--dark`
    /// で固定されていれば無視する（固定は固定）。light/dark が変わったら
    /// [`App::set_light`] で作り直して `true`。
    fn follow_scheme(&mut self, light: bool) -> bool {
        if self.config.light.is_some() || light == self.light {
            return false;
        }
        self.set_light(light);
        true
    }

    /// light/dark を入れ替え、`light` から導いているものを全部作り直す: 構文の
    /// テーマ（[`ThemePair`] のその側。`--theme` で両側が同じでも、既定の文字色は
    /// 背景で変わる）、UI の色（選択の背景・枠）、描画済みのプレビュー
    /// （キャッシュのキーに light は入っていない）。画面には次の描画で出る。
    fn set_light(&mut self, light: bool) {
        self.light = light;
        self.highlight = Highlighter::new(self.config.theme.for_background(light), light);
        self.ui_selected_bg = theme::selected_bg(light);
        self.ui_border = theme::border_color(light);
        self.preview_cache = None;
    }

    /// Focus lost: start a fresh away-diff stack — only the changes
    /// made while unfocused are reported on return.
    fn focus_lost(&mut self) {
        self.focused = false;
        self.away_changes.clear();
    }

    /// Focus regained: drop the away stack and revert instantly to the
    /// normal listing (the mtime sort already floats the touched files
    /// to the top; no flash — the away display is deliberately quiet).
    fn focus_gained(&mut self) {
        self.focused = true;
        self.away_changes.clear();
    }

    /// Rebuild `visible` from `files` (filter + clusters), then place the
    /// cursor on `cursor_path` if it is still listed, else on the first
    /// file. `cursor_path` = `None` keeps the current cursor file.
    fn rebuild_visible(&mut self, cursor_path: Option<&Path>) {
        let want = match cursor_path {
            Some(p) => Some(p.to_path_buf()),
            None => self.cursor_file().map(|e| e.path.clone()),
        };
        let now = chrono::Local::now();
        let cutoff = self.since.map(|s| s.cutoff(now));
        let needle = files::prepare_filter(&self.filter);
        let mut visible: Vec<Row> = Vec::new();
        let mut last_cluster: Option<Cluster> = None;
        for (i, e) in self.files.iter().enumerate() {
            if self.filter_on && !files::matches_prepared(e, &needle) {
                continue;
            }
            if cutoff.is_some_and(|c| e.mtime < c) {
                continue;
            }
            // `u` filter: only files with uncommitted changes (spec
            // 4-2). Composes with the text filter and the --since
            // cutoff above — three independent axes.
            if self.uncommitted_only && !self.is_uncommitted(e) {
                continue;
            }
            let c = cluster_of(to_local(e.mtime), now);
            if last_cluster != Some(c) {
                last_cluster = Some(c);
                visible.push(Row::Separator(c));
            }
            visible.push(Row::File(i));
        }
        self.visible = visible;
        // Cursor: prefer the preserved file, else the first file row.
        self.cursor = want
            .and_then(|p| {
                self.visible
                    .iter()
                    .position(|r| matches!(r, Row::File(i) if self.files[*i].path == p))
            })
            .unwrap_or_else(|| first_file_row(&self.visible));
    }

    /// Whether a file has uncommitted changes (git active only).
    fn is_uncommitted(&self, e: &FileEntry) -> bool {
        self.git
            .as_ref()
            .is_some_and(|g| g.marker_for(&e.path).is_some())
    }

    /// Push a fresh listing through the git cache. The cache re-queries
    /// git only when the listing changed since its last query (spec §5:
    /// the every-2s refresh tick that finds the tree unchanged must not
    /// spawn `git status` on every tick).
    fn refresh_git(&mut self, entries: &[FileEntry]) {
        if let Some(g) = &mut self.git {
            g.refresh_if_listing_changed(entries);
        }
    }

    /// Re-scan the root, re-sort, remap the selection by path, and rebuild
    /// the visible list (used after child exits and on toggle changes).
    fn rescan(&mut self) {
        match files::scan(&self.root, self.show_hidden, self.show_dirs) {
            Ok(mut entries) => {
                files::sort_entries(&mut entries, self.sort);
                self.refresh_git(&entries);
                self.commit_scan(entries);
            }
            Err(e) => {
                // Same root-gone rule as refresh_if_changed: a deleted
                // root must not freeze the stale listing.
                let root_gone = std::fs::metadata(&self.root)
                    .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound);
                if root_gone && !self.files.is_empty() {
                    self.commit_scan(Vec::new());
                }
                self.flash_err(format!("rescan failed: {e:#}"));
            }
        }
    }

    /// Periodic silent refresh: re-scan and commit only when the listing
    /// actually changed (an agent editing files while ashiato is open
    /// floats the touched files up on its own — the preview cache and
    /// cursor/selection survive when nothing changed). A scan error is
    /// normally transient and therefore silent — except when the root
    /// itself is gone: the stale listing must not linger, so it is
    /// replaced with an empty one and the user is told once.
    fn refresh_if_changed(&mut self) {
        let started = Instant::now();
        let mut entries = match files::scan(&self.root, self.show_hidden, self.show_dirs) {
            Ok(entries) => entries,
            Err(_) => {
                // NotFound only: a permission blip must not wipe the
                // listing, but a deleted root has no files left to show.
                let root_gone = std::fs::metadata(&self.root)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                if root_gone && !self.files.is_empty() {
                    self.commit_scan(Vec::new());
                    self.flash_err(format!("root deleted: {}", self.root.display()));
                }
                return;
            }
        };
        files::sort_entries(&mut entries, self.sort);
        // Away-diff: while the terminal is unfocused, external edits
        // accumulate in `away_changes` (deduped, first-seen order) so
        // the listing can dim everything but the touched files, and
        // focus return can flash them.
        if !self.focused {
            for p in files::changed_paths(&self.files, &entries) {
                if !self.away_changes.contains(&p) {
                    self.away_changes.push(p);
                }
            }
        }
        // The scan blocks the event loop: after a slow one (huge tree),
        // back off to a ~10% duty cycle instead of freezing every tick.
        self.refresh_every = REFRESH_TICK.max(started.elapsed() * 10);
        if entries != self.files {
            self.refresh_git(&entries);
            self.commit_scan(entries);
        }
    }

    /// Adopt a fresh scan (already in the current sort order): remap the
    /// selection and cursor by path, rebuild the visible list, and drop
    /// the preview cache.
    fn commit_scan(&mut self, entries: Vec<FileEntry>) {
        let selected_paths = self.selected_paths();
        let cursor_path = self.cursor_file().map(|e| e.path.clone());
        self.files = entries;
        self.selected = selected_paths
            .iter()
            .filter_map(|p| self.files.iter().position(|f| &f.path == p))
            .collect();
        self.preview_cache = None;
        self.rebuild_visible(cursor_path.as_deref());
    }

    /// Whether the preview pane is shown this frame.
    fn preview_visible(&self) -> bool {
        match self.config.preview {
            PreviewMode::On => true,
            PreviewMode::Off => false,
            PreviewMode::Auto => {
                let w = ratatui::crossterm::terminal::size()
                    .map(|(w, _)| w)
                    .unwrap_or(PREVIEW_MIN_WIDTH);
                w >= PREVIEW_MIN_WIDTH
            }
        }
    }

    /// The preview pane's lines for the cursor file. Speed-first render
    /// policy: a cached preview is drawn as-is (so j/k alternation
    /// between already-seen files never stalls); a fresh render happens
    /// only when input is deliberate (not bursting); while the user is
    /// mashing j/k the pane shows the header + "…" placeholder, which
    /// costs nothing — each cursor move is a list-only draw. The real
    /// preview catches up the moment the input pauses (see
    /// [`event_loop`]).
    fn preview_lines(&mut self, width: usize, height: usize) -> Vec<Line<'static>> {
        let header_style = Style::default()
            .fg(self.ui_border)
            .add_modifier(Modifier::BOLD);
        let Some(e) = self.cursor_file() else {
            return vec![Line::from(Span::styled(
                "(no selection)",
                Style::default().fg(Color::DarkGray),
            ))];
        };
        let key = PreviewKey {
            path: e.path.clone(),
            mtime: e.mtime,
            size: e.size,
            width,
            height,
        };
        let header = Line::from(Span::styled(
            preview::header_for(&e.path, e.is_dir),
            header_style,
        ));
        if let Some((_, p)) = self.preview_cache.as_ref().filter(|(k, _)| *k == key) {
            let mut lines = vec![header];
            lines.extend(p.rows.iter().cloned());
            return lines;
        }
        if self.bursting {
            // Input burst in progress: defer the render (speed first).
            return vec![
                header,
                Line::from(Span::styled(
                    "…",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
        }
        let p = preview::render(&e.path, e.size, e.is_dir, width, height, &self.highlight);
        self.preview_cache = Some((key, p));
        self.last_render_finish = Some(Instant::now());
        let p = &self.preview_cache.as_ref().expect("just filled").1;
        let mut lines = vec![header];
        lines.extend(p.rows.iter().cloned());
        lines
    }

    /// Cursor one file row down (j / Down), skipping cluster
    /// separators. The view follows via [`App::sync_offset`]: it
    /// scrolls only when the cursor crosses the window's bottom edge.
    fn move_down(&mut self, list_h: usize) {
        let mut row = self.cursor;
        while row + 1 < self.visible.len() {
            row += 1;
            if matches!(self.visible[row], Row::File(_)) {
                break;
            }
        }
        self.cursor = row;
        self.sync_offset(list_h);
    }

    /// Cursor one file row up (k / Up), skipping separators.
    fn move_up(&mut self, list_h: usize) {
        let start = self.cursor;
        let mut row = start;
        while row > 0 {
            row -= 1;
            if matches!(self.visible[row], Row::File(_)) {
                break;
            }
        }
        // The loop stops early only on a file, so running off the top can
        // land on the first cluster's separator — never a cursor target.
        // Keep the old cursor then (the invariant: cursor sits on a file).
        if !matches!(self.visible.get(row), Some(Row::File(_))) {
            row = start;
        }
        self.cursor = row;
        self.sync_offset(list_h);
    }

    /// Align the scroll offset with the cursor after a keyboard/mouse
    /// move. The wheel adjusts `offset` itself, but key moves never
    /// did — `effective_offset` re-derived the view from a stale
    /// `offset` on every move, so at the window's bottom the next Up
    /// keystroke re-scrolled the view under the cursor instead of
    /// moving it (the cursor stayed glued to the bottom row; same at
    /// the top after a wheel scroll). Syncing makes `offset` the
    /// fixed point of `effective_offset`: the view scrolls only when
    /// the cursor actually crosses the window edge (vim-style).
    fn sync_offset(&mut self, list_h: usize) {
        self.offset = self.effective_offset(list_h);
    }

    /// Number of file rows currently listed.
    fn visible_count(&self) -> usize {
        self.visible
            .iter()
            .filter(|r| matches!(r, Row::File(_)))
            .count()
    }

    /// The scroll offset corrected so the cursor row stays in view
    /// (clamped to the scrollable range; used by draw and the mouse).
    fn effective_offset(&self, list_h: usize) -> usize {
        let list_h = list_h.max(1);
        let max = self.visible.len().saturating_sub(list_h);
        let mut off = self.offset.min(max);
        if self.cursor < off {
            off = self.cursor;
        } else if self.cursor >= off + list_h {
            off = self.cursor + 1 - list_h;
        }
        off.min(max)
    }
}

/// The first file row index in `visible` (0 when there are none).
fn first_file_row(visible: &[Row]) -> usize {
    visible
        .iter()
        .position(|r| matches!(r, Row::File(_)))
        .unwrap_or(0)
}

/// Snap a row position to the nearest file row (separators are never
/// cursor targets — spec).
fn clamp_to_file(visible: &[Row], mut row: usize) -> usize {
    if visible.is_empty() {
        return 0;
    }
    row = row.min(visible.len() - 1);
    for i in row..visible.len() {
        if matches!(visible[i], Row::File(_)) {
            return i;
        }
    }
    for i in (0..=row).rev() {
        if matches!(visible[i], Row::File(_)) {
            return i;
        }
    }
    0
}

/// Print one line to stdout, treating a closed pipe as a silent stop
/// (the consumer quit early — e.g. `ashiato --files | fzf` when fzf
/// exits first). Rust's `println!` panics on EPIPE, which would dump a
/// stack trace into the user's terminal.
fn print_line(line: &str) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    match writeln!(out, "{line}") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `--files` mode: scan, filter (--filter / --since), print, exit.
/// The listing is the same one the TUI shows — herdr resolution,
/// gitignore, always-on ignores, hidden/dirs toggles, sort — but as
/// plain lines for `ashiato --files | fzf` and friends.
fn run_files(config: &Config) -> Result<()> {
    let root = resolve_root(config.dir.as_deref())?;
    let mut entries = files::scan(&root, config.show_hidden, config.show_dirs)?;
    files::sort_entries(&mut entries, config.sort);
    let now = chrono::Local::now();
    for e in filter_entries(entries, &config.filter, config.since, now) {
        let line = match config.format {
            OutputFormat::Path => e.path.display().to_string(),
            OutputFormat::Tsv => {
                let t = to_local(e.mtime);
                let name = e
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.display().to_string());
                format!(
                    "{}\t{name}\t{}",
                    t.format("%Y-%m-%d %H:%M:%S"),
                    e.path.display()
                )
            }
        };
        print_line(&line)?;
    }
    Ok(())
}

/// Apply the `--filter` text and the `--since` cutoff to a scanned list
/// (pure, so the `--files` behavior is unit-testable).
fn filter_entries(
    entries: Vec<FileEntry>,
    filter: &str,
    since: Option<Since>,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<FileEntry> {
    let needle = files::prepare_filter(filter);
    entries
        .into_iter()
        .filter(|e| {
            files::matches_prepared(e, &needle)
                && since.is_none_or(|s| e.mtime >= s.cutoff(now))
        })
        .collect()
}

/// The number of list rows that fit: terminal height minus the outer
/// border (2) and the footer (2).
fn list_height(terminal_h: u16) -> usize {
    terminal_h.saturating_sub(4).max(1) as usize
}

/// The pane rectangles for the full terminal `area`:
/// `(list, preview, footer)`. Shared by draw and the mouse handler so
/// clicks map to exactly the geometry that was drawn.
fn pane_layout(area: Rect, preview_on: bool) -> (Rect, Rect, Rect) {
    // The outer block's border eats one cell on every side.
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    // The content area excludes the footer's two rows, so neither pane
    // can bleed into it.
    let content = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(inner);
    let (list, preview) = if preview_on {
        let cols = Layout::horizontal([
            Constraint::Percentage(55),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(content[0]);
        (cols[0], cols[2])
    } else {
        (content[0], Rect::default())
    };
    (list, preview, content[1])
}

/// The TUI terminal: crossterm over a boxed writer (stdout, or /dev/tty
/// when stdout is piped).
type Term = Terminal<CrosstermBackend<Box<dyn std::io::Write>>>;

/// Build the terminal. When stdout is piped, the TUI renders on the
/// controlling terminal (/dev/tty) so the pipe carries only the selected
/// paths (`ashiato . | xargs akapen` — the generic picker use).
/// Without a tty available the stdout writer is kept as a fallback.
fn make_terminal() -> Result<Term> {
    use std::io::IsTerminal;
    let writer: Box<dyn std::io::Write> = if !std::io::stdout().is_terminal() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map(|f| Box::new(f) as Box<dyn std::io::Write>)
            .unwrap_or_else(|_| Box::new(std::io::stdout()))
    } else {
        Box::new(std::io::stdout())
    };
    Ok(Terminal::new(CrosstermBackend::new(writer))?)
}

/// Enter the TUI terminal modes: alternate screen, hidden cursor, mouse
/// capture — and focus reporting. Used on startup and again when the TUI
/// comes back after a child process (akapen, vim, …) exits. The child is
/// usually a crossterm app too, and its teardown sends
/// `DisableFocusChange`; without the re-send, the away-diff feature
/// (external edits while the terminal is unfocused) would stay dead for
/// the rest of the session.
///
/// 配色の知らせ（モード 2031）の購読はここではなく [`TermGuard::scheme`]
/// が張り外しする（固定なら張らない）。
fn enter_tui_modes(w: &mut impl std::io::Write) -> std::io::Result<()> {
    use ratatui::crossterm::QueueableCommand;
    w.queue(EnterAlternateScreen)?;
    w.queue(Hide)?;
    w.queue(EnableMouseCapture)?;
    w.queue(EnableFocusChange)?;
    w.flush()
}

/// 端末を手放すときに戻すもの — [`enter_tui_modes`] の逆。終わるとき
/// （[`TermGuard`] の Drop。panic の巻き戻しも）・シグナル
/// （[`install_signal_handlers`] が前もって列にしておく）・Ctrl+Z
/// （[`suspend_tui`]）で使う。配色の知らせの購読は、この前に
/// [`TermGuard::scheme`] が外す。
fn leave_tui_modes(w: &mut impl std::io::Write) -> std::io::Result<()> {
    ratatui::crossterm::queue!(
        w,
        Show,
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableFocusChange
    )?;
    w.flush()
}

/// 子プロセス（`--open-cmd` の akapen など）に端末を渡す前に戻すもの。
/// フォーカスの報告は今までどおり残す（子の多くは自分で張り、出るときに外す）。
/// 配色の知らせの購読は、この前に [`TermGuard::scheme`] が必ず外す — 子の
/// akapen は自分で購読し、crossterm で読む子は届いた知らせで止まる。
fn leave_for_child(w: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(w, LeaveAlternateScreen, Show, DisableMouseCapture)
}

/// When stdin is not a terminal (xargs gives children /dev/null; scripts
/// redirect it), rebind fd 0 to a real tty so the input reader can read
/// it. On macOS this must be the actual pty slave: /dev/tty is a
/// synthetic node that kqueue (mio) rejects with EINVAL, and crossterm's
/// own /dev/tty fallback therefore failed with "Failed to initialize
/// input reader" (`ashiato --files | xargs akapen` died). 今は入力を
/// termtheme の読み手で読む（macOS では select で待つので /dev/tty も読める）
/// が、起動時の背景色の判定（OSC 11）は stdin が端末のときだけ問い合わせ、
/// 答えも stdin から読むので、張り替えは今も要る。
fn ensure_terminal_stdin() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return;
    }
    #[cfg(target_os = "macos")]
    if rebind_to_controlling_pty() {
        return;
    }
    // Other platforms (and macOS without a matching pty): the controlling
    // terminal node itself — poll(2) accepts it there.
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        use std::os::fd::AsRawFd;
        // SAFETY: both fds are valid; dup2 replaces fd 0 with the tty.
        unsafe {
            libc::dup2(tty.as_raw_fd(), libc::STDIN_FILENO);
        }
    }
}

/// macOS: find the real pty slave whose foreground process group is ours
/// (the controlling terminal's `tcgetpgrp` == our `getpgrp`) and rebind
/// stdin to it. Returns whether a match was found and dup2'ed.
#[cfg(target_os = "macos")]
fn rebind_to_controlling_pty() -> bool {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let me = unsafe { libc::getpgrp() };
    let Ok(rd) = std::fs::read_dir("/dev") else { return false };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("ttys") {
            continue;
        }
        let path = format!("/dev/{name}");
        let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
        else {
            continue;
        };
        if unsafe { libc::tcgetpgrp(f.as_raw_fd()) } == me {
            // SAFETY: valid fds; rebind stdin to the real tty.
            unsafe {
                libc::dup2(f.as_raw_fd(), libc::STDIN_FILENO);
            }
            return true;
        }
    }
    false
}

/// True while the TUI owns the terminal (raw mode on). The
/// SIGINT/SIGTERM handler restores the terminal only then: while a
/// child process owns it (open_selection's raw-off window) a signal
/// must leave the child's terminal alone.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard for the TUI's terminal state: raw mode and, once
/// entered, the alternate screen. Drop restores exactly what is on,
/// so errors, panics, and early returns can't leave the user's shell
/// in raw mode on a phantom screen. The state flags keep the
/// child-process window (open_selection) from being restored twice:
/// while the child owns the terminal both are off and Drop is a no-op.
struct TermGuard {
    /// The TUI terminal; `None` until `make_terminal` succeeds.
    term: Option<Term>,
    /// Raw mode is currently enabled (mirrored by [`TUI_ACTIVE`]).
    raw: bool,
    /// The alternate screen is currently entered.
    alt: bool,
    /// 配色の知らせ（モード 2031）の購読。購読は端末の状態で、プロセスが
    /// 終わっても残るので、端末を手放すとき（終わるとき・panic・シグナル・
    /// 子プロセスに渡すとき・Ctrl+Z）は必ず外す — 外し忘れると、子の akapen や
    /// 後のシェルに知らせが届き、crossterm で読むものはそこで止まる。張り外しは
    /// termtheme の [`Subscription`] が受け持つ（子に渡しているあいだは、落ちても
    /// 書かない）。書き先は [`make_terminal`] と同じ選び方（`Subscription::new`）。
    /// `--light` / `--dark` で固定したときは `Subscription::fixed` で、何も書かない。
    scheme: Subscription,
}

impl TermGuard {
    /// Raw mode is on: the signal handler must restore the terminal.
    fn raw_on(&mut self) {
        self.raw = true;
        TUI_ACTIVE.store(true, Ordering::SeqCst);
    }

    /// Raw mode is off (a child or the shell owns the terminal): the
    /// signal handler must leave it alone.
    fn raw_off(&mut self) {
        self.raw = false;
        TUI_ACTIVE.store(false, Ordering::SeqCst);
    }

    /// 起動時: TUI のモードに入り、配色の知らせの購読を始める（起動時の判定
    /// — OSC 11 — の後で呼ぶ。問い合わせはしない）。
    fn enter(&mut self) {
        let _ = enter_tui_modes(self.backend_mut());
        self.alt = true;
        let _ = self.scheme.start();
    }

    /// 端末を手放す — 子プロセスに渡す前（`leave` は [`leave_for_child`]）と
    /// Ctrl+Z で止まる前（[`leave_tui_modes`]）。購読を先に外し、戻るまでは
    /// 落ちても書かない（端末は子やシェルのもの）。
    fn hand_off(
        &mut self,
        leave: fn(&mut CrosstermBackend<Box<dyn std::io::Write>>) -> std::io::Result<()>,
    ) {
        let _ = self.scheme.suspend();
        let _ = leave(self.backend_mut());
        self.alt = false;
    }

    /// 端末が戻った（作り直した [`Term`] を入れた後）: TUI のモードに入り直し、
    /// 購読を張り直して、離れている間に変わったかもしれない今の配色を問い合わせる
    /// （`CSI ? 996 n`。答えは知らせと同じ形で届き、[`App::follow_scheme`] が拾う）。
    fn take_back(&mut self) {
        let _ = enter_tui_modes(self.backend_mut());
        self.alt = true;
        let _ = self.scheme.resume();
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // The signal handler must not restore after we have (a signal
        // landing mid-restore would double-leave the alternate screen).
        TUI_ACTIVE.store(false, Ordering::SeqCst);
        // 購読を先に外す（子に渡しているあいだは何も書かない）。フィールドの
        // Drop はこの本体の後なので、順を決めるためにここで stop する。
        let _ = self.scheme.stop();
        if self.alt && let Some(t) = &mut self.term {
            let _ = leave_tui_modes(t.backend_mut());
        }
        if self.raw {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }
}

impl std::ops::Deref for TermGuard {
    type Target = Term;

    fn deref(&self) -> &Term {
        self.term
            .as_ref()
            .expect("TermGuard: the TUI terminal is not armed")
    }
}

impl std::ops::DerefMut for TermGuard {
    fn deref_mut(&mut self) -> &mut Term {
        self.term
            .as_mut()
            .expect("TermGuard: the TUI terminal is not armed")
    }
}

/// The restore sequences the signal handler writes (built once by
/// [`leave_tui_modes`], the normal teardown itself, so they can't drift;
/// the handler itself must not allocate or open anything).
static TERMINAL_RESTORE: OnceLock<Vec<u8>> = OnceLock::new();

/// SIGINT/SIGTERM handler: drop the color-scheme subscription, leave the
/// alternate screen (and show the cursor, stop mouse/focus reporting),
/// then re-raise with the default
/// disposition so the process dies with the conventional signal status.
/// The handler writes only to already-open fds: on
/// macOS, opening /dev/tty or calling tcsetattr from a handler races
/// with process exit (with crossterm's kqueue in flight) and can hang
/// the dying process in the kernel. Raw mode is left to the shell,
/// which restores the termios when the job dies.
extern "C" fn restore_terminal_and_die(sig: libc::c_int) {
    // 配色の知らせの購読を外す。termtheme が、張っているときだけ、
    // Subscription の書き先（開いてある fd）へ write(2) する — 固定・子に
    // 渡しているあいだ・外した後は何もしない（async-signal-safe）。
    termtheme::scheme::unsubscribe_in_signal_handler();
    if TUI_ACTIVE.load(Ordering::SeqCst) {
        // Both fds are the terminal in the normal case; when stdout is
        // piped (--output), stderr still is.
        if let Some(bytes) = TERMINAL_RESTORE.get() {
            for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                unsafe {
                    libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                }
            }
        }
    }
    unsafe {
        // SAFETY: re-raising with the default disposition kills us with
        // the status the shell expects (128 + signum).
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Install the terminal-restoring signal handlers. In raw mode the tty
/// never delivers SIGINT for Ctrl+C (that arrives as a key event), so
/// these cover SIGTERM and signals sent from outside.
fn install_signal_handlers() {
    // Build the restore bytes eagerly: the handler must not allocate.
    let mut restore = Vec::new();
    let _ = leave_tui_modes(&mut restore);
    let _ = TERMINAL_RESTORE.set(restore);
    unsafe {
        // SAFETY: `restore_terminal_and_die` is a valid handler, and
        // the default disposition is restored before it re-raises.
        libc::signal(libc::SIGINT, restore_terminal_and_die as libc::sighandler_t);
        libc::signal(libc::SIGTERM, restore_terminal_and_die as libc::sighandler_t);
    }
}

fn run(config: Config) -> Result<()> {
    // `--files`: the time-ordered listing as a data source (pipes, fzf).
    // No TUI → no tty juggling either (ensure_terminal_stdin scans
    // /dev/ttys* on macOS; pointless without an event loop).
    if config.files {
        return run_files(&config);
    }
    ensure_terminal_stdin();
    // SIGTERM and external SIGINT must not leave the terminal broken:
    // the handler restores it, then the default disposition re-raises.
    install_signal_handlers();
    let root = resolve_root(config.dir.as_deref())?;
    // Light/dark resolution: --light/--dark win, else the terminal's
    // background is queried (OSC 11). The query needs raw mode (the
    // answer is plain bytes on stdin), so raw mode is enabled before
    // the App is built; unanswerable terminals fall back to dark.
    ratatui::crossterm::terminal::enable_raw_mode()?;
    // The guard owns the terminal state from here on: Drop restores
    // raw mode and the alternate screen on every exit path (errors,
    // panics, early returns). open_selection disarms it while a child
    // owns the terminal, so nothing restores twice.
    let mut guard = TermGuard {
        term: None,
        raw: false,
        alt: false,
        // 固定（--light / --dark）なら購読しない: 固定は固定。
        scheme: if config.light.is_some() {
            Subscription::fixed()
        } else {
            Subscription::new()
        },
    };
    guard.raw_on();
    // 起動時の判定の待ちのあいだに打たれたキー（先打ち）は、termtheme が捨てずに
    // 入力の読み手（event_loop）へ渡す。
    let light = config
        .light
        .unwrap_or_else(|| termtheme::background::detect_light().unwrap_or(false));
    // The syntax theme: the side matching light/dark (--theme covers both,
    // else --theme-dark / --theme-light, else the config file's [theme]);
    // a missing or unresolvable name falls back to that side's default
    // (Highlighter::new). 開いている間に配色が変われば App::set_light が
    // 作り直す。
    let highlight = Highlighter::new(config.theme.for_background(light), light);
    let mut app = App {
        config,
        root: root.clone(),
        files: Vec::new(),
        visible: Vec::new(),
        cursor: 0,
        offset: 0,
        selected: BTreeSet::new(),
        filter: String::new(),
        filter_on: true,
        since: None,
        filter_active: false,
        sort: Sort::MtimeDesc,
        show_hidden: false,
        show_dirs: false,
        light,
        highlight,
        ui_selected_bg: theme::selected_bg(light),
        ui_border: theme::border_color(light),
        preview_cache: None,
        status: None,
        pending_output: None,
        needs_immediate_redraw: false,
        last_refresh: Instant::now(),
        refresh_every: REFRESH_TICK,
        last_input: Instant::now(),
        bursting: false,
        last_render_finish: None,
        focused: true,
        away_changes: Vec::new(),
        git: None,
        uncommitted_only: false,
        running: true,
    };
    // The config flags seed the sort/toggles; the `t`/Ctrl+h/`d` keys
    // cycle them at runtime.
    app.sort = app.config.sort;
    app.show_hidden = app.config.show_hidden;
    app.show_dirs = app.config.show_dirs;
    let terminal = make_terminal()?; // the guard restores raw mode on error
    guard.term = Some(terminal);
    guard.enter();
    // The first scan blocks the event loop — on a huge tree it takes
    // seconds, and with the alternate screen already up that reads as a
    // frozen/black screen. Draw a "scanning" status before the scan;
    // event_loop's opening draw renders the result the moment it returns.
    app.flash("scanning…");
    guard.draw(|f| draw(f, &mut app))?;
    // The `--filter`/`--since` seeds apply to the first scan; `/` edits
    // the filter later.
    app.filter = app.config.filter.clone();
    app.since = app.config.since;
    // The git snapshot's first full query (status + numstat) can take
    // a moment on a big repo — it runs under the "scanning…" frame,
    // like the first filesystem scan. Discovery itself is one cheap
    // `rev-parse`; the full query waits for the first listing (rescan
    // → refresh_git), so the signature gate starts clean.
    app.git = git::GitCache::discover(&app.root);
    app.rescan();
    if app.files.is_empty() {
        let failed = app.status.as_ref().is_some_and(|(_, _, err)| *err);
        if !failed {
            app.flash("no files found");
        }
    } else {
        // The scan is done and the listing is live: drop the placeholder.
        app.status = None;
    }
    let res = event_loop(&mut guard, &mut app);
    // The guard's Drop restores the terminal (alternate screen + raw
    // mode) before the output prints below.
    drop(guard);
    // `--output` mode: the paths print only after the TUI is fully down,
    // to the (piped) stdout — the TUI rendered on /dev/tty, so the pipe
    // carries nothing but the paths.
    if let Some(paths) = app.pending_output {
        for p in paths {
            print_line(&p.display().to_string())?;
        }
    }
    res
}

/// Whether this process still has a controlling terminal — i.e. the
/// pane/window this TUI runs in is alive. When the session leader exits
/// (window/pane closed) the kernel releases the controlling terminal and
/// `/dev/tty` stops opening (ENXIO), even though reads of the pty keep
/// blocking (the pty master may stay open — herdr keeps it for
/// scrollback). Polled every tick as a death watchdog.
fn controlling_terminal_alive() -> bool {
    std::fs::OpenOptions::new().read(true).open("/dev/tty").is_ok()
}

/// Whether stdin's writer is gone (POLLHUP): a pipe-based virtual
/// terminal (e.g. a herdr plugin pane) whose owner closed. Reads return
/// EOF forever. termtheme の読み手はそれを `Err` にして event_loop を抜けるが、
/// 子プロセスを待っている間は読まないので、ここで見る。
fn stdin_hung_up() -> bool {
    let mut pfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: 0,
        revents: 0,
    };
    // SAFETY: poll(2) on fd 0, which is open here (the input reader reads it).
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    n > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

fn event_loop(terminal: &mut TermGuard, app: &mut App) -> Result<()> {
    terminal.draw(|f| draw(f, app))?;
    // Death watchdog: exit cleanly when the session dies under us. With
    // a controlling terminal we watch `/dev/tty`; without one from the
    // start (pipe-based virtual terminal) we watch stdin for HUP.
    let watch_terminal = controlling_terminal_alive();
    let watch_stdin = !watch_terminal;
    loop {
        if (watch_terminal && !controlling_terminal_alive())
            || (watch_stdin && stdin_hung_up())
        {
            return Err(anyhow::anyhow!("terminal gone; exiting"));
        }
        // Burst-aware poll: while the user is mashing j/k the preview is
        // deferred (see mark_input / preview_lines), and the poll wakes
        // exactly when the burst ends so the final preview pops ~BURST_GAP
        // after the key is released instead of waiting out the full tick.
        let timeout = poll_timeout(app.bursting, app.last_input.elapsed());
        let mut any_input = false;
        // 入力は termtheme の読み手で読む（crossterm の event::poll / read は
        // 使わない）。crossterm 0.29 は配色の知らせ（`CSI ? 997 ; n n`）を受けると
        // 後ろの入力を飲み込み、poll ごと止まる。
        if input::poll(timeout)? {
            for _ in 0..MAX_EVENTS_PER_FRAME {
                // A child process just exited and the terminal was
                // re-initialized: draw now, don't drain stale events.
                if app.needs_immediate_redraw {
                    app.needs_immediate_redraw = false;
                    break;
                }
                if !input::poll(Duration::ZERO)? {
                    break;
                }
                match input::read()? {
                    Input::Event(Event::Key(key)) => {
                        mark_input(app);
                        any_input = true;
                        // Release/Repeat kinds (kitty protocol) don't
                        // drive the UI, but they still count as input
                        // for the burst tracker.
                        if key.kind == KeyEventKind::Press {
                            on_key(app, key.code, key.modifiers, Some(terminal));
                        }
                    }
                    Input::Event(Event::Mouse(mouse)) => {
                        mark_input(app);
                        any_input = true;
                        on_mouse(app, mouse);
                    }
                    Input::Event(Event::FocusGained) => app.focus_gained(),
                    Input::Event(Event::FocusLost) => app.focus_lost(),
                    Input::Event(_) => {}
                    // 配色の知らせ・背景色の答え: 作り直しは follow_scheme、
                    // 描き直しはこのフレームの終わりの draw。
                    other => {
                        if let Some(light) = other.light() {
                            app.follow_scheme(light);
                        }
                    }
                }
            }
        }
        if !any_input && app.bursting && app.last_input.elapsed() >= BURST_GAP {
            // The burst ended without a trailing event (the key was
            // released): the placeholder gives way to the real preview.
            app.bursting = false;
        }
        terminal.draw(|f| draw(f, app))?;
        // Silent refresh: an external edit (the agent rewriting files)
        // replaces the listing within a tick; cursor/selection follow by
        // path, so the display just "becomes" the new state.
        if app.last_refresh.elapsed() >= app.refresh_every {
            app.last_refresh = Instant::now();
            app.refresh_if_changed();
        }
        if app
            .status
            .as_ref()
            .is_some_and(|(_, at, _)| at.elapsed() > STATUS_SECS)
        {
            app.status = None;
        }
        if !app.running {
            return Ok(());
        }
    }
}

fn on_key(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
    terminal: Option<&mut TermGuard>,
) {
    if app.filter_active {
        return on_filter_key(app, key, modifiers, terminal);
    }
    let height = list_height(
        ratatui::crossterm::terminal::size()
            .map(|(_, h)| h)
            .unwrap_or(24),
    );
    match key {
        // Legacy terminals encode Ctrl+h as BS (0x08) — indistinguishable
        // from the Backspace key there, so plain Backspace is the same
        // hidden-dirs toggle (documented in --help).
        KeyCode::Backspace => toggle_hidden(app),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(height),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(height),
        KeyCode::Char('g') => {
            app.cursor = first_file_row(&app.visible);
            app.sync_offset(height);
        }
        KeyCode::Char('G') => {
            app.cursor = clamp_to_file(&app.visible, app.visible.len().saturating_sub(1));
            app.sync_offset(height);
        }
        // NOTE: a match guard applies to ALL or-patterns of an arm, so
        // PageDown/PageUp get their own guard-free arms — sharing the
        // Ctrl+d/Ctrl+u arm would make plain PgDn/PgUp dead keys.
        KeyCode::PageDown => half_page_down(app, height),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            half_page_down(app, height)
        }
        KeyCode::PageUp => half_page_up(app, height),
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            half_page_up(app, height)
        }
        KeyCode::Char(' ') => {
            if let Some(i) = app.cursor_file_idx() {
                if !app.selected.remove(&i) {
                    app.selected.insert(i);
                }
            }
        }
        KeyCode::Enter => open_selection(app, terminal),
        KeyCode::Char('o') => open_selection_alt(app, terminal),
        KeyCode::Char('y') => copy_paths(app),
        KeyCode::Char('/') => app.filter_active = true,
        KeyCode::Char('t') => {
            app.sort = app.sort.next();
            app.resort_files();
            app.preview_cache = None;
            app.rebuild_visible(None);
        }
        KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => toggle_hidden(app),
        KeyCode::Char('d') => {
            app.show_dirs = !app.show_dirs;
            app.flash(if app.show_dirs {
                "directories shown"
            } else {
                "directories hidden"
            });
            app.rescan();
        }
        // `u`: "only files with uncommitted changes" (git repos, spec
        // 4-2). Outside a repo it is a dead key — the pre-git behavior
        // (4-3: `u` 無効).
        KeyCode::Char('u') => {
            if app.git.is_some() {
                app.uncommitted_only = !app.uncommitted_only;
                app.flash(if app.uncommitted_only {
                    "uncommitted only"
                } else {
                    "showing all"
                });
                app.rebuild_visible(None);
            }
        }
        KeyCode::Char('q') => app.running = false,
        // Raw mode eats the tty's SIGINT, so Ctrl+C arrives as a key
        // here, not as a signal; quit like `q` (a dead ^C would trap
        // the user in the TUI). Ctrl+Z is likewise swallowed: suspend
        // explicitly (the terminal is restored and SIGTSTP raised).
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.running = false,
        KeyCode::Char('z') if modifiers.contains(KeyModifiers::CONTROL) => {
            suspend_tui(app, terminal)
        }
        // Filter on/off toggle: the text is preserved, so `\` again
        // restores the same view (`--filter` is the context's default
        // view; this is the quick way to step out of it and back).
        KeyCode::Char('\\') => {
            app.filter_on = !app.filter_on;
            app.flash(if app.filter_on {
                "filter on"
            } else {
                "filter off — showing all"
            });
            app.rebuild_visible(None);
        }
        KeyCode::Esc => {
            app.selected.clear();
        }
        _ => {}
    }
}

/// Record an input event for the burst tracker. Events arriving closer
/// than [`BURST_GAP`] to the previous one put the UI into "mashing" mode
/// (held j/k repeat, fast wheel scrolls): the preview render is deferred
/// until the input pauses, so every cursor move costs only the list draw.
/// A deliberate press after a pause renders the preview immediately.
fn mark_input(app: &mut App) {
    let now = Instant::now();
    app.bursting = now.duration_since(app.last_input) < BURST_GAP
        || app
            .last_render_finish
            .is_some_and(|t| now.duration_since(t) < BURST_GAP);
    app.last_input = now;
}

/// バースト中の `input::poll` タイムアウト。バースト終了予定（前回入力から
/// [`BURST_GAP`]）ちょうどに目覚めるよう残り時間を返すが、重いフレーム
/// （プレビュー描画や再スキャン）の直後は `quiet` が既に BURST_GAP を
/// 超えていることがあるため、飽和減算でパニックを避け下限 5ms に丸める。
fn poll_timeout(bursting: bool, quiet: Duration) -> Duration {
    if bursting {
        Duration::from_millis(TICK_MS)
            .min(BURST_GAP.saturating_sub(quiet).max(Duration::from_millis(5)))
    } else {
        Duration::from_millis(TICK_MS)
    }
}

/// Ctrl+h / Backspace: toggle dot-directory visibility. Dot-FILES
/// (`.gitignore` etc.) are always listed; only dot-directories
/// (`.claude/`, `.github/`, …) are hidden by default (spec layout).
fn toggle_hidden(app: &mut App) {
    app.show_hidden = !app.show_hidden;
    app.flash(if app.show_hidden {
        "hidden dirs shown"
    } else {
        "hidden dirs hidden"
    });
    app.rescan();
}

/// Half-page cursor movement (PgDn / Ctrl+d, PgUp / Ctrl+u). The
/// view follows via [`App::sync_offset`] like the other key moves.
fn half_page_down(app: &mut App, height: usize) {
    app.cursor = clamp_to_file(&app.visible, app.cursor + (height / 2).max(1));
    app.sync_offset(height);
}

fn half_page_up(app: &mut App, height: usize) {
    app.cursor = clamp_to_file(&app.visible, app.cursor.saturating_sub((height / 2).max(1)));
    app.sync_offset(height);
}

/// The filter input line: every key goes to the buffer (incremental — the
/// list re-filters on each keystroke).
fn on_filter_key(
    app: &mut App,
    key: KeyCode,
    modifiers: KeyModifiers,
    terminal: Option<&mut TermGuard>,
) {
    match key {
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            // Typing re-applies the filter (incremental).
            app.filter_on = true;
            app.filter.push(c);
            app.rebuild_visible(None);
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.rebuild_visible(None);
        }
        // Esc cancels the filter entirely (spec: フィルタ解除).
        KeyCode::Esc => {
            app.filter.clear();
            app.filter_on = false;
            app.filter_active = false;
            app.rebuild_visible(None);
        }
        // Enter applies and closes the input (empty = no filter).
        KeyCode::Enter => {
            app.filter_on = true;
            app.filter_active = false;
            app.rebuild_visible(None);
        }
        // Ctrl+C quits even while the filter input is active (the
        // universal interrupt habit must not be a dead key here
        // either); Ctrl+Z suspends like everywhere else.
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.running = false,
        KeyCode::Char('z') if modifiers.contains(KeyModifiers::CONTROL) => {
            suspend_tui(app, terminal)
        }
        _ => {}
    }
}

/// Enter: with `--open-cmd` given, hand the target files to it (blocking),
/// then rescan; without one (or with `--output`) queue the paths for
/// stdout and exit — the generic picker behavior.
fn open_selection(app: &mut App, terminal: Option<&mut TermGuard>) {
    let paths = app.target_paths();
    if paths.is_empty() {
        app.flash_err("no files");
        return;
    }
    if app.config.output || app.config.open_cmd.is_none() {
        app.pending_output = Some(paths);
        app.running = false;
        return;
    }
    let cmd = expand_cmd(app.config.open_cmd.as_deref().expect("checked above"), &paths);
    open_with_cmd(app, terminal, &cmd);
}

/// `o`: with `--alt-open-cmd` given, hand the target files to it in the
/// same blocking + rescan flow as Enter's `--open-cmd`; without one the
/// key is dead — flash an error and keep running.
fn open_selection_alt(app: &mut App, terminal: Option<&mut TermGuard>) {
    let Some(cmd) = app.config.alt_open_cmd.clone() else {
        app.flash_err("no --alt-open-cmd");
        return;
    };
    let paths = app.target_paths();
    if paths.is_empty() {
        app.flash_err("no files");
        return;
    }
    let cmd = expand_cmd(&cmd, &paths);
    open_with_cmd(app, terminal, &cmd);
}

/// Shared child flow for `--open-cmd` / `--alt-open-cmd`: suspend the TUI
/// while the child owns the terminal, block until it exits, then re-enter
/// the TUI and rescan.
fn open_with_cmd(app: &mut App, mut terminal: Option<&mut TermGuard>, cmd: &str) {
    if let Some(g) = terminal.as_deref_mut() {
        // Suspend the TUI while the child owns the terminal (akapen's
        // editor pattern): leave the alternate screen and raw mode, and
        // disarm the guard — the child owns the terminal now, so a
        // panic or signal mid-child must not restore it out from under
        // the child. 配色の知らせの購読も先に外す（hand_off）。
        g.hand_off(leave_for_child);
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
        g.raw_off();
    }
    let mut child = match Command::new("sh").arg("-c").arg(&cmd).spawn() {
        Ok(c) => c,
        Err(e) => {
            app.flash_err(format!("spawn failed: {e}"));
            return;
        }
    };
    // Death watchdog while the child owns the screen: if the session
    // dies (pane/window closed) the child would otherwise leak and spin
    // forever in a dead session. Kill it and bail — the terminal is gone,
    // so restoring it is pointless.
    // TUI を持たないとき（テスト等）は守る対象のセッションが無い。stdin が
    // 書き手のいないパイプだと POLLHUP になり、子を kill して rescan の前に
    // 抜けてしまうので、見張りは TUI を持っているときだけ張る。
    let watching = terminal.is_some();
    let watch_terminal = watching && controlling_terminal_alive();
    let watch_stdin = watching && !watch_terminal;
    let status = loop {
        if (watch_terminal && !controlling_terminal_alive())
            || (watch_stdin && stdin_hung_up())
        {
            let _ = child.kill();
            let _ = child.wait();
            app.flash_err("terminal gone — child killed");
            return;
        }
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => break Err(e),
        }
    };
    if let Some(g) = terminal.as_deref_mut() {
        // Re-enter raw mode and rebuild a fresh terminal for the TUI.
        let _ = ratatui::crossterm::terminal::enable_raw_mode();
        g.raw_on();
        match make_terminal() {
            Ok(term) => {
                **g = term;
                // Re-send focus reporting along with the screen re-entry:
                // the child may have sent DisableFocusChange on its way
                // out, which would kill the away-diff silently. 購読も
                // 張り直し、離れている間に変わった配色を問い合わせる。
                g.take_back();
            }
            Err(e) => {
                app.flash_err(format!("terminal restore failed: {e:#}"));
                app.running = false;
            }
        }
    }
    // The rescan below blocks the event loop — on a huge tree it takes
    // seconds, and the freshly entered alternate screen is still blank:
    // draw a "scanning" status first. (The post-scan frame is drawn via
    // needs_immediate_redraw when the event loop resumes.)
    if app.running {
        app.flash("scanning…");
        if let Some(t) = terminal {
            let _ = t.draw(|f| draw(f, app));
        }
    }
    // The child may have edited files: rescan so fresh mtimes float up
    // (spec flow step 4-5). The child owned the terminal, so we are
    // focused again — a stale focus event must not flip the away-diff on.
    app.rescan();
    app.focused = true;
    app.away_changes.clear();
    match status {
        Ok(s) if s.success() => app.flash("done — list rescanned"),
        Ok(s) => app.flash_err(format!(
            "command exited with {}",
            s.code().map_or_else(|| "signal".to_string(), |c| c.to_string())
        )),
        Err(e) => app.flash_err(format!("spawn failed: {e}")),
    }
    app.needs_immediate_redraw = true;
}

/// Whether our process group owns the controlling terminal: the shell
/// can `fg` us back only then. A background job must not stop itself
/// — no shell would resume it.
fn is_foreground() -> bool {
    unsafe {
        // SAFETY: fd 0 is a valid, open tty here (ensure_terminal_stdin).
        libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp()
    }
}

/// Ctrl+Z: suspend the TUI like a normal foreground job. The terminal
/// is restored first (alternate screen left, raw mode off), then
/// SIGTSTP's default disposition stops us; the shell prints its
/// prompt, and `fg` resumes here with the TUI rebuilt — the same
/// restore → child → rebuild pattern as [`open_selection`].
fn suspend_tui(app: &mut App, terminal: Option<&mut TermGuard>) {
    let Some(g) = terminal else {
        return;
    };
    if !is_foreground() {
        app.flash("not a foreground job — can't suspend");
        return;
    }
    g.hand_off(leave_tui_modes);
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    g.raw_off();
    unsafe {
        // SAFETY: SIGTSTP's default action stops us; the terminal is
        // already restored, so the shell gets a clean prompt.
        libc::raise(libc::SIGTSTP);
    }
    // Resumed (fg): re-enter raw mode and rebuild the TUI. 購読も張り直し、
    // 止まっている間に変わった配色を問い合わせる。
    let _ = ratatui::crossterm::terminal::enable_raw_mode();
    g.raw_on();
    match make_terminal() {
        Ok(term) => {
            **g = term;
            g.take_back();
        }
        Err(e) => {
            app.flash_err(format!("terminal restore failed: {e:#}"));
            app.running = false;
        }
    }
    // The terminal was the shell's while we were stopped: the focus
    // state is stale, like after a child command.
    app.focused = true;
    app.away_changes.clear();
    app.needs_immediate_redraw = true;
}

/// `y`: copy the target files' full paths to the clipboard.
fn copy_paths(app: &mut App) {
    let paths = app.target_paths();
    if paths.is_empty() {
        app.flash_err("no files");
        return;
    }
    let text = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    match clipboard::copy_to_clipboard(&text) {
        Ok(()) => app.flash(format!(
            "{} path{} copied",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        )),
        Err(e) => app.flash_err(format!("clipboard: {e:#}")),
    }
}

/// Expand `--open-cmd` placeholders (spec): `{}` = all paths space-joined,
/// `{1}` `{2}`.. = the Nth path. Without any substituted placeholder the
/// paths are appended. Every path is shell-quoted.
///
/// Single pass over the command text only: substituted paths are never
/// rescanned. (Sequential `str::replace` calls would re-substitute
/// placeholders INSIDE already-inserted paths — a file named `x{2}.md`
/// would corrupt the quoting and open a shell-injection surface.)
fn expand_cmd(cmd: &str, paths: &[PathBuf]) -> String {
    let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
    let mut out = String::with_capacity(cmd.len());
    let mut substituted = false;
    let mut rest = cmd;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let Some(close) = tail.find('}') else {
            // No closing brace anywhere: the remainder is literal.
            rest = tail;
            break;
        };
        let inner = &tail[1..close];
        if inner.is_empty() {
            out.push_str(&quoted.join(" "));
            substituted = true;
        } else if let Some(q) = inner
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| quoted.get(i))
        {
            out.push_str(q);
            substituted = true;
        } else {
            // Not a placeholder ({x}) or out of range ({9} with 2 files):
            // kept literal.
            out.push_str(&tail[..=close]);
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    if substituted {
        out
    } else {
        format!("{out} {}", quoted.join(" "))
    }
}

/// Single-quote a path for `sh -c` (paths with spaces must survive).
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

fn on_mouse(app: &mut App, mouse: MouseEvent) {
    // Only the list pane is mouse-driven: wheel scrolls, a click on a
    // file row moves the cursor.
    let (w, h) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let (list_area, _, _) = pane_layout(Rect::new(0, 0, w, h), app.preview_visible());
    let list_h = list_height(h);
    match mouse.kind {
        MouseEventKind::ScrollDown => scroll_view(app, 1, list_h),
        MouseEventKind::ScrollUp => scroll_view(app, -1, list_h),
        MouseEventKind::Down(MouseButton::Left) => {
            // Clicks outside the list pane (border, preview, footer) are
            // not cursor targets.
            if mouse.column < list_area.x
                || mouse.column >= list_area.x + list_area.width
                || mouse.row < list_area.y
                || mouse.row >= list_area.y + list_area.height
            {
                return;
            }
            let row = (mouse.row - list_area.y) as usize;
            let idx = app.effective_offset(list_h) + row;
            if idx < app.visible.len() && matches!(app.visible[idx], Row::File(_)) {
                app.cursor = idx;
                app.sync_offset(list_h);
            }
        }
        _ => {}
    }
}

/// Wheel scroll: move the view by `delta` rows, dragging the cursor along
/// when it would leave the window. Without the drag the wheel is a no-op:
/// the offset is slaved to the cursor (`effective_offset`), so a view
/// scrolled past the cursor would snap straight back.
fn scroll_view(app: &mut App, delta: isize, list_h: usize) {
    if app.visible.is_empty() {
        return;
    }
    let list_h = list_h.max(1);
    let max = app.visible.len().saturating_sub(list_h);
    let new = app
        .effective_offset(list_h)
        .saturating_add_signed(delta)
        .min(max);
    app.offset = new;
    let bottom = (new + list_h - 1).min(app.visible.len() - 1);
    if app.cursor < new {
        // The cursor fell off the top: snap to the window's first file.
        app.cursor = clamp_to_file(&app.visible, new);
    } else if app.cursor > bottom {
        // Fell off the bottom: snap to the window's last file, searching
        // upward so the view isn't dragged back down.
        if let Some(i) = (new..=bottom)
            .rev()
            .find(|&i| matches!(app.visible[i], Row::File(_)))
        {
            app.cursor = i;
        }
    }
}

/// Draw one frame: outer block (title), list pane, preview pane, footer.
fn draw(f: &mut Frame, app: &mut App) {
    let now = chrono::Local::now();
    let title = format!(
        "ashiato · {} · {} files",
        app.root.display(),
        app.visible_count()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.ui_border))
        .title(Span::styled(title, Style::default().fg(app.ui_border)));
    let area = f.area();
    f.render_widget(block, area);
    let preview_on = app.preview_visible();
    let (list_area, preview_area, footer_area) = pane_layout(area, preview_on);
    let list_h = list_height(area.height);
    let offset = app.effective_offset(list_h);
    // Away-diff: per-row membership is checked in the row loop, so the
    // path set is built once per frame.
    let away: HashSet<&Path> = app.away_changes.iter().map(|p| p.as_path()).collect();

    // --- left pane: clustered file list -------------------------------
    let mut list_lines: Vec<Line> = Vec::with_capacity(list_h);
    let list_width = list_area.width.saturating_sub(1) as usize;
    if app.visible.is_empty() {
        let msg = if app.uncommitted_only && (app.filter.is_empty() || !app.filter_on) {
            "no uncommitted changes"
        } else if app.filter.is_empty() || !app.filter_on {
            "no files"
        } else {
            "no match"
        };
        list_lines.push(Line::from(Span::styled(
            msg,
            Style::default().fg(Color::DarkGray),
        )));
    }
    for i in offset..offset.saturating_add(list_h).min(app.visible.len()) {
        match &app.visible[i] {
            Row::Separator(c) => {
                list_lines.push(separator_line(&c.label(now), list_width, app.ui_border))
            },
            Row::File(idx) => {
                let in_away = away.contains(app.files[*idx].path.as_path());
                list_lines.push(file_line(
                    app,
                    *idx,
                    list_width,
                    i == app.cursor,
                    now,
                    in_away,
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(list_lines), list_area);

    // --- right pane: preview (hidden when --preview off, or auto on a
    // narrow terminal) ------------------------------------------------
    if preview_on {
        let pw = preview_area.width.saturating_sub(1) as usize;
        let ph = preview_area.height.saturating_sub(1) as usize;
        f.render_widget(Paragraph::new(app.preview_lines(pw, ph)), preview_area);
    }

    // --- footer -------------------------------------------------------
    let mut footer1: Vec<Span> = Vec::new();
    if app.filter_active {
        footer1.push(Span::styled(
            format!("filter: {}", app.filter),
            Style::default().fg(Color::Yellow),
        ));
        footer1.push(Span::raw("▏"));
        let footer2 = vec![hint("Enter", "apply"), hint("Esc", "clear")];
        f.render_widget(
            Paragraph::new(vec![Line::from(footer1), Line::from(footer2)]),
            footer_area,
        );
        return;
    }
    footer1.push(Span::styled(
        format!("[{}]", app.sort.label()),
        Style::default().fg(Color::White),
    ));
    footer1.push(Span::raw(" "));
    footer1.push(toggle_span("hidden", app.show_hidden));
    footer1.push(Span::raw(" "));
    footer1.push(toggle_span("dirs", app.show_dirs));
    if app.git.is_some() {
        footer1.push(Span::raw(" "));
        footer1.push(toggle_span("uncommitted", app.uncommitted_only));
    }
    if !app.filter.is_empty() {
        footer1.push(Span::raw(" "));
        footer1.push(Span::styled(
            format!(
                "[filter:{}{}]",
                app.filter,
                if app.filter_on { "" } else { " off" }
            ),
            if app.filter_on {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    if !app.selected.is_empty() {
        footer1.push(Span::raw(" "));
        footer1.push(Span::styled(
            format!("[selected:{}]", app.selected.len()),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !app.focused && !app.away_changes.is_empty() {
        footer1.push(Span::raw(" "));
        footer1.push(Span::styled(
            format!("[away: {} changed]", app.away_changes.len()),
            Style::default().fg(Color::Yellow),
        ));
    }
    let mut footer2 = vec![
        hint("Space", "select"),
        hint("Enter", if app.config.open_cmd.is_some() { "open" } else { "output" }),
        hint("y", "copy"),
        hint("/", "filter"),
        hint("\\", "toggle"),
        hint("t", "sort"),
        hint("Ctrl+h", "hidden"),
        hint("d", "dirs"),
        hint("q", "quit"),
    ];
    if app.config.alt_open_cmd.is_some() {
        footer2.push(hint("o", "open-alt"));
    }
    if app.git.is_some() {
        footer2.push(hint("u", "uncommitted"));
    }
    if let Some((msg, _, err)) = &app.status {
        footer1.push(Span::raw("  "));
        footer1.push(Span::styled(
            msg.clone(),
            Style::default().fg(if *err { Color::Red } else { Color::Yellow }),
        ));
    }
    f.render_widget(
        Paragraph::new(vec![Line::from(footer1), Line::from(footer2)]),
        footer_area,
    );
}

/// Secondary-text style: the terminal's default foreground with the DIM
/// attribute (SGR 2) — the terminal picks its own "quieter fg", which
/// tracks the user's palette and stays readable where a hard-coded
/// bright-black (`DarkGray`) can sink into the background.
fn dim_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn hint(key: &str, action: &str) -> Span<'static> {
    Span::styled(format!("{key}:{action}  "), dim_style())
}

fn toggle_span(name: &str, on: bool) -> Span<'static> {
    Span::styled(
        format!("[{name}]"),
        if on { Style::default() } else { dim_style() },
    )
}

/// Cluster header: orientation, not content — the rule rides the
/// title's border color (same as the outer frame), so the frame and
/// the clusters read as one structure; the date label is secondary
/// text (default fg + DIM, the same step as the datetime).
/// 2026-08-07: the DarkGray+DIM tier and the label's italics were
/// dropped in stages — first raised to uniform DIM, then the rule
/// separated onto the border color per the on-screen comparison.
fn separator_line(label: &str, width: usize, border: Color) -> Line<'static> {
    let rule_style = Style::default().fg(border);
    let label_style = dim_style();
    let head_w = files::display_width(&format!("── {label} "));
    let fill = "─".repeat(width.saturating_sub(head_w));
    Line::from(vec![
        Span::styled("── ", rule_style),
        Span::styled(format!("{label} "), label_style),
        Span::styled(fill, rule_style),
    ])
}

/// One file row: marker + dir part (border color — same quiet as the
/// title's frame) + basename (terminal default fg, Cyan on the cursor
/// row) + a right-aligned secondary element: the datetime (DIM;
/// italic within the hour — freshness is typography, not color), or —
/// for files with uncommitted changes in a git repo — the `+N -M`
/// marker fused with the datetime (`+7 now`; spec 4-1's change scale
/// fused with the uncommitted signal, without crushing the time
/// browser's freshness cue). Hidden when the name
/// needs the width — the cluster headers carry the date context, so
/// the name always wins. The list respects the terminal palette:
/// default fg + DIM for secondary text, ANSI accents only (2026-08-07;
/// the syntect theme fg / RGB ladder experiments were dropped).
/// No icon (2026-08-05: the 📄/📁 emoji was dropped — extension-based
/// reading is enough, per the spec's no-icon stance). Cursor and
/// Space-selected rows get the akapen gray background (spec).
///
/// Away-diff mode (terminal unfocused with a non-empty away stack):
/// the rows keep their normal colors; only the *untouched* files' times
/// sink further (DarkGray + DIM), so the touched files' times read as
/// the fresh ones by contrast (2026-08-07: the earlier whole-list dim +
/// `+` marker + focus-return flash were dropped as too loud). Focus
/// return reverts instantly.
fn file_line(
    app: &App,
    idx: usize,
    width: usize,
    is_cursor: bool,
    now: chrono::DateTime<chrono::Local>,
    in_away: bool,
) -> Line<'static> {
    let e = &app.files[idx];
    let sel = app.selected.contains(&idx);
    let away_diff = !app.focused && !app.away_changes.is_empty();
    // Cursor/selection: background change only, on the akapen gray.
    // Only the cursor row's *name* gets a Cyan accent (akapen's view
    // mode); the datetime stays gray — the time is secondary.
    let base = if is_cursor || sel {
        Style::default().bg(app.ui_selected_bg)
    } else {
        Style::default()
    };
    let marker = if is_cursor {
        "> "
    } else if sel {
        "* "
    } else {
        "  "
    };
    let name_style = if is_cursor {
        base.fg(Color::Cyan)
    } else {
        base // no fg: the terminal's default foreground
    };
    // Split the relative path: the directory part is dimmed, the
    // basename is the star of the row.
    let rel = e.display_rel();
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (&rel[..i + 1], &rel[i + 1..]),
        None => ("", rel.as_str()),
    };
    let name = if e.is_dir {
        format!("{name}/")
    } else {
        name.to_string()
    };
    // Layout priority: marker + dir + name fill the row first; the
    // right side is right-aligned only when it fits.
    // Git: a dirty file's right side is its `+N -M` marker fused with
    // the freshness time (`+7 now`) — the marker carries the change
    // scale and the uncommitted signal (spec 4-1), the time keeps
    // ashiato's core (the mtime browser must not lose "now"), so the
    // spec's "one element" is the whole right side, not a bare marker.
    // When the width tightens the time is sacrificed first — recency
    // already lives in the mtime sort and the cluster headers — and a
    // dirty row never silently loses its marker. Committed rows keep
    // exactly the pre-git display.
    let dt = format_time(now, to_local(e.mtime));
    let dt_w = files::display_width(&dt);
    let git_marker = app
        .git
        .as_ref()
        .and_then(|g| g.marker_for(&e.path))
        .map(git::GitStatus::marker);
    let git_marker_w =
        git_marker.as_ref().map(|m| files::display_width(m)).unwrap_or(0);
    let avail = width.saturating_sub(files::display_width(marker));
    let (dir, name) = fit_path(dir, &name, avail);
    let left_w = files::display_width(&dir) + files::display_width(&name);
    // At least two columns of gap, or the name butts against the element.
    let room = avail.saturating_sub(left_w).saturating_sub(2);
    let marker_fits = git_marker.is_some() && git_marker_w <= room;
    let compound_fits = marker_fits && git_marker_w + 1 + dt_w <= room;
    let time_fits = git_marker.is_none() && dt_w <= room;
    // Both pieces are secondary text (DIM). Freshness (`now`/`Nm ago` —
    // within the hour) italicizes the *time* only; a marker has no
    // freshness cue of its own (it already says "uncommitted").
    // Away-diff sinks the untouched files' elements a step further
    // (DarkGray + DIM) so the touched ones stand out by contrast — the
    // sinking outranks the fresh cue: it is the whole point.
    let fresh = is_fresh(now, to_local(e.mtime));
    let away_sink = away_diff && !in_away;
    let marker_style = {
        let mut s = base.add_modifier(Modifier::DIM);
        if away_sink {
            s = s.fg(Color::DarkGray);
        }
        s
    };
    let mut time_style = base.add_modifier(Modifier::DIM);
    if away_sink {
        time_style = time_style.fg(Color::DarkGray);
    } else if fresh {
        time_style = time_style.add_modifier(Modifier::ITALIC);
    }
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    if let Some(m) = &git_marker {
        if marker_fits {
            right_spans.push(Span::styled(m.clone(), marker_style));
            if compound_fits {
                right_spans.push(Span::raw(" "));
                right_spans.push(Span::styled(dt, time_style));
            }
        }
    } else if time_fits {
        right_spans.push(Span::styled(dt, time_style));
    }
    let right_w: usize = right_spans
        .iter()
        .map(|s| files::display_width(s.content.as_ref()))
        .sum();
    let mut spans = vec![
        Span::styled(marker, name_style),
        // The dir part is orientation, like the cluster rule: the
        // border (title) color — the same structural quiet as the
        // frame. 2026-08-07: raised from DarkGray+DIM (too dim on the
        // terminal palette) to default+DIM, then to the border color
        // per the on-screen comparison.
        Span::styled(dir, base.fg(app.ui_border)),
        Span::styled(name, name_style),
    ];
    if !right_spans.is_empty() {
        spans.push(Span::styled(" ".repeat(avail.saturating_sub(left_w + right_w)), base));
        spans.extend(right_spans);
    }
    Line::from(spans)
}

/// Fit `dir` + `name` into `avail` columns. Sacrifice order (the name is
/// sacred): the datetime first (the caller hides it), then the dir part
/// at whole-component boundaries (kept tail — the components closest to
/// the name carry the most orientation), then the name's head.
fn fit_path(dir: &str, name: &str, avail: usize) -> (String, String) {
    let dir_w = files::display_width(dir);
    let name_w = files::display_width(name);
    if dir_w + name_w <= avail {
        return (dir.to_string(), name.to_string());
    }
    if name_w >= avail {
        // The name alone doesn't fit: drop the dir, keep the name's head.
        return (String::new(), truncate_keep_head(name, avail));
    }
    // The dir part shrinks first, at component boundaries.
    let suffix = truncate_keep_tail(dir, avail - name_w);
    if suffix.is_empty() {
        (String::new(), name.to_string())
    } else {
        (format!("…/{suffix}"), name.to_string())
    }
}

/// The longest prefix of `s` that fits `avail` columns, followed by "…"
/// when truncated (char-boundary safe).
fn truncate_keep_head(s: &str, avail: usize) -> String {
    if files::display_width(s) <= avail {
        return s.to_string();
    }
    if avail == 0 {
        return String::new();
    }
    let keep = avail - 1; // room for the marker
    let mut w = 0;
    let mut cut = 0;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > keep {
            break;
        }
        w += cw;
        cut = i + ch.len_utf8();
    }
    format!("{}{}", &s[..cut], "…")
}

/// The longest suffix of `dir` that fits `avail` columns, kept at
/// whole-component boundaries (each component with its trailing `/`);
/// empty when even the last component doesn't fit. The caller adds the
/// "…/" marker.
fn truncate_keep_tail(dir: &str, avail: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for comp in dir.split('/').rev() {
        if comp.is_empty() {
            continue; // the trailing '/' is implied by the separator
        }
        let piece = format!("{comp}/");
        let pw = files::display_width(&piece);
        if w + pw > avail {
            break;
        }
        out.insert_str(0, &piece);
        w += pw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn run_config(args: &[&str]) -> Config {
        match Config::parse(args.iter().map(|s| s.to_string())).unwrap() {
            Action::Run(c) => c,
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn poll_timeout_does_not_panic_when_quiet_exceeds_burst_gap() {
        // 重いフレームで quiet > BURST_GAP になっても飽和して 5ms に落ちる
        // （以前は Duration の減算オーバーフローでパニックしていた）。
        assert_eq!(
            poll_timeout(true, BURST_GAP + Duration::from_secs(1)),
            Duration::from_millis(5)
        );
        // ちょうど BURST_GAP でも同様。
        assert_eq!(poll_timeout(true, BURST_GAP), Duration::from_millis(5));
        // バースト継続中は残り時間まで眠る。
        assert_eq!(
            poll_timeout(true, Duration::from_millis(10)),
            Duration::from_millis(TICK_MS).min(BURST_GAP - Duration::from_millis(10))
        );
        // 非バースト時は通常の tick。
        assert_eq!(poll_timeout(false, Duration::ZERO), Duration::from_millis(TICK_MS));
    }

    #[test]
    fn expand_cmd_substitutes_all_and_indexed_placeholders() {
        let paths = vec![p("/a/one.md"), p("/b/two.md")];
        assert_eq!(
            expand_cmd("akapen --theme Nord {}", &paths),
            "akapen --theme Nord '/a/one.md' '/b/two.md'"
        );
        assert_eq!(
            expand_cmd("vim -p {1} {2}", &paths),
            "vim -p '/a/one.md' '/b/two.md'"
        );
        // No indexed placeholder and no `{}` → paths appended.
        assert_eq!(
            expand_cmd("x {3}", &paths),
            "x {3} '/a/one.md' '/b/two.md'"
        );
        // No placeholder → paths appended.
        assert_eq!(expand_cmd("akapen", &paths), "akapen '/a/one.md' '/b/two.md'");
        // Non-numeric braces stay literal (and don't count as substituted).
        assert_eq!(
            expand_cmd("awk '{print}'", &paths),
            "awk '{print}' '/a/one.md' '/b/two.md'"
        );
        // Doubled braces (`{{}}` / `{{1}}`) are not a placeholder form:
        // the help text documents `{}` / `{1}`, so they stay literal
        // and the paths are appended.
        assert_eq!(
            expand_cmd("vim {{}}", &paths),
            "vim {{}} '/a/one.md' '/b/two.md'"
        );
        assert_eq!(
            expand_cmd("vim {{1}}", &paths),
            "vim {{1}} '/a/one.md' '/b/two.md'"
        );
    }

    #[test]
    fn expand_cmd_never_rescans_substituted_paths() {
        // A file name containing a placeholder must stay inert data —
        // sequential str::replace would substitute INSIDE the quoted
        // path and corrupt the command line (shell-injection surface).
        let paths = vec![p("/a/x{2}.md"), p("/b/two.md")];
        assert_eq!(
            expand_cmd("vim {1} {2}", &paths),
            "vim '/a/x{2}.md' '/b/two.md'"
        );
        let paths = vec![p("/a/x{}.md")];
        assert_eq!(expand_cmd("akapen {}", &paths), "akapen '/a/x{}.md'");
        assert_eq!(expand_cmd("akapen {1}", &paths), "akapen '/a/x{}.md'");
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote(Path::new("/a b/c'd.md")), "'/a b/c'\\''d.md'");
        assert_eq!(shell_quote(Path::new("/plain.md")), "'/plain.md'");
    }

    #[test]
    fn config_parses_flags_and_positional() {
        let c = run_config(&["/proj", "--sort", "ctime", "--show-hidden", "--show-dirs"]);
        assert_eq!(c.dir, Some(p("/proj")));
        assert_eq!(c.sort, Sort::CtimeDesc);
        assert!(c.show_hidden);
        assert!(c.show_dirs);
        assert_eq!(c.open_cmd, None);
        assert!(!c.output);
    }

    #[test]
    fn config_output_is_exclusive_with_open_cmd() {
        assert!(Config::parse(
            ["--output".to_string(), "--open-cmd".to_string(), "x".to_string()]
        )
        .is_err());
        // Same for the `o`-key command.
        assert!(Config::parse(
            ["--output".to_string(), "--alt-open-cmd".to_string(), "x".to_string()]
        )
        .is_err());
        assert!(run_config(&["--output"]).output);
    }

    #[test]
    fn theme_and_light_flags_parse() {
        let c = run_config(&["--theme", "Nord"]);
        assert_eq!(c.theme, ThemePair::both("Nord"));
        assert_eq!(c.light, None); // default: auto-detect
        assert_eq!(run_config(&["--light"]).light, Some(true));
        assert_eq!(run_config(&["--dark"]).light, Some(false));
        // The last of --light/--dark wins.
        assert_eq!(run_config(&["--light", "--dark"]).light, Some(false));
        // A bare --theme with no value is an error (a silently dropped
        // flag looks like it worked).
        assert!(Config::parse(["--theme".to_string()]).is_err());
        assert!(Config::parse(["--theme-dark".to_string()]).is_err());
        assert!(Config::parse(["--theme-light".to_string()]).is_err());
    }

    /// 設定ファイルの中身を注入して解釈する（実ファイルは読まない）。
    fn config_with_file(args: &[&str], toml: &str) -> Config {
        let file = ConfigFile::parse(Path::new("/cfg/ashiato/config.toml"), toml, None).unwrap();
        match Config::parse_with_file(args.iter().map(|s| s.to_string()), move || Ok(Some(file)))
            .unwrap()
        {
            Action::Run(c) => c,
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn the_theme_sides_default_to_none_and_pick_by_background() {
        assert_eq!(
            run_config(&[]).theme,
            ThemePair::default(),
            "既定は Highlighter が選ぶ"
        );
        let themes = ThemePair {
            dark: Some("D".into()),
            light: Some("L".into()),
        };
        assert_eq!(themes.for_background(false), Some("D"));
        assert_eq!(themes.for_background(true), Some("L"));
        assert_eq!(ThemePair::default().for_background(true), None);
    }

    #[test]
    fn the_theme_dark_and_light_flags_set_one_side_each() {
        assert_eq!(
            run_config(&["--theme-dark", "Dracula"]).theme,
            ThemePair {
                dark: Some("Dracula".into()),
                light: None
            },
            "もう片側は既定のまま"
        );
        assert_eq!(
            run_config(&["--theme-light", "Catppuccin Latte", "--theme-dark", "Nord"]).theme,
            ThemePair {
                dark: Some("Nord".into()),
                light: Some("Catppuccin Latte".into())
            }
        );
    }

    #[test]
    fn the_theme_flag_covers_both_sides_and_beats_the_per_side_flags() {
        // 1 本だったころの意味を変えない。どちらの背景でもそれを使う。
        let c = run_config(&["--theme-dark", "Nord", "--theme", "Dracula"]);
        assert_eq!(c.theme, ThemePair::both("Dracula"));
        // 順番に依らない（後ろの `--theme-light` にも勝つ）。
        let c = run_config(&["--theme", "Dracula", "--theme-light", "Catppuccin Latte"]);
        assert_eq!(c.theme, ThemePair::both("Dracula"));
    }

    #[test]
    fn the_config_file_theme_sits_under_the_flags() {
        let toml = "[theme]\ndark = \"Nord\"\nlight = \"Catppuccin Latte\"\n";
        // 設定ファイルだけ。
        assert_eq!(
            config_with_file(&[], toml).theme,
            ThemePair {
                dark: Some("Nord".into()),
                light: Some("Catppuccin Latte".into())
            }
        );
        // 片側のフラグはその側だけを上書きする。
        assert_eq!(
            config_with_file(&["--theme-light", "Solarized (light)"], toml).theme,
            ThemePair {
                dark: Some("Nord".into()),
                light: Some("Solarized (light)".into())
            }
        );
        // `--theme` は両側を上書きする。
        assert_eq!(
            config_with_file(&["--theme", "Dracula"], toml).theme,
            ThemePair::both("Dracula")
        );
        // 片側だけ書いた設定ファイルは、もう片側を既定に残す。
        assert_eq!(
            config_with_file(&[], "[theme]\nlight = \"Catppuccin Latte\"\n").theme,
            ThemePair {
                dark: None,
                light: Some("Catppuccin Latte".into())
            }
        );
    }

    #[test]
    fn the_config_file_is_read_after_the_short_circuits_and_flag_errors() {
        let broken = || -> Result<Option<ConfigFile>> { bail!("broken config") };
        // 壊れた設定ファイルでも `--help` / `--version` は読める（直し方を
        // 調べる道を塞がない）。
        assert!(matches!(
            Config::parse_with_file(["--help".to_string()], broken),
            Ok(Action::Help)
        ));
        assert!(matches!(
            Config::parse_with_file(["-V".to_string()], broken),
            Ok(Action::Version)
        ));
        // フラグの誤りが先に出る。
        let err = Config::parse_with_file(["--outpu".to_string()], broken)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("--outpu"), "{err}");
        // それ以外は、設定ファイルのエラーで止まる。
        let err = Config::parse_with_file(["--theme".to_string(), "Nord".to_string()], broken)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("broken config"), "{err}");
    }

    #[test]
    fn unknown_flags_and_extra_positionals_error() {
        assert!(Config::parse(["--outpu".to_string()]).is_err()); // typo
        assert!(Config::parse(["/a".to_string(), "/b".to_string()]).is_err());
    }

    #[test]
    fn config_defaults() {
        let c = run_config(&[]);
        assert_eq!(c.dir, None); // herdr → cwd resolution
        assert_eq!(c.sort, Sort::MtimeDesc);
        assert_eq!(c.open_cmd, None);
        assert_eq!(c.alt_open_cmd, None);
    }

    #[test]
    fn since_parses_keywords_and_durations() {
        assert_eq!(Since::parse("today"), Some(Since::Today));
        assert_eq!(Since::parse("yesterday"), Some(Since::Yesterday));
        assert_eq!(Since::parse("1d"), Some(Since::Days(1)));
        assert_eq!(Since::parse("2w"), Some(Since::Days(14)));
        assert_eq!(Since::parse("bogus"), None);
        assert_eq!(Since::parse(""), None);
        // A multibyte unit must parse-fail, not panic on a char boundary.
        assert_eq!(Since::parse("5日"), None);
        // Negative cutoffs (a future time) are rejected.
        assert_eq!(Since::parse("-1d"), None);
    }

    #[test]
    fn since_cutoff_is_midnight_for_today() {
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 5, 14, 23, 0).unwrap();
        let cutoff = Since::Today.cutoff(now);
        let dt: chrono::DateTime<chrono::Local> = cutoff.into();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-08-05 00:00:00");
        // 1d = 24h before now, to the second.
        let cutoff = Since::Days(1).cutoff(now);
        let dt: chrono::DateTime<chrono::Local> = cutoff.into();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-08-04 14:23:00");
    }

    #[test]
    fn since_yesterday_cutoff_is_the_previous_local_midnight() {
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 5, 14, 23, 0).unwrap();
        let cutoff = Since::Yesterday.cutoff(now);
        let dt: chrono::DateTime<chrono::Local> = cutoff.into();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-08-04 00:00:00");
    }

    #[test]
    fn midnight_cutoff_uses_the_single_instant() {
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 8, 5)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let expected = chrono::Local.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let cutoff = midnight_cutoff(midnight, &mut |m| {
            assert_eq!(m, midnight, "probes the requested day first");
            chrono::LocalResult::Single(expected)
        });
        let expected: std::time::SystemTime = expected.into();
        assert_eq!(cutoff, expected);
    }

    #[test]
    fn midnight_cutoff_prefers_the_earliest_ambiguous_instant() {
        // DST fall-back: the midnight occurs twice; the cutoff is the
        // earlier occurrence (the day's true start).
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let early = chrono::Local.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let late = early + chrono::Duration::hours(1);
        let cutoff = midnight_cutoff(midnight, &mut |_| {
            chrono::LocalResult::Ambiguous(early, late)
        });
        let early: std::time::SystemTime = early.into();
        assert_eq!(cutoff, early);
    }

    #[test]
    fn midnight_cutoff_steps_back_over_a_skipped_day() {
        // A whole day skipped (DST jump — Pacific/Apia skipped
        // 2011-12-30): the midnight doesn't exist, so the cutoff falls
        // back to the previous day's midnight. (The resolver fakes the
        // skipped day; the fallback probe resolves on the real local
        // timezone.)
        let midnight = chrono::NaiveDate::from_ymd_opt(2011, 12, 30)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let mut probes = 0;
        let cutoff = midnight_cutoff(midnight, &mut |m| {
            probes += 1;
            if m == midnight {
                chrono::LocalResult::None
            } else {
                chrono::LocalResult::Single(
                    chrono::Local
                        .from_local_datetime(&m)
                        .earliest()
                        .expect("a normal local midnight resolves"),
                )
            }
        });
        assert_eq!(probes, 2, "one step back to the previous midnight");
        let expected: chrono::DateTime<chrono::Local> = chrono::Local
            .from_local_datetime(
                &chrono::NaiveDate::from_ymd_opt(2011, 12, 29)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .expect("the previous midnight exists");
        let expected: std::time::SystemTime = expected.into();
        assert_eq!(cutoff, expected);
    }

    #[test]
    fn filter_entries_applies_text_and_since() {
        use std::time::{Duration, UNIX_EPOCH};
        let now = chrono::Local.with_ymd_and_hms(2026, 8, 5, 12, 0, 0).unwrap();
        let mk = |rel: &str, age_h: u64| FileEntry {
            path: p(&format!("/r/{rel}")),
            rel: p(rel),
            rel_lower: rel.to_lowercase(),
            mtime: UNIX_EPOCH + Duration::from_secs(now.timestamp() as u64 - age_h * 3600),
            ctime: UNIX_EPOCH,
            is_dir: false,
            size: 0,
        };
        let entries = vec![
            mk("a.md", 1),   // 1h ago
            mk("b.rs", 30),  // 30h ago
            mk("c.md", 200), // 200h ago
        ];
        // --since 2d keeps a.md and b.rs.
        let out = filter_entries(entries.clone(), "", Some(Since::Days(2)), now);
        let names: Vec<&str> = out.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, vec!["a.md", "b.rs"]);
        // --filter .md keeps only the markdown files.
        let out = filter_entries(entries.clone(), ".md", None, now);
        let names: Vec<&str> = out.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, vec!["a.md", "c.md"]);
        // Both combined.
        let out = filter_entries(entries, ".md", Some(Since::Days(1)), now);
        let names: Vec<&str> = out.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, vec!["a.md"]);
    }

    #[test]
    fn files_flag_parses() {
        let c = run_config(&["--files"]);
        assert!(c.files);
        assert_eq!(c.format, OutputFormat::Path);
        let c = run_config(&["--files", "--format", "tsv", "--since", "1d"]);
        assert_eq!(c.format, OutputFormat::Tsv);
        assert_eq!(c.since, Some(Since::Days(1)));
        // Invalid --since/--format values are errors, not silent no-ops.
        assert!(Config::parse(["--since".to_string(), "bogus".to_string()]).is_err());
        assert!(Config::parse(["--format".to_string(), "bogus".to_string()]).is_err());
    }

    #[test]
    fn preview_flag_parses_modes() {
        assert_eq!(run_config(&["--preview", "on"]).preview, PreviewMode::On);
        assert_eq!(run_config(&["--preview", "off"]).preview, PreviewMode::Off);
        assert_eq!(run_config(&["--preview", "auto"]).preview, PreviewMode::Auto);
        assert!(Config::parse(["--preview".to_string(), "bogus".to_string()]).is_err());
        assert_eq!(run_config(&[]).preview, PreviewMode::Auto); // default
    }

    #[test]
    fn filter_flag_seeds_the_initial_filter() {
        let c = run_config(&["--filter", ".md"]);
        assert_eq!(c.filter, ".md");
        // Default: no filter.
        assert_eq!(run_config(&[]).filter, "");
        // A bare `--filter` with no value is an error.
        assert!(Config::parse(["--filter".to_string()]).is_err());
    }

    #[test]
    fn open_cmd_is_optional() {
        let c = run_config(&["--open-cmd", "vim -p {}"]);
        assert_eq!(c.open_cmd.as_deref(), Some("vim -p {}"));
        // A bare `--open-cmd` with no value is an error.
        assert!(Config::parse(["--open-cmd".to_string()]).is_err());
    }

    #[test]
    fn alt_open_cmd_is_optional() {
        let c = run_config(&["--alt-open-cmd", "yazi {}"]);
        assert_eq!(c.alt_open_cmd.as_deref(), Some("yazi {}"));
        // Default: none — the `o` key is a dead key.
        assert_eq!(run_config(&[]).alt_open_cmd, None);
        // A bare `--alt-open-cmd` with no value is an error.
        assert!(Config::parse(["--alt-open-cmd".to_string()]).is_err());
    }

    #[test]
    fn sort_flag_parses_and_rejects_unknown() {
        assert_eq!(run_config(&["--sort", "mtime"]).sort, Sort::MtimeDesc);
        assert_eq!(run_config(&["--sort", "ctime"]).sort, Sort::CtimeDesc);
        assert!(Config::parse(["--sort".to_string(), "bogus".to_string()]).is_err());
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(Config::parse(["--help".to_string()]), Ok(Action::Help)));
        assert!(matches!(Config::parse(["-V".to_string()]), Ok(Action::Version)));
    }

    #[test]
    fn clamp_to_file_skips_separators() {
        // visible: [Sep(Today), File(0), File(1), Sep(Yesterday), File(2)]
        let visible = vec![
            Row::Separator(Cluster::Today),
            Row::File(0),
            Row::File(1),
            Row::Separator(Cluster::Yesterday),
            Row::File(2),
        ];
        assert_eq!(clamp_to_file(&visible, 0), 1);
        assert_eq!(clamp_to_file(&visible, 4), 4);
        assert_eq!(clamp_to_file(&visible, 3), 4); // separator → next file
        assert_eq!(clamp_to_file(&visible, 2), 2);
        assert_eq!(clamp_to_file(&visible, 99), 4);
    }

    #[test]
    fn filter_toggle_keeps_text_and_reveals_all() {
        let mut app = App {
            config: run_config(&["--filter", ".md"]),
            root: p("/r"),
            files: vec![
                FileEntry {
                    path: p("/r/a.md"),
                    rel: p("a.md"),
                    rel_lower: "a.md".into(),
                    mtime: std::time::UNIX_EPOCH,
                    ctime: std::time::UNIX_EPOCH,
                    is_dir: false,
                    size: 0,
                },
                FileEntry {
                    path: p("/r/b.rs"),
                    rel: p("b.rs"),
                    rel_lower: "b.rs".into(),
                    mtime: std::time::UNIX_EPOCH,
                    ctime: std::time::UNIX_EPOCH,
                    is_dir: false,
                    size: 0,
                },
            ],
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: ".md".into(),
            filter_on: true,
            since: None,
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        };
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 1, "filter applies when on");
        // Toggle off: everything shows, the text is kept.
        app.filter_on = false;
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 2, "all files when filter is off");
        assert_eq!(app.filter, ".md", "the filter text survives the toggle");
        // Toggle back on: the same filtered view returns.
        app.filter_on = true;
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 1, "filter restores when toggled on");
    }

    #[test]
    fn since_applies_in_the_tui_listing() {
        use std::time::{Duration as StdDuration, UNIX_EPOCH};
        // rebuild_visible compares against the real clock (Local::now()),
        // so mtimes must be relative to it — a fixed date turns into a
        // time bomb the day the calendar passes it.
        let now = chrono::Local::now();
        let mk = |rel: &str, age_h: u64| FileEntry {
            path: p(&format!("/r/{rel}")),
            rel: p(rel),
            rel_lower: rel.to_lowercase(),
            mtime: UNIX_EPOCH
                + StdDuration::from_secs(now.timestamp() as u64 - age_h * 3600),
            ctime: UNIX_EPOCH,
            is_dir: false,
            size: 0,
        };
        let mut app = App {
            config: run_config(&[]),
            root: p("/r"),
            files: vec![mk("new.md", 1), mk("old.md", 30)],
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_on: true,
            since: Some(Since::Days(1)),
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        };
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 1, "--since 1d keeps only the fresh file");
    }

    #[test]
    fn refresh_clears_the_listing_when_the_root_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut app = App {
            config: run_config(&[]),
            root: dir.path().to_path_buf(),
            files: files::scan(dir.path(), false, false).unwrap(),
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_on: true,
            since: None,
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        };
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 1);
        // Unchanged listing: no commit, no status message.
        app.refresh_if_changed();
        assert_eq!(app.files.len(), 1);
        assert!(app.status.is_none());
        // Delete the root: the stale listing must not linger.
        drop(dir);
        app.refresh_if_changed();
        assert!(app.files.is_empty(), "a deleted root empties the listing");
        assert!(
            app.status.as_ref().is_some_and(|(_, _, err)| *err),
            "the user is told the root is gone"
        );
        // Already empty: no repeated alert on every tick.
        app.status = None;
        app.refresh_if_changed();
        assert!(app.status.is_none(), "no repeated alerts");
    }

    #[test]
    fn away_diff_accumulates_while_unfocused_then_flashes_and_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        // 更新時刻は環境の時計の粒度（Linux では数 ms）に任せず明示する。
        // 同じ大きさで続けて書き換えると、粗い環境では mtime が一致して
        // 変化として見えないため（検出は mtime・大きさ・種別の比較）。
        let base = std::time::SystemTime::now() - Duration::from_secs(1000);
        let write_at = |path: &Path, body: &str, secs: u64| {
            std::fs::write(path, body).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(base + Duration::from_secs(secs))
                .unwrap();
        };
        write_at(&f, "v1", 0);
        let mut app = App {
            config: run_config(&[]),
            root: dir.path().to_path_buf(),
            files: files::scan(dir.path(), false, false).unwrap(),
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_on: true,
            since: None,
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        };
        // Focused: edits don't accumulate.
        write_at(&f, "v2", 10);
        app.refresh_if_changed();
        assert!(app.away_changes.is_empty(), "focused edits are not away-changes");
        // Away: edits stack up, deduped, in first-seen order.
        app.focus_lost();
        write_at(&f, "v3", 20);
        app.refresh_if_changed();
        let g = dir.path().join("b.txt");
        write_at(&g, "x", 30);
        app.refresh_if_changed();
        assert_eq!(app.away_changes.len(), 2, "two files changed while away");
        assert_eq!(app.away_changes[0], f);
        assert_eq!(app.away_changes[1], g);
        app.refresh_if_changed();
        assert_eq!(app.away_changes.len(), 2, "no double-count on rescan");
        // Back: the stack drops instantly — no flash, straight to normal.
        app.focus_gained();
        assert!(
            app.away_changes.is_empty(),
            "focus return drops the away stack immediately"
        );
    }

    #[test]
    fn effective_offset_keeps_cursor_visible() {
        let mut app = App {
            config: run_config(&[]),
            root: p("/r"),
            files: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_on: true,
            since: None,
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        };
        // 10 file rows, 5 visible; cursor at 9.
        app.visible = (0..10).map(|i| Row::File(i)).collect();
        app.cursor = 9;
        assert_eq!(app.effective_offset(5), 5);
        app.offset = 99;
        assert_eq!(app.effective_offset(5), 5); // clamped to max
        app.cursor = 0;
        app.offset = 5;
        assert_eq!(app.effective_offset(5), 0); // pulled back up
    }

    #[test]
    fn fit_path_keeps_name_full_before_truncating() {
        // Fits: nothing truncated.
        assert_eq!(fit_path("src/util/", "main.rs", 20), ("src/util/".into(), "main.rs".into()));
        // Name + dir overflow: the dir shrinks at component boundaries,
        // keeping the tail (components closest to the name).
        assert_eq!(fit_path("src/util/", "main.rs", 14), ("…/util/".into(), "main.rs".into()));
        assert_eq!(fit_path("a/very/long/dir/", "main.rs", 20), ("…/long/dir/".into(), "main.rs".into()));
        // Not even one dir component fits: the dir is dropped entirely.
        assert_eq!(fit_path("src/util/", "main.rs", 8), ("".into(), "main.rs".into()));
        // The name alone doesn't fit: its head is kept with "…".
        assert_eq!(fit_path("", "main.rs", 4), ("".into(), "mai…".into()));
        assert_eq!(fit_path("src/", "main.rs", 4), ("".into(), "mai…".into()));
        // Root-level files: no dir part.
        assert_eq!(fit_path("", "README.md", 20), ("".into(), "README.md".into()));
    }

    #[test]
    fn truncate_keep_head_is_char_boundary_safe() {
        assert_eq!(truncate_keep_head("main.rs", 5), "main…");
        assert_eq!(truncate_keep_head("main.rs", 20), "main.rs");
        // CJK: 日(2) fits keep=3, 本 would overflow.
        assert_eq!(truncate_keep_head("日本語.rs", 4), "日…");
        assert_eq!(truncate_keep_head("main.rs", 0), "");
        assert_eq!(truncate_keep_head("main.rs", 1), "…");
    }

    #[test]
    fn truncate_keep_tail_keeps_whole_components() {
        assert_eq!(truncate_keep_tail("src/util/", 5), "util/");
        assert_eq!(truncate_keep_tail("src/util/", 9), "src/util/");
        assert_eq!(truncate_keep_tail("src/util/", 4), ""); // even "util/" (5) doesn't fit
        assert_eq!(truncate_keep_tail("a/b/c/", 3), "c/");
        assert_eq!(truncate_keep_tail("a/b/c/", 2), "c/"); // exactly fits
    }

    /// A bare App over the given entries (TUI-less unit tests).
    fn test_app(files: Vec<FileEntry>) -> App {
        App {
            config: run_config(&[]),
            root: p("/r"),
            files,
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: BTreeSet::new(),
            filter: String::new(),
            filter_on: true,
            since: None,
            filter_active: false,
            sort: Sort::MtimeDesc,
            show_hidden: false,
            show_dirs: false,
            light: false,
            highlight: Highlighter::new(None, false),
            ui_selected_bg: theme::selected_bg(false),
            ui_border: theme::border_color(false),
            preview_cache: None,
            status: None,
            pending_output: None,
            needs_immediate_redraw: false,
            last_refresh: Instant::now(),
            refresh_every: REFRESH_TICK,
            last_input: Instant::now(),
            bursting: false,
            last_render_finish: None,
            focused: true,
            away_changes: Vec::new(),
            git: None,
            uncommitted_only: false,
            running: true,
        }
    }

    #[test]
    fn ctrl_c_quits_like_q() {
        let mut app = test_app(Vec::new());
        on_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, None);
        assert!(!app.running, "Ctrl+C quits (raw mode eats the tty's SIGINT)");
        // A plain `c` (no control) must NOT quit.
        let mut app = test_app(Vec::new());
        on_key(&mut app, KeyCode::Char('c'), KeyModifiers::empty(), None);
        assert!(app.running);
    }

    #[test]
    fn ctrl_c_quits_even_while_filtering() {
        let mut app = test_app(Vec::new());
        app.filter_active = true;
        on_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, None);
        assert!(!app.running);
    }

    #[test]
    fn ctrl_z_without_a_terminal_is_a_noop() {
        // The suspend path needs the TUI guard and a foreground pgrp;
        // with neither (unit tests) it must not stop the process.
        let mut app = test_app(Vec::new());
        on_key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL, None);
        assert!(app.running);
    }

    /// A `Write` sink for the Vec-backed terminal in the TermGuard
    /// tests: the bytes the guard emits on drop land in a shared
    /// buffer the test can inspect afterwards.
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A terminal over a shared in-memory buffer — no tty needed. A
    /// fixed viewport skips the backend size query (`Terminal::new`
    /// would ioctl the real terminal).
    fn buf_terminal(buf: &SharedBuf) -> Term {
        let writer: Box<dyn std::io::Write> = Box::new(buf.clone());
        Terminal::with_options(
            CrosstermBackend::new(writer),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 1, 1)),
            },
        )
        .unwrap()
    }

    /// A guard over [`buf_terminal`] whose color-scheme subscription writes
    /// to the same buffer, so the test sees both in the order written.
    /// `fixed` is the `--light` / `--dark` shape (`Subscription::fixed`).
    fn buf_guard(buf: &SharedBuf, fixed: bool) -> TermGuard {
        TermGuard {
            term: Some(buf_terminal(buf)),
            raw: false,
            alt: false,
            scheme: if fixed {
                Subscription::fixed()
            } else {
                Subscription::with_writer(buf.clone())
            },
        }
    }

    impl SharedBuf {
        /// Everything written so far, emptying the buffer.
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    #[test]
    fn term_guard_drop_restores_entered_terminal_state() {
        // Normal exit: raw mode on, alternate screen entered — Drop
        // must leave the alternate screen, show the cursor, and stop
        // mouse/focus reporting (the shell's screen restored).
        let buf = SharedBuf::default();
        let mut guard = buf_guard(&buf, false);
        guard.raw = true;
        guard.enter();
        buf.take();
        drop(guard);
        let bytes = buf.take();
        assert!(bytes.contains("\x1b[?1049l"), "leaves the alternate screen");
        assert!(bytes.contains("\x1b[?25h"), "shows the cursor");
        assert!(bytes.contains("\x1b[?1000l"), "disables mouse capture");
        assert!(bytes.contains("\x1b[?1004l"), "disables focus reporting");
        assert!(
            bytes.starts_with("\x1b[?2031l"),
            "drops the color-scheme subscription first: {bytes:?}"
        );
    }

    #[test]
    fn term_guard_drop_is_a_noop_while_a_child_owns_the_terminal() {
        // open_selection's child window: raw mode off and the alternate
        // screen left — Drop must not restore anything (the child owns
        // the terminal; a second teardown would corrupt its screen, and
        // the subscription the child may have made is the child's).
        let buf = SharedBuf::default();
        let mut guard = buf_guard(&buf, false);
        guard.enter();
        guard.hand_off(leave_for_child);
        buf.take();
        drop(guard);
        assert_eq!(buf.take(), "");
    }

    #[test]
    fn term_guard_drop_without_a_terminal_only_disables_raw_mode() {
        // make_terminal failed: raw mode on but no terminal yet — Drop
        // must not write anywhere (and must not panic).
        let guard = TermGuard {
            term: None,
            raw: true,
            alt: false,
            scheme: Subscription::fixed(),
        };
        drop(guard);
    }

    #[test]
    fn burst_tracker_flips_on_rapid_events() {
        let mut app = test_app(Vec::new());
        // Startup sets last_input = now; model a press arriving later.
        app.last_input = Instant::now() - Duration::from_millis(500);
        mark_input(&mut app); // a deliberate press after a long pause
        assert!(!app.bursting);
        std::thread::sleep(Duration::from_millis(10));
        mark_input(&mut app); // 10ms later: a burst (held j/k repeat)
        assert!(app.bursting);
        std::thread::sleep(Duration::from_millis(100));
        mark_input(&mut app); // a deliberate press after a pause
        assert!(!app.bursting);
    }

    #[test]
    fn burst_tracker_counts_a_key_right_after_a_slow_render_as_burst() {
        // The stutter cascade: a render that takes longer than BURST_GAP
        // inflates the quiet gap, so the next repeat would look
        // deliberate and re-render (per-key renders for the whole hold).
        // A key arriving within BURST_GAP of the render's finish was
        // queued during it — a hold, so it must count as a burst.
        let mut app = test_app(Vec::new());
        app.last_input = Instant::now() - Duration::from_millis(500);
        mark_input(&mut app); // deliberate press: rendered immediately
        assert!(!app.bursting);
        // The render (slow, e.g. a minified bundle) just finished.
        app.last_render_finish = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(10));
        mark_input(&mut app); // the repeat queued behind the render
        assert!(app.bursting, "a key within BURST_GAP of the render = hold");
        // The burst ends normally once the key flow stops.
        std::thread::sleep(Duration::from_millis(100));
        mark_input(&mut app);
        assert!(!app.bursting);
    }

    #[test]
    fn preview_defers_while_bursting_and_renders_when_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let x = dir.path().join("x.rs");
        std::fs::write(&x, "fn main() {}\n// note\n").unwrap();
        let y = dir.path().join("y.rs");
        std::fs::write(&y, "fn y() {}\n// hi\n").unwrap();
        let mk = |p: &std::path::Path, secs: u64| FileEntry {
            path: p.to_path_buf(),
            rel: p.file_name().unwrap().into(),
            rel_lower: p.file_name().unwrap().to_string_lossy().to_lowercase(),
            mtime: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            ctime: std::time::UNIX_EPOCH,
            is_dir: false,
            size: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        };
        let mut app = test_app(vec![mk(&x, 0)]);
        app.rebuild_visible(None);
        // Deliberate input (not bursting): rendered immediately.
        let lines = app.preview_lines(40, 10);
        assert!(lines[0].to_string().contains("x.rs"));
        assert!(lines.len() > 2, "rendered rows, not a placeholder");
        // Move to an uncached file while bursting: placeholder only.
        app.files.push(mk(&y, 1));
        app.rebuild_visible(None);
        app.cursor = first_file_row(&app.visible) + 1;
        app.bursting = true;
        let lines = app.preview_lines(40, 10);
        assert_eq!(lines.len(), 2, "header + ellipsis while bursting");
        assert!(lines[0].to_string().contains("y.rs"));
        assert!(lines[1].to_string().contains('…'));
        // Burst over: the real preview renders and caches.
        app.bursting = false;
        let lines = app.preview_lines(40, 10);
        assert!(lines.len() > 2);
        assert!(lines[0].to_string().contains("y.rs"));
        // And the cached preview is used even while bursting again.
        app.bursting = true;
        let lines = app.preview_lines(40, 10);
        assert!(lines.len() > 2, "cached preview survives bursts");
    }

    #[test]
    fn sort_cycle_keeps_selection_on_the_same_files() {
        use std::time::{Duration as StdDuration, UNIX_EPOCH};
        let mk = |rel: &str, secs: u64| FileEntry {
            path: p(&format!("/r/{rel}")),
            rel: p(rel),
            rel_lower: rel.to_lowercase(),
            mtime: UNIX_EPOCH + StdDuration::from_secs(secs),
            ctime: UNIX_EPOCH + StdDuration::from_secs(secs),
            is_dir: false,
            size: 0,
        };
        // mtime↓: b.md (20) sits at index 0, a.md (10) at index 1.
        let mut app = test_app(vec![mk("b.md", 20), mk("a.md", 10)]);
        app.selected.insert(0); // b.md
        app.sort = app.sort.next(); // mtime↑ — reverses the order
        app.resort_files();
        // The selection must follow the file, not the slot: without the
        // remap, index 0 would now mean a.md.
        assert_eq!(app.selected_paths(), vec![p("/r/b.md")]);
    }

    #[test]
    fn key_moves_scroll_the_view_only_at_the_window_edge() {
        let mut app = test_app(Vec::new());
        app.visible = (0..30).map(Row::File).collect();
        let h = 10;
        // j to the window's bottom: the view scrolls one row, the
        // cursor sits on the bottom row.
        for _ in 0..10 {
            app.move_down(h);
        }
        assert_eq!(app.cursor, 10);
        assert_eq!(app.offset, 1);
        assert_eq!(app.cursor - app.offset, 9);
        // k must move the cursor up within the window. (Regression:
        // key moves never synced `offset`, so effective_offset
        // re-scrolled the view under the cursor and it stayed glued
        // to the bottom row.)
        app.move_up(h);
        assert_eq!(app.cursor, 9);
        assert_eq!(app.offset, 1);
        assert_eq!(app.cursor - app.offset, 8, "cursor leaves the bottom row");
        // All the way down to the last file, then back up: the view
        // follows the cursor, which climbs through the window.
        while app.cursor < 29 {
            app.move_down(h);
        }
        assert_eq!(app.offset, 20);
        app.move_up(h);
        assert_eq!(app.cursor, 28);
        assert_eq!(app.offset, 20, "no re-scroll under a moving cursor");
        // Wheel-scrolled state (cursor at the window's top): up keeps
        // scrolling the view at the top edge (vim-style), down walks
        // the cursor into the window without moving the view.
        app.cursor = 5;
        app.offset = 5;
        app.move_up(h);
        assert_eq!(app.cursor, 4);
        assert_eq!(app.offset, 4, "top edge: up scrolls the view");
        app.move_down(h);
        assert_eq!(app.cursor, 5);
        assert_eq!(app.offset, 4, "cursor inside the window: the view stays");
    }

    #[test]
    fn wheel_scroll_moves_view_and_drags_cursor() {
        let mut app = test_app(Vec::new());
        app.visible = (0..10).map(Row::File).collect();
        // 5-row window, cursor at the top: scrolling down used to be a
        // no-op (effective_offset snapped the view back to the cursor).
        scroll_view(&mut app, 1, 5);
        assert_eq!(app.offset, 1);
        assert_eq!(app.cursor, 1, "cursor is dragged along the top edge");
        // The offset clamps at len - list_h no matter how far we scroll.
        for _ in 0..20 {
            scroll_view(&mut app, 1, 5);
        }
        assert_eq!(app.offset, 5);
        // Cursor at the bottom: scrolling up drags it along the bottom edge.
        app.cursor = 9;
        app.offset = 5;
        scroll_view(&mut app, -1, 5);
        assert_eq!(app.offset, 4);
        assert_eq!(app.cursor, 8, "cursor snaps to the window's last row");
    }

    #[test]
    fn move_up_at_the_top_stays_on_the_first_file() {
        // visible: [Sep(Today), File(0), File(1), Sep(Yesterday), File(2)]
        let mut app = test_app(Vec::new());
        app.visible = vec![
            Row::Separator(Cluster::Today),
            Row::File(0),
            Row::File(1),
            Row::Separator(Cluster::Yesterday),
            Row::File(2),
        ];
        let h = 10; // the whole list fits in the window
        app.cursor = 1;
        app.offset = 0;
        // k from the first file: the Today separator above is never a
        // cursor target, so the cursor stays put. (Regression: move_up
        // ran off the top onto the separator, which blanked the preview
        // and made Enter fail with "no files".)
        app.move_up(h);
        assert_eq!(app.cursor, 1, "k at the top stays on the first file");
        assert_eq!(app.offset, 0, "the view must not move");
        // k from the later cluster still skips its separator.
        app.cursor = 4;
        app.move_up(h);
        assert_eq!(app.cursor, 2, "k skips the Yesterday separator");
    }

    #[test]
    fn movement_keys_and_scroll_never_land_on_separators() {
        // visible: [Sep(Today), F(0), F(1), F(2), F(3), Sep(Yesterday),
        //           F(4), F(5), F(6), F(7), F(8)]
        let mut app = test_app(Vec::new());
        app.visible = vec![
            Row::Separator(Cluster::Today),
            Row::File(0),
            Row::File(1),
            Row::File(2),
            Row::File(3),
            Row::Separator(Cluster::Yesterday),
            Row::File(4),
            Row::File(5),
            Row::File(6),
            Row::File(7),
            Row::File(8),
        ];
        let h = 5;
        let assert_on_file = |app: &App, ctx: &str| {
            assert!(
                matches!(app.visible.get(app.cursor), Some(Row::File(_))),
                "{ctx}: cursor {} must sit on a file row",
                app.cursor
            );
        };
        // j walks down to the first cluster's last file, then crosses
        // the separator onto the first Yesterday file.
        app.cursor = 1;
        app.offset = 0;
        for _ in 0..3 {
            app.move_down(h);
        }
        assert_eq!(app.cursor, 4);
        app.move_down(h);
        assert_eq!(app.cursor, 6, "j skips the Yesterday separator");
        assert_on_file(&app, "after j across the separator");
        // k crosses back over the separator, then walks up to the top —
        // where k must stop on the first file, not the Today separator.
        app.move_up(h);
        assert_eq!(app.cursor, 4, "k skips the Yesterday separator");
        for _ in 0..3 {
            app.move_up(h);
        }
        assert_eq!(app.cursor, 1);
        app.move_up(h);
        assert_eq!(app.cursor, 1, "k at the top stays on the first file");
        // g / G: the exact expressions the key handler runs.
        app.cursor = first_file_row(&app.visible);
        assert_eq!(app.cursor, 1, "g lands on the first file");
        app.cursor = clamp_to_file(&app.visible, app.visible.len().saturating_sub(1));
        assert_eq!(app.cursor, 10, "G lands on the last file");
        // Half-page (PgDn/PgUp): clamp_to_file snaps past separators.
        app.cursor = 3;
        half_page_down(&mut app, h);
        assert_eq!(app.cursor, 6, "PgDn snaps to the next file");
        half_page_up(&mut app, h);
        assert_eq!(app.cursor, 4, "PgUp snaps to the previous file");
        half_page_up(&mut app, h);
        assert_eq!(app.cursor, 2);
        half_page_up(&mut app, h);
        assert_eq!(app.cursor, 1, "PgUp at the top snaps to the first file");
        // Wheel: scroll down until the offset lands on the Yesterday
        // separator — the dragged cursor must snap to the next file.
        app.cursor = 1;
        app.offset = 0;
        for _ in 0..5 {
            scroll_view(&mut app, 1, h);
        }
        assert_eq!(app.offset, 5, "offset sits on the separator row");
        assert_eq!(app.cursor, 6, "the dragged cursor skips the separator");
        assert_on_file(&app, "after the drag onto a separator offset");
        // Scroll up: while the cursor stays inside the window, the view
        // is free to sit on a separator.
        scroll_view(&mut app, -1, h);
        assert_eq!(app.offset, 4);
        assert_eq!(app.cursor, 6);
        // Cursor below the window after a wheel-up: it snaps to the
        // window's last file row, searching upward past the separator.
        app.cursor = 10;
        app.offset = 6;
        scroll_view(&mut app, -1, h);
        assert_eq!(app.offset, 5);
        assert_eq!(app.cursor, 9, "the bottom drag lands on a file");
    }

    #[test]
    fn enter_tui_modes_re_enables_focus_reporting() {
        // 子プロセス（akapen/vim 等の crossterm 系）は終了時に
        // DisableFocusChange を送り得るため、TUI 復帰時にもフォーカス
        // レポートを有効化し直す必要がある。欠けると away-diff が
        // セッション中ずっと無効化されたままになる（回帰防止: 復帰時の
        // 再送に EnableFocusChange が含まれていることを ANSI で検証）。
        let mut out = Vec::new();
        enter_tui_modes(&mut out).unwrap();
        let ansi = String::from_utf8(out).unwrap();
        assert!(ansi.contains("\x1b[?1004h"), "EnableFocusChange missing: {ansi:?}");
        assert!(ansi.contains("\x1b[?1049h"), "EnterAlternateScreen missing: {ansi:?}");
        assert!(ansi.contains("\x1b[?25l"), "Hide missing: {ansi:?}");
        assert!(ansi.contains("\x1b[?1000h"), "EnableMouseCapture missing: {ansi:?}");
    }

    #[test]
    fn open_selection_alt_rescans_and_reports_done_after_child_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "1").unwrap();
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.config.alt_open_cmd = Some("true".to_string());
        app.rescan();
        let c = dir.path().join("c.md");
        std::fs::write(&c, "3").unwrap();
        open_selection_alt(&mut app, None);
        assert!(
            app.files.iter().any(|e| e.path == c),
            "rescan after the child exit picks up external edits"
        );
        assert_eq!(
            app.status.as_ref().map(|(m, _, err)| (m.as_str(), *err)),
            Some(("done — list rescanned", false))
        );
        assert!(app.running);
    }

    #[test]
    fn o_key_is_a_dead_key_without_alt_open_cmd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "1").unwrap();
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.rescan();
        app.config.alt_open_cmd = None;
        open_selection_alt(&mut app, None);
        assert_eq!(
            app.status.as_ref().map(|(m, _, err)| (m.as_str(), *err)),
            Some(("no --alt-open-cmd", true))
        );
        assert!(app.running);
    }

    #[test]
    fn open_selection_rescans_and_reports_done_after_child_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "1").unwrap();
        std::fs::write(dir.path().join("b.md"), "2").unwrap();
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.config.open_cmd = Some("true".to_string());
        app.rescan();
        // The child (simulated: `true`) edits the tree while the TUI is
        // suspended; the post-exit rescan must pick the new file up, and
        // the "scanning…" placeholder must give way to the done message.
        let c = dir.path().join("c.md");
        std::fs::write(&c, "3").unwrap();
        app.focused = false;
        app.away_changes = vec![c.clone()];
        open_selection(&mut app, None);
        assert!(
            app.files.iter().any(|e| e.path == c),
            "rescan after the child exit picks up external edits"
        );
        assert_eq!(
            app.status.as_ref().map(|(m, _, err)| (m.as_str(), *err)),
            Some(("done — list rescanned", false))
        );
        // The child owned the terminal: we are focused again and the
        // stale away stack must not flip the away-diff on.
        assert!(app.focused);
        assert!(app.running);
    }

    /// A temp git repo with one commit; `None` (test skipped) when git
    /// is unavailable. The repo lives under the *canonical* temp root
    /// so its paths match `git rev-parse`'s symlink-resolved output
    /// (macOS `/var` → `/private/var`) — the prefix match in
    /// `GitCache::marker_for` depends on both sides agreeing.
    fn git_repo(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
        if !git::git_available() {
            return None;
        }
        let base =
            std::env::temp_dir().canonicalize().unwrap_or_else(|_| std::env::temp_dir());
        let dir = tempfile::Builder::new().tempdir_in(&base).unwrap();
        git::git(dir.path(), &["init", "-q"]);
        git::git(dir.path(), &["config", "user.email", "t@t"]);
        git::git(dir.path(), &["config", "user.name", "t"]);
        for (name, content) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        git::git(dir.path(), &["add", "."]);
        git::git(dir.path(), &["commit", "-qm", "init"]);
        Some(dir)
    }

    #[test]
    fn u_key_toggles_uncommitted_only_view_in_a_repo() {
        let Some(dir) = git_repo(&[("a.md", "one\n"), ("b.rs", "two\n")]) else {
            return;
        };
        // An unstaged edit and an untracked file — both must count as
        // uncommitted (spec: `??` は未コミット変更として扱う).
        std::fs::write(dir.path().join("b.rs"), "two\nchanged\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "new\n").unwrap();
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.git = git::GitCache::discover(dir.path());
        app.rescan();
        assert_eq!(app.visible_count(), 3);
        // `u` filters to uncommitted files only.
        on_key(&mut app, KeyCode::Char('u'), KeyModifiers::empty(), None);
        assert!(app.uncommitted_only);
        assert_eq!(
            app.visible_count(),
            2,
            "only the edited b.rs and the untracked c.txt remain"
        );
        // The dirty rows render the marker fused with the freshness
        // time; the committed row shows the plain time (no '+' anywhere).
        let now = chrono::Local::now();
        let idx = |rel: &str| app.files.iter().position(|e| e.rel == p(rel)).unwrap();
        let dirty = file_line(&app, idx("b.rs"), 80, false, now, false);
        assert!(dirty.to_string().contains("+1"), "b.rs: one added line");
        assert!(
            dirty.to_string().contains("+1 now"),
            "the marker is fused with the freshness time, not replacing it"
        );
        let clean = file_line(&app, idx("a.md"), 80, false, now, false);
        assert!(
            !clean.to_string().contains('+'),
            "committed rows keep the time display, no marker"
        );
        // `u` again: back to the full listing.
        on_key(&mut app, KeyCode::Char('u'), KeyModifiers::empty(), None);
        assert!(!app.uncommitted_only);
        assert_eq!(app.visible_count(), 3);
    }

    #[test]
    fn dirty_rows_sacrifice_time_before_marker_on_narrow_widths() {
        let Some(dir) = git_repo(&[("src/b.rs", "two\n"), ("a.md", "one\n")]) else {
            return;
        };
        std::fs::write(dir.path().join("src/b.rs"), "two\nchanged\n").unwrap();
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.git = git::GitCache::discover(dir.path());
        app.rescan();
        let now = chrono::Local::now();
        let idx = app.files.iter().position(|e| e.rel == p("src/b.rs")).unwrap();
        // Wide: marker and time coexist (`+1 now`).
        let wide = file_line(&app, idx, 80, false, now, false);
        assert!(
            wide.to_string().contains("+1 now"),
            "both pieces fit on a wide row: {}",
            wide.to_string()
        );
        // Narrow (14): the time goes first — recency lives in the
        // mtime sort and the cluster headers — and the uncommitted
        // signal survives.
        let narrow = file_line(&app, idx, 14, false, now, false).to_string();
        assert!(narrow.contains("+1"), "the marker survives: {narrow:?}");
        assert!(
            !(narrow.contains("now") || narrow.contains("m ago") || narrow.contains(':')),
            "the time is the first sacrifice: {narrow:?}"
        );
        // A committed row at the same width keeps the time (pre-git).
        let idx_clean = app.files.iter().position(|e| e.rel == p("a.md")).unwrap();
        let clean = file_line(&app, idx_clean, 14, false, now, false).to_string();
        assert!(
            clean.contains("now") || clean.contains("m ago") || clean.contains(':'),
            "committed rows keep the time: {clean:?}"
        );
    }

    #[test]
    fn u_key_is_a_noop_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let mut app = test_app(files::scan(dir.path(), false, false).unwrap());
        app.root = dir.path().to_path_buf();
        app.rebuild_visible(None);
        assert_eq!(app.visible_count(), 1);
        on_key(&mut app, KeyCode::Char('u'), KeyModifiers::empty(), None);
        assert!(
            !app.uncommitted_only,
            "u must not toggle outside a git repo (spec 4-3: `u` 無効)"
        );
        assert_eq!(app.visible_count(), 1, "the listing is untouched");
    }

    #[test]
    fn git_markers_refresh_after_a_rescan() {
        let Some(dir) = git_repo(&[("f.md", "one\n")]) else { return };
        let mut app = test_app(Vec::new());
        app.root = dir.path().to_path_buf();
        app.git = git::GitCache::discover(dir.path());
        app.rescan();
        let now = chrono::Local::now();
        let idx = app.files.iter().position(|e| e.rel == p("f.md")).unwrap();
        assert!(
            !file_line(&app, idx, 80, false, now, false).to_string().contains('+'),
            "clean at commit"
        );
        // The agent edits the file; the next rescan (post-Enter flow)
        // must pick the marker up.
        std::fs::write(dir.path().join("f.md"), "one\ntwo\n").unwrap();
        app.rescan();
        let idx = app.files.iter().position(|e| e.rel == p("f.md")).unwrap();
        assert!(
            file_line(&app, idx, 80, false, now, false).to_string().contains("+1"),
            "rescan refreshes the git snapshot"
        );
    }

    /// Catppuccin の本文の色（Mocha はダーク、Latte はライト）。
    const MOCHA_FG: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
    const LATTE_FG: Color = Color::Rgb(0x4c, 0x4f, 0x69);

    /// 配色の知らせのテスト用の App: `args` のフラグで、`light` から始め、
    /// プレビューに映る Markdown のファイルを 1 つ置く。
    fn scheme_app(args: &[&str], light: bool) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "plain text\n").unwrap();
        let mut app = test_app(files::scan(dir.path(), false, false).unwrap());
        app.root = dir.path().to_path_buf();
        app.config = run_config(args);
        app.set_light(light);
        app.rebuild_visible(None);
        (dir, app)
    }

    /// プレビューの本文の最初の字の色（描画済みならキャッシュから）。
    fn preview_fg(app: &mut App) -> Option<Color> {
        let lines = app.preview_lines(40, 10);
        lines.get(1)?.spans.first()?.style.fg
    }

    const CATPPUCCIN: &[&str] = &[
        "--theme-dark",
        "Catppuccin Mocha",
        "--theme-light",
        "Catppuccin Latte",
    ];

    #[test]
    fn a_color_scheme_notice_rebuilds_everything_derived_from_light() {
        let (_dir, mut app) = scheme_app(CATPPUCCIN, false);
        assert_eq!(preview_fg(&mut app), Some(MOCHA_FG));
        assert!(app.preview_cache.is_some(), "描画済み");
        // ライトになった知らせ: 構文のテーマ・UI の色・描画済みのプレビューを
        // 作り直す（プレビューのキャッシュのキーは変わらないので、残せば古い色）。
        assert!(app.follow_scheme(true));
        assert!(app.light);
        assert_eq!(app.ui_selected_bg, theme::selected_bg(true));
        assert_eq!(app.ui_border, theme::border_color(true));
        assert_eq!(app.highlight.default_fg(), LATTE_FG);
        assert_eq!(preview_fg(&mut app), Some(LATTE_FG));
        // ダークに戻った知らせ。
        assert!(app.follow_scheme(false));
        assert!(!app.light);
        assert_eq!(app.ui_selected_bg, theme::selected_bg(false));
        assert_eq!(app.ui_border, theme::border_color(false));
        assert_eq!(preview_fg(&mut app), Some(MOCHA_FG));
    }

    #[test]
    fn the_same_scheme_again_keeps_the_rendered_preview() {
        // 戻ったときの問い合わせ（CSI ? 996 n）の答えは、たいてい今と同じ。
        let (_dir, mut app) = scheme_app(CATPPUCCIN, false);
        preview_fg(&mut app);
        assert!(!app.follow_scheme(false));
        assert!(app.preview_cache.is_some(), "作り直すものは無い");
    }

    #[test]
    fn a_fixed_light_or_dark_ignores_notices() {
        for (flag, light) in [("--dark", false), ("--light", true)] {
            let mut args = vec![flag];
            args.extend_from_slice(CATPPUCCIN);
            let (_dir, mut app) = scheme_app(&args, light);
            let fg = preview_fg(&mut app);
            assert!(!app.follow_scheme(!light), "{flag}");
            assert_eq!(app.light, light, "{flag}");
            assert_eq!(app.ui_selected_bg, theme::selected_bg(light), "{flag}");
            assert_eq!(app.ui_border, theme::border_color(light), "{flag}");
            assert_eq!(preview_fg(&mut app), fg, "{flag}");
        }
    }

    #[test]
    fn one_theme_on_both_sides_still_swaps_the_ui_colors() {
        let (_dir, mut app) = scheme_app(&["--theme", "Catppuccin Mocha"], false);
        assert!(app.follow_scheme(true));
        assert_eq!(app.ui_selected_bg, theme::selected_bg(true));
        assert_eq!(app.ui_border, theme::border_color(true));
        assert_eq!(
            preview_fg(&mut app),
            Some(MOCHA_FG),
            "構文のテーマは両側とも Mocha"
        );
    }

    const SUBSCRIBE: &str = "\x1b[?2031h";
    const UNSUBSCRIBE: &str = "\x1b[?2031l";
    const QUERY_SCHEME: &str = "\x1b[?996n";
    const ALT_IN: &str = "\x1b[?1049h";
    const ALT_OUT: &str = "\x1b[?1049l";

    /// 起動 → `--open-cmd` の子に渡して戻る → Ctrl+Z で止まって `fg` で戻る →
    /// 終わる（`q`）を通し、購読の列と代替画面の出入りを書いた順に並べる。
    fn hand_off_sequence(fixed: bool) -> Vec<&'static str> {
        let buf = SharedBuf::default();
        let mut guard = buf_guard(&buf, fixed);
        guard.enter();
        guard.hand_off(leave_for_child);
        guard.take_back();
        guard.hand_off(leave_tui_modes);
        guard.take_back();
        drop(guard);
        let written = buf.take();
        let mut found: Vec<(usize, &'static str)> =
            [SUBSCRIBE, UNSUBSCRIBE, QUERY_SCHEME, ALT_IN, ALT_OUT]
                .into_iter()
                .flat_map(|seq| written.match_indices(seq).map(move |(at, _)| (at, seq)))
                .collect();
        found.sort();
        found.into_iter().map(|(_, seq)| seq).collect()
    }

    #[test]
    fn the_tui_subscribes_and_unsubscribes_around_every_hand_off() {
        assert_eq!(
            hand_off_sequence(false),
            [
                // 起動: 画面に入ってから張る。起動時の判定は OSC 11 のままで、
                // 問い合わせない。
                ALT_IN,
                SUBSCRIBE,
                // 子に渡す前に外す。
                UNSUBSCRIBE,
                ALT_OUT,
                // 戻ったら張り直してから、離れている間に変わった分を問い合わせる。
                ALT_IN,
                SUBSCRIBE,
                QUERY_SCHEME,
                // Ctrl+Z で止まる前にも外し、fg で戻ったら同じく張り直す。
                UNSUBSCRIBE,
                ALT_OUT,
                ALT_IN,
                SUBSCRIBE,
                QUERY_SCHEME,
                // 終わるとき。
                UNSUBSCRIBE,
                ALT_OUT,
            ]
        );
    }

    #[test]
    fn a_fixed_light_or_dark_never_subscribes() {
        // --light / --dark で固定したときは、どの手放しでも購読の列を書かない
        // （画面の出入りは同じ）。
        assert_eq!(
            hand_off_sequence(true),
            [ALT_IN, ALT_OUT, ALT_IN, ALT_OUT, ALT_IN, ALT_OUT]
        );
    }
}
