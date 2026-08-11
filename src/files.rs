//! File collection, sorting, time clustering, and filtering.
//!
//! The scan walks the root with the `ignore` crate (respecting `.gitignore`
//! and `.ignore` files), then applies ashiato's own always-on ignores and
//! the hidden/dirs display filters. Sort order and time clusters are pure
//! functions so they are unit-testable with a controlled "now".

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate, Timelike};
use ignore::{DirEntry, WalkBuilder};
use unicode_width::UnicodeWidthStr;

/// Directories excluded on every scan, regardless of `--show-hidden`
/// (spec: "常に除外"). Component-name match, like gitignore `name/`.
pub const DEFAULT_IGNORE_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__"];

/// File names excluded on every scan (`.DS_Store` is dot-hidden but must
/// stay excluded even with `--show-hidden`).
pub const DEFAULT_IGNORE_FILES: &[&str] = &[".DS_Store"];

/// One collected file (or directory, when dirs are shown).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the scan root ("" for the root itself).
    pub rel: PathBuf,
    /// `display_rel()` lowercased, computed once at scan time: filter
    /// matching runs against this instead of re-lowercasing every entry
    /// on each keystroke (see [`matches_prepared`]).
    pub rel_lower: String,
    /// Modified time (sort and cluster basis).
    pub mtime: SystemTime,
    /// Change time (ctime) — sort basis for `--sort ctime`. Unix-only via
    /// `MetadataExt`; falls back to mtime on other platforms.
    pub ctime: SystemTime,
    pub is_dir: bool,
    pub size: u64,
}

impl FileEntry {
    /// The relative path as a display string, with `/` separators.
    pub fn display_rel(&self) -> String {
        self.rel.to_string_lossy().replace('\\', "/")
    }
}

/// Sort order (the `t` key cycles through these; `--sort` picks the start).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    MtimeDesc,
    MtimeAsc,
    CtimeDesc,
    CtimeAsc,
}

impl Sort {
    /// The next order in the `t` cycle: mtime↓ → mtime↑ → ctime↓ → ctime↑.
    pub fn next(self) -> Self {
        match self {
            Sort::MtimeDesc => Sort::MtimeAsc,
            Sort::MtimeAsc => Sort::CtimeDesc,
            Sort::CtimeDesc => Sort::CtimeAsc,
            Sort::CtimeAsc => Sort::MtimeDesc,
        }
    }

    /// Footer badge text.
    pub fn label(self) -> &'static str {
        match self {
            Sort::MtimeDesc => "mtime↓",
            Sort::MtimeAsc => "mtime↑",
            Sort::CtimeDesc => "ctime↓",
            Sort::CtimeAsc => "ctime↑",
        }
    }

    /// Parse `--sort <name>`; `None` for unknown values (the caller
    /// reports the error instead of silently defaulting).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "mtime" => Some(Sort::MtimeDesc),
            "ctime" => Some(Sort::CtimeDesc),
            _ => None,
        }
    }
}

/// Paths whose entry differs between two listings, in `new`'s order:
/// entries added, entries whose `(mtime, size, is_dir)` changed, then —
/// appended last — entries that disappeared from the listing. The
/// away-diff uses this to accumulate external edits while the terminal
/// is unfocused.
pub fn changed_paths(old: &[FileEntry], new: &[FileEntry]) -> Vec<PathBuf> {
    let key = |e: &FileEntry| (e.mtime, e.size, e.is_dir);
    let mut seen: std::collections::BTreeMap<&Path, (SystemTime, u64, bool)> =
        old.iter().map(|e| (e.path.as_path(), key(e))).collect();
    let mut out = Vec::new();
    for e in new {
        if seen.remove(e.path.as_path()) != Some(key(e)) {
            out.push(e.path.clone());
        }
    }
    out.extend(seen.into_keys().map(|p| p.to_path_buf()));
    out
}

/// Sort `entries` in place by the given order; ties break on the relative
/// path so the order is deterministic.
pub fn sort_entries(entries: &mut [FileEntry], sort: Sort) {
    let key = |e: &FileEntry| match sort {
        Sort::MtimeDesc | Sort::MtimeAsc => e.mtime,
        Sort::CtimeDesc | Sort::CtimeAsc => e.ctime,
    };
    entries.sort_by(|a, b| {
        let ord = match sort {
            Sort::MtimeDesc | Sort::CtimeDesc => key(b).cmp(&key(a)),
            Sort::MtimeAsc | Sort::CtimeAsc => key(a).cmp(&key(b)),
        };
        ord.then_with(|| a.rel.cmp(&b.rel))
    });
}

/// The time clusters, newest first: Today / Yesterday, then **one header
/// per day**. Older days are never lumped into week/month buckets — the
/// date is the foundation you need when you come back to a file days
/// later (spec: `Wed, Aug 4` style headers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cluster {
    Today,
    Yesterday,
    /// A day before yesterday; the header shows its date.
    Date(NaiveDate),
}

impl Cluster {
    /// Header label: `Today` / `Yesterday` keep their names; older days
    /// show the date (e.g. `Wed, Aug 4`), with the year appended when it
    /// differs from `now`'s year (`Wed, Dec 3, 2025`) so a scan that
    /// crosses years stays unambiguous.
    pub fn label(self, now: chrono::DateTime<Local>) -> String {
        match self {
            Cluster::Today => "Today".to_string(),
            Cluster::Yesterday => "Yesterday".to_string(),
            Cluster::Date(d) => {
                if d.year() == now.year() {
                    d.format("%a, %b %-d").to_string()
                } else {
                    d.format("%a, %b %-d, %Y").to_string()
                }
            }
        }
    }
}

/// Which cluster `t` (a local datetime) belongs to, with boundaries
/// computed from `now`:
///
/// | label        | condition                                   |
/// |--------------|---------------------------------------------|
/// | Today        | today 00:00 onward                          |
/// | Yesterday    | yesterday 00:00 .. today 00:00              |
/// | Wed, Aug 4 … | every earlier day gets its own date header  |
pub fn cluster_of(t: chrono::DateTime<Local>, now: chrono::DateTime<Local>) -> Cluster {
    let d = t.date_naive();
    let today = now.date_naive();
    if d >= today {
        Cluster::Today
    } else if d == today - chrono::Duration::days(1) {
        Cluster::Yesterday
    } else {
        Cluster::Date(d)
    }
}

/// Convert a `SystemTime` to local time for clustering/formatting.
pub fn to_local(t: SystemTime) -> chrono::DateTime<Local> {
    t.into()
}

/// Row time display: **relative within the last hour** (`now`, `5m ago`),
/// then the plain time of day (`HH:MM`) — the cluster headers carry the
/// date context, so older rows need only the time. Relative spans are
/// elapsed-time based, never calendar words. `ago` appearing at all means
/// "touched within the hour"; beyond that `HH:MM` says exactly when, which
/// beats `3h ago` (and `2m ago` vs `3h ago` were too easy to misread).
pub fn format_time(now: chrono::DateTime<Local>, t: chrono::DateTime<Local>) -> String {
    let elapsed = now.signed_duration_since(t);
    if elapsed < chrono::Duration::seconds(60) {
        // Also covers future timestamps (clock skew): clamp to "now".
        "now".to_string()
    } else if elapsed < chrono::Duration::minutes(60) {
        format!("{}m ago", elapsed.num_minutes())
    } else {
        format!("{:02}:{:02}", t.hour(), t.minute())
    }
}

/// Whether `t` falls in [`format_time`]'s relative window (`now`/`Nm ago`,
/// i.e. touched within the hour) — the rows whose time gets the fresh
/// accent color. Kept next to `format_time` so the two share one boundary.
pub fn is_fresh(now: chrono::DateTime<Local>, t: chrono::DateTime<Local>) -> bool {
    now.signed_duration_since(t) < chrono::Duration::minutes(60)
}

/// Case-insensitive substring match on the relative path. A leading `/` in
/// the filter is stripped so the spec's examples work: `/main.rs` matches
/// `src/main.rs`, `/src/` matches `src/…` paths, `/.md` matches `.md`
/// files (spec: パスのどの位置でもマッチ).
/// (One-shot form: the hot paths prepare the needle once and call
/// [`matches_prepared`] per entry, which searches the entry's scan-time
/// lowercased path; this stays as the simple API and the unit tests'
/// entry point.)
#[cfg_attr(not(test), allow(dead_code))]
pub fn matches_filter(entry: &FileEntry, filter: &str) -> bool {
    matches_prepared(entry, &prepare_filter(filter))
}

/// Normalize a filter once (leading `/` stripped, lowercased) so a list
/// rebuild lowercases the needle once, not once per entry.
pub fn prepare_filter(filter: &str) -> String {
    filter.strip_prefix('/').unwrap_or(filter).to_lowercase()
}

/// Match against a [`prepare_filter`]-normalized needle. The entry side
/// is already lowercased ([`FileEntry::rel_lower`], computed once at
/// scan time), so a list rebuild pays one lowercase pass on the needle
/// and none per entry — the old per-entry `display_rel().to_lowercase()`
/// alone cost hundreds of milliseconds per keystroke on 100k-file trees.
pub fn matches_prepared(entry: &FileEntry, needle: &str) -> bool {
    entry.rel_lower.contains(needle)
}

/// Whether a walk entry should be excluded by the always-on ignores or the
/// hidden filter. Hidden = dot-*directories* (e.g. `.claude/`, `.github/`),
/// not dot-files: the spec's example layout shows `.gitignore` in the
/// default listing, and `.DS_Store` is in the always-on ignore list
/// precisely because dot-files are not hidden by the toggle. `.claude/`,
/// `.codex/`, `.pi/` are never in the always-on ignore list — they are
/// only hidden by the display toggle (spec: agent files are review targets).
fn entry_filtered(entry: &DirEntry, root: &Path, show_hidden: bool) -> bool {
    let rel = match entry.path().strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return true,
    };
    let is_dir = entry.file_type().map_or(false, |t| t.is_dir());
    let comps: Vec<std::ffi::OsString> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            _ => None,
        })
        .collect();
    for (i, comp) in comps.iter().enumerate() {
        let name = comp.to_string_lossy();
        if DEFAULT_IGNORE_DIRS.contains(&name.as_ref()) {
            return true;
        }
        // Intermediate components are always directories; the last one is
        // a directory only when the entry itself is.
        let comp_is_dir = is_dir || i + 1 < comps.len();
        if !show_hidden && comp_is_dir && name.starts_with('.') {
            return true;
        }
    }
    if !is_dir {
        let name = entry.file_name().to_string_lossy();
        if DEFAULT_IGNORE_FILES.contains(&name.as_ref()) {
            return true;
        }
    }
    false
}

/// Scan `root` recursively: `.gitignore`/`.ignore` respected, the always-on
/// ignores applied, and hidden entries (or directories) dropped per the
/// display flags. Does not follow directory symlinks. Errors on individual
/// entries (unreadable dirs, races) are skipped; only a missing root is fatal.
pub fn scan(root: &Path, show_hidden: bool, show_dirs: bool) -> Result<Vec<FileEntry>> {
    let meta = std::fs::metadata(root)
        .with_context(|| format!("scanning {}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }
    let mut builder = WalkBuilder::new(root);
    let root_owned = root.to_path_buf();
    builder
        .standard_filters(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        // Apply .gitignore even when the root is not inside a git repo
        // (ignore crate's default requires git discovery).
        .require_git(false)
        .hidden(false) // handled by entry_filtered (hidden is a display toggle)
        .filter_entry(move |e| !entry_filtered(e, &root_owned, show_hidden));
    let mut out = Vec::new();
    for result in builder.build() {
        let Ok(entry) = result else { continue };
        let Some(ft) = entry.file_type() else { continue };
        if ft.is_dir() && !show_dirs {
            continue;
        }
        let path = entry.path().to_path_buf();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_path_buf();
        // The root itself is not a list entry.
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Lowercase the search string once here: matches_prepared runs on
        // every filter keystroke, and re-lowercasing per entry per rebuild
        // is what made big trees slow (the needle side is already prepared
        // once per rebuild by prepare_filter).
        let rel_lower = rel.to_string_lossy().replace('\\', "/").to_lowercase();
        out.push(FileEntry {
            mtime: meta.modified().unwrap_or(UNIX_EPOCH),
            ctime: ctime_of(&meta),
            path,
            rel,
            rel_lower,
            is_dir: ft.is_dir(),
            size: meta.len(),
        });
    }
    Ok(out)
}

/// ctime as a `SystemTime` (Unix `stat.ctime`, whole seconds). Non-Unix
/// platforms fall back to mtime.
#[cfg(unix)]
fn ctime_of(meta: &std::fs::Metadata) -> SystemTime {
    use std::os::unix::fs::MetadataExt;
    let secs = meta.ctime().max(0) as u64;
    UNIX_EPOCH + std::time::Duration::from_secs(secs)
}

#[cfg(not(unix))]
fn ctime_of(meta: &std::fs::Metadata) -> SystemTime {
    meta.modified().unwrap_or(UNIX_EPOCH)
}

/// The display width of a filename row's left part (emoji + rel path).
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDateTime, TimeZone};
    use std::time::Duration;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::DateTime<Local> {
        let dt = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            chrono::NaiveTime::from_hms_opt(h, min, 0).unwrap(),
        );
        Local.from_local_datetime(&dt).single().unwrap()
    }

    /// A fixed "now": Wed 2026-08-05 14:00. Monday of that week is Aug 3.
    fn now() -> chrono::DateTime<Local> {
        local(2026, 8, 5, 14, 0)
    }

    #[test]
    fn cluster_is_per_day() {
        let n = now(); // Wed 2026-08-05
        assert_eq!(cluster_of(local(2026, 8, 5, 0, 1), n), Cluster::Today);
        assert_eq!(cluster_of(local(2026, 8, 4, 23, 59), n), Cluster::Yesterday);
        assert_eq!(cluster_of(local(2026, 8, 4, 0, 0), n), Cluster::Yesterday);
        // Every earlier day gets its own date cluster, however far back.
        let d = |y: i32, m: u32, day: u32| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(
            cluster_of(local(2026, 8, 3, 12, 0), n),
            Cluster::Date(d(2026, 8, 3))
        );
        assert_eq!(
            cluster_of(local(2026, 7, 31, 0, 0), n),
            Cluster::Date(d(2026, 7, 31))
        );
        assert_eq!(
            cluster_of(local(2025, 12, 3, 0, 0), n),
            Cluster::Date(d(2025, 12, 3))
        );
    }

    #[test]
    fn cluster_label_keeps_today_yesterday_and_shows_dates() {
        let n = now();
        assert_eq!(Cluster::Today.label(n), "Today");
        assert_eq!(Cluster::Yesterday.label(n), "Yesterday");
        let d = |y: i32, m: u32, day: u32| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // Same year as now: no year in the header.
        assert_eq!(Cluster::Date(d(2026, 8, 3)).label(n), "Mon, Aug 3");
        // Different year: year appended so the header stays unambiguous.
        assert_eq!(Cluster::Date(d(2025, 12, 3)).label(n), "Wed, Dec 3, 2025");
    }

    #[test]
    fn format_time_is_relative_within_1h_then_hhmm() {
        let n = now(); // 2026-08-05 14:00:00
        // < 60s: "now" (also the clamp for future/clock-skew timestamps).
        assert_eq!(format_time(n, n - chrono::Duration::seconds(59)), "now");
        assert_eq!(format_time(n, n + chrono::Duration::hours(1)), "now");
        // 1–59 minutes.
        assert_eq!(format_time(n, n - chrono::Duration::seconds(60)), "1m ago");
        assert_eq!(format_time(n, n - chrono::Duration::minutes(5)), "5m ago");
        assert_eq!(format_time(n, n - chrono::Duration::minutes(59)), "59m ago");
        // ≥ 1h: HH:MM — the cluster headers carry the date context, and
        // "ago" appearing at all means "within the hour".
        assert_eq!(format_time(n, n - chrono::Duration::hours(1)), "13:00");
        assert_eq!(format_time(n, n - chrono::Duration::hours(23)), "15:00");
        assert_eq!(format_time(n, n - chrono::Duration::hours(24)), "14:00");
        assert_eq!(format_time(n, local(2026, 8, 3, 9, 5)), "09:05");
    }

    #[test]
    fn is_fresh_matches_format_time_relative_window() {
        let n = now();
        // The fresh window is exactly the now/Nm-ago window: < 60 minutes
        // elapsed, future timestamps (clock skew) included.
        assert!(is_fresh(n, n + chrono::Duration::hours(1)));
        assert!(is_fresh(n, n - chrono::Duration::minutes(59)));
        assert!(!is_fresh(n, n - chrono::Duration::minutes(60)));
        assert!(!is_fresh(n, n - chrono::Duration::hours(24)));
    }

    #[test]
    fn sort_cycle_is_mtime_desc_asc_ctime_desc_asc() {
        let mut s = Sort::MtimeDesc;
        assert_eq!(s.label(), "mtime↓");
        s = s.next();
        assert_eq!(s.label(), "mtime↑");
        s = s.next();
        assert_eq!(s.label(), "ctime↓");
        s = s.next();
        assert_eq!(s.label(), "ctime↑");
        s = s.next();
        assert_eq!(s.label(), "mtime↓");
    }

    fn entry(rel: &str, mtime: SystemTime) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/root").join(rel),
            rel: PathBuf::from(rel),
            rel_lower: rel.to_lowercase(),
            mtime,
            ctime: mtime,
            is_dir: false,
            size: 0,
        }
    }

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn changed_paths_lists_adds_edits_and_deletions() {
        // edit b.md (mtime bump), add d.md, delete a.md.
        let old = vec![entry("a.md", t(10)), entry("b.md", t(10)), entry("c.md", t(10))];
        let new = vec![
            entry("b.md", t(20)),
            entry("c.md", t(10)),
            entry("d.md", t(5)),
        ];
        let binding = changed_paths(&old, &new);
        let names: Vec<&str> = binding
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        // New-list order, deletions appended last.
        assert_eq!(names, vec!["/root/b.md", "/root/d.md", "/root/a.md"]);
        // No changes at all → empty.
        assert!(changed_paths(&old, &old).is_empty());
        assert!(changed_paths(&new, &new).is_empty());
    }

    #[test]
    fn sort_orders_by_time_then_path() {
        let mut v = vec![
            entry("b.md", t(10)),
            entry("a.md", t(20)),
            entry("c.md", t(10)),
        ];
        sort_entries(&mut v, Sort::MtimeDesc);
        let names: Vec<&str> = v.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, vec!["a.md", "b.md", "c.md"]); // 20, 10, 10 (b<c tie)
        sort_entries(&mut v, Sort::MtimeAsc);
        let names: Vec<&str> = v.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, vec!["b.md", "c.md", "a.md"]);
    }

    #[test]
    fn filter_matches_anywhere_in_the_relative_path() {
        assert!(matches_filter(&entry("src/main.rs", t(1)), "main.rs"));
        assert!(matches_filter(&entry("src/main.rs", t(1)), "/src/"));
        assert!(matches_filter(&entry("src/main.rs", t(1)), "src/"));
        assert!(matches_filter(&entry("src/main.rs", t(1)), "/main.rs"));
        assert!(matches_filter(&entry("src/main.rs", t(1)), ".rs"));
        assert!(matches_filter(&entry("testdata/full.md", t(1)), ".md"));
        assert!(matches_filter(&entry("testdata/full.md", t(1)), "/.md"));
        assert!(matches_filter(&entry("src/Main.rs", t(1)), "main")); // case-insensitive
        assert!(matches_filter(&entry("src/main.rs", t(1)), "/")); // bare slash = all
        assert!(!matches_filter(&entry("src/main.rs", t(1)), "toml"));
        assert!(matches_filter(&entry("a/b/c", t(1)), "")); // empty = all
    }

    #[test]
    fn filter_matches_mixed_case_paths() {
        let e = entry("Src/Util/Main.RS", t(1));
        assert!(matches_filter(&e, "main.rs"));
        assert!(matches_filter(&e, "/SRC/"));
        assert!(matches_filter(&e, "util"));
        assert!(matches_filter(&e, "s")); // any letter, upper or lower
        assert!(!matches_filter(&e, "toml"));
    }

    #[test]
    fn filter_lowercases_unicode_paths() {
        // Lowercasing is Unicode-aware: umlauts and accented letters
        // match their lowercase needle, in either direction.
        assert!(matches_filter(&entry("docs/Märchen.md", t(1)), "märchen"));
        assert!(matches_filter(&entry("CAFÉ.txt", t(1)), "café"));
        assert!(matches_filter(&entry("café.txt", t(1)), "CAFÉ"));
        // Greek: uppercase Σ lowercases to σ, so the lowercase needle
        // matches the uppercase path.
        assert!(matches_filter(&entry("Σίσυφος.txt", t(1)), "σίσυφος"));
        // İ (I with dot above) expands to two chars when lowercased
        // (i + combining dot): the match needs the expanded form.
        assert!(matches_filter(&entry("\u{130}stanbul.txt", t(1)), "i\u{307}stanbul"));
        assert!(!matches_filter(&entry("\u{130}stanbul.txt", t(1)), "istanbul"));
    }

    /// Build a temp tree and scan it, asserting which entries survive.
    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in [
            "src/main.rs",
            "src/util/mod.rs",
            "README.md",
            ".gitignore",
            ".git/config",          // always ignored
            "target/debug/x",       // always ignored
            "node_modules/pkg/x.js", // always ignored
            "__pycache__/x.pyc",    // always ignored
            ".claude/settings.json", // hidden by default, never always-ignored
            ".pi/agent.md",          // hidden by default, never always-ignored
            "sub/.DS_Store",         // always ignored even when hidden shown
            "docs/design.md",
        ] {
            let p = dir.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        dir
    }

    #[test]
    fn scan_applies_always_on_ignores_and_hidden_toggle() {
        let dir = make_tree();
        let root = dir.path();
        let hidden_off = scan(root, false, true)
            .unwrap()
            .into_iter()
            .map(|e| e.display_rel())
            .collect::<Vec<_>>();
        for name in [
            "src/main.rs",
            "src/util/mod.rs",
            "README.md",
            "docs/design.md",
            ".gitignore", // dot-FILES are shown even with hidden off (spec layout)
        ] {
            assert!(hidden_off.contains(&name.to_string()), "missing {name}");
        }
        for name in [
            ".git/config",
            "target/debug/x",
            "node_modules/pkg/x.js",
            "__pycache__/x.pyc",
            ".claude/settings.json",
            ".pi/agent.md",
            "sub/.DS_Store",
        ] {
            assert!(!hidden_off.contains(&name.to_string()), "leaked {name}");
        }
        // Hidden shown: .claude / .pi come back, always-ignores stay gone.
        let hidden_on = scan(root, true, true)
            .unwrap()
            .into_iter()
            .map(|e| e.display_rel())
            .collect::<Vec<_>>();
        assert!(hidden_on.contains(&".claude/settings.json".to_string()));
        assert!(hidden_on.contains(&".pi/agent.md".to_string()));
        assert!(!hidden_on.contains(&".git/config".to_string()));
        assert!(!hidden_on.contains(&"target/debug/x".to_string()));
        assert!(!hidden_on.contains(&"sub/.DS_Store".to_string()));
    }

    #[test]
    fn scan_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
        std::fs::write(dir.path().join("kept.txt"), "x").unwrap();
        let names: Vec<String> = scan(dir.path(), true, false)
            .unwrap()
            .into_iter()
            .map(|e| e.display_rel())
            .collect();
        assert!(names.contains(&"kept.txt".to_string()));
        assert!(!names.contains(&"ignored.txt".to_string()));
        // .gitignore itself is a dot-FILE: shown even with hidden on.
        assert!(names.contains(&".gitignore".to_string()));
    }

    #[test]
    fn scan_dirs_toggle() {
        let dir = make_tree();
        let files_only = scan(dir.path(), true, false)
            .unwrap()
            .into_iter()
            .map(|e| e.display_rel())
            .collect::<Vec<_>>();
        assert!(!files_only.contains(&"src".to_string()));
        let with_dirs = scan(dir.path(), true, true)
            .unwrap()
            .into_iter()
            .map(|e| e.display_rel())
            .collect::<Vec<_>>();
        assert!(with_dirs.contains(&"src".to_string()));
        assert!(with_dirs.contains(&"src/util".to_string()));
    }

    #[test]
    fn scan_precomputes_the_lowercased_search_string() {
        let dir = make_tree();
        let entries = scan(dir.path(), false, true).unwrap();
        assert!(!entries.is_empty());
        for e in &entries {
            // rel_lower is exactly the display path lowercased once at
            // scan time — the string matches_prepared searches.
            assert_eq!(e.rel_lower, e.display_rel().to_lowercase());
        }
        // And the precomputed string drives filtering.
        assert!(entries.iter().any(|e| matches_prepared(e, "main.rs")));
    }

    #[test]
    fn scan_missing_root_errors() {
        assert!(scan(Path::new("/nonexistent/ashiato-test"), true, false).is_err());
    }

    #[test]
    fn scan_single_file_root_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(scan(&f, true, false).is_err());
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(matches_filter(&entry("anything", t(1)), ""));
    }
}
