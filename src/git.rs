//! git integration (git-integration-spec.md §4) — a dormant add-on.
//!
//! Active only when the scan root sits inside a git work tree: rows for
//! files with uncommitted changes get a `+N -M` marker — the change
//! scale and the "uncommitted" signal fused into one right-side element
//! (4-1) — and `u` filters the listing to those files (4-2). Outside a
//! repo — or with git missing from PATH — [`GitCache::discover`]
//! yields `None` and the TUI behaves exactly as before (P1).
//!
//! Cost (spec §5): the every-2s refresh tick spawns git only when the
//! filesystem listing actually changed since the last query
//! (signature-gated). A stable tree never pays a subprocess; the
//! staleness that creates is explicitly tolerated (P4, snapshot
//! approach). `--untracked-files=normal` keeps the status output small
//! (untracked dirs arrive as one `?? dir/` entry, expanded against the
//! listing), and untracked line counts read the file once per query
//! with a size cap. Line counts come from `git diff --numstat`; binary
//! numstats (`-`) render as a bare `-` (spec §5).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::files::FileEntry;

/// Untracked files larger than this are not line-counted (counting is
/// a full read; a multi-MB log must not stall the TUI) — they show the
/// binary marker `-`, like a binary numstat.
const MAX_COUNTED_SIZE: u64 = 2 * 1024 * 1024;
/// NUL within the first bytes marks a file binary (the preview's
/// reading heuristic, reused here for counting).
const BINARY_PROBE: usize = 8192;

/// A file's uncommitted-change status (its right-side row marker).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitStatus {
    /// Line counts (numstat). Untracked files carry their own line
    /// count as pure additions.
    Diff { added: u64, deleted: u64 },
    /// Binary (numstat `-`), or an untracked file too large / binary
    /// to count: the marker is a bare `-`.
    Binary,
}

impl GitStatus {
    /// The row marker: `+N -M` with zero sides dropped, `-` for
    /// binary. `+0` covers mode-only changes (numstat `0 0`).
    pub fn marker(self) -> String {
        match self {
            GitStatus::Diff { added: 0, deleted: 0 } => "+0".to_string(),
            GitStatus::Diff { added: 0, deleted } => format!("-{deleted}"),
            GitStatus::Diff { added, deleted: 0 } => format!("+{added}"),
            GitStatus::Diff { added, deleted } => format!("+{added} -{deleted}"),
            GitStatus::Binary => "-".to_string(),
        }
    }
}

/// Signature of one listing entry: `(path, mtime, size, is_dir)` — the
/// away-diff's triple plus the path. When no component changed since
/// the last query, `git status` output cannot have changed (mode-only
/// edits are the known blind spot, shared with the away-diff, and
/// tolerable under P4's snapshot principle).
#[derive(Clone, PartialEq, Eq, Hash)]
struct FileSig {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    is_dir: bool,
}

impl FileSig {
    fn of(e: &FileEntry) -> Self {
        FileSig { path: e.path.clone(), mtime: e.mtime, size: e.size, is_dir: e.is_dir }
    }
}

/// The git snapshot behind the feature. One instance per App; `None`
/// (App side) outside a work tree.
pub struct GitCache {
    /// Work-tree top (`git rev-parse --show-toplevel`), canonical.
    repo_root: PathBuf,
    /// Repo-relative path → uncommitted status (tracked + untracked).
    markers: HashMap<PathBuf, GitStatus>,
    /// Listing signature at the last query; `None` = never queried.
    sig: Option<HashSet<FileSig>>,
}

impl GitCache {
    /// Detect the work tree containing `root` (one cheap `git
    /// rev-parse`). `None` when root is not inside a repo or git is
    /// missing — the caller keeps the feature dormant (P1).
    pub fn discover(root: &Path) -> Option<GitCache> {
        // rev-parse 自体はロックを取らないが、git_output と同じ方針で
        // 「ashiato の git は一切ロックを作らない」を不変条件にしておく。
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--show-toplevel"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let top = String::from_utf8(out.stdout).ok()?;
        // git resolves symlinks (on macOS `/var` → `/private/var`);
        // normalize so the prefix match against the scan root's path
        // form cannot miss.
        let repo_root =
            std::fs::canonicalize(top.trim()).unwrap_or_else(|_| PathBuf::from(top.trim()));
        Some(GitCache {
            repo_root,
            markers: HashMap::new(),
            sig: None,
        })
    }

    /// Re-query when `files` changed since the last query: the 2s tick
    /// that finds the tree unchanged never spawns git (spec §5's cache
    /// strategy). A listing that keeps changing (an agent writing)
    /// re-queries per scan — that is the active-work case where fresh
    /// markers are wanted.
    pub fn refresh_if_listing_changed(&mut self, files: &[FileEntry]) {
        let sig: HashSet<FileSig> = files.iter().map(FileSig::of).collect();
        if self.sig.as_ref() != Some(&sig) {
            self.query(files);
            self.sig = Some(sig);
        }
    }

    /// The uncommitted status of an absolute path, if any.
    pub fn marker_for(&self, path: &Path) -> Option<GitStatus> {
        let rel = path.strip_prefix(&self.repo_root).ok()?;
        self.markers.get(rel).copied()
    }

    /// One snapshot: `status --porcelain` (which files are uncommitted,
    /// and their untracked classification) + `diff --numstat HEAD`
    /// (line counts; `--no-renames` keeps one path per entry — a rename
    /// reads as the new file's addition). Both tolerate failure (an
    /// unborn HEAD, a missing git, …): the status side still marks
    /// files, and missing numstat entries fall back to `+0`.
    fn query(&mut self, files: &[FileEntry]) {
        let mut markers = HashMap::new();
        let mut untracked_dirs = Vec::new();
        let mut untracked_files = Vec::new();
        if let Some(out) = git_output(
            &self.repo_root,
            &["status", "--porcelain", "-z", "--untracked-files=normal"],
        ) {
            parse_status(&out, &mut markers, &mut untracked_dirs, &mut untracked_files);
        }
        if let Some(out) = git_output(
            &self.repo_root,
            &["diff", "--numstat", "--no-renames", "-z", "HEAD"],
        ) {
            for (path, st) in parse_numstat(&out) {
                markers.insert(path, st);
            }
        }
        for path in &untracked_files {
            markers.insert(path.clone(), untracked_status(&self.repo_root.join(path)));
        }
        // `?? dir/` covers every file under it (normal mode); expand
        // against the listing so each row gets its own marker.
        for dir in &untracked_dirs {
            for e in files {
                if let Some(rel) = e
                    .path
                    .strip_prefix(&self.repo_root)
                    .ok()
                    .filter(|rel| rel.starts_with(dir))
                {
                    markers.insert(rel.to_path_buf(), untracked_status(&e.path));
                }
            }
        }
        self.markers = markers;
    }
}

/// Run `git -C root <args>`; `None` on spawn failure or non-zero exit
/// (callers treat both as "no data", e.g. `git diff HEAD` in a repo
/// with an unborn HEAD).
///
/// `GIT_OPTIONAL_LOCKS=0`: `status` / `diff` は読み取りに見えて、stat が
/// 古い index を opportunistic refresh で書き戻す（= `.git/index.lock` を
/// 取る）。ashiato は 2 秒ごとの裏方ポーリングなので、その最中に kill
/// されると 0 バイトの index.lock が残留し、同じチェックアウトの git を
/// 全部止めてしまう（2026-08-13 に実害を確認）。この変数は git 2.15 で
/// まさにバックグラウンドツール向けに入ったもので、refresh 書き戻しだけ
/// をやめる — 出力は変わらず、ロックを一切作らなくなる。
fn git_output(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Parse `git status --porcelain -z`: fields are `XY path`, and a
/// rename/copy (`R`/`C`) carries the old path as its own next field.
/// Tracked changes get a `Diff{0,0}` placeholder that `query` overwrites
/// from the numstat; untracked files and dirs (`?? dir/`, trailing
/// slash) are collected for the line-count pass.
fn parse_status(
    out: &[u8],
    markers: &mut HashMap<PathBuf, GitStatus>,
    untracked_dirs: &mut Vec<PathBuf>,
    untracked_files: &mut Vec<PathBuf>,
) {
    let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue; // "XY p" minimum; malformed entries are skipped
        }
        let (x, y) = (field[0], field[1]);
        let mut raw = String::from_utf8_lossy(&field[3..]).into_owned();
        if x == b'?' {
            if raw.ends_with('/') {
                raw.pop();
                untracked_dirs.push(PathBuf::from(raw));
            } else {
                untracked_files.push(PathBuf::from(raw));
            }
        } else if x != b' ' || y != b' ' {
            markers.insert(PathBuf::from(raw), GitStatus::Diff { added: 0, deleted: 0 });
        }
        if x == b'R' || x == b'C' || y == b'R' || y == b'C' {
            let _ = fields.next(); // the old path
        }
    }
}

/// Parse `git diff --numstat --no-renames -z HEAD`: fields are
/// `added<TAB>deleted<TAB>path`, with `-` in a count meaning binary.
fn parse_numstat(out: &[u8]) -> HashMap<PathBuf, GitStatus> {
    let mut map = HashMap::new();
    for field in out.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let mut it = field.split(|&b| b == b'\t');
        let (Some(added), Some(deleted), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let pb = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        let is_dash = |c: &[u8]| c.len() == 1 && c[0] == b'-';
        let st = if is_dash(added) || is_dash(deleted) {
            GitStatus::Binary
        } else {
            let parse = |c: &[u8]| {
                std::str::from_utf8(c).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
            };
            GitStatus::Diff { added: parse(added), deleted: parse(deleted) }
        };
        map.insert(pb, st);
    }
    map
}

/// The marker for an untracked file: its own line count as pure
/// additions (git gives no numstat for `??` files, and `git add -N`
/// would pollute the user's index). Binary or too-large files read as
/// `-`, matching the numstat convention.
fn untracked_status(path: &Path) -> GitStatus {
    let Ok(meta) = std::fs::metadata(path) else { return GitStatus::Binary };
    if meta.len() > MAX_COUNTED_SIZE {
        return GitStatus::Binary;
    }
    let Ok(bytes) = std::fs::read(path) else { return GitStatus::Binary };
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return GitStatus::Binary;
    }
    // git counts a final unterminated line as a line (verified against
    // `git diff --numstat` for no-trailing-newline files).
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
    let trailing = u64::from(!bytes.is_empty() && *bytes.last().unwrap() != b'\n');
    GitStatus::Diff { added: newlines + trailing, deleted: 0 }
}

/// Whether the `git` binary answers — the integration tests skip (not
/// fail) on gitless machines.
#[cfg(test)]
pub(crate) fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// Run `git -C dir <args>`, panicking on failure — the integration
/// tests' command runner.
#[cfg(test)]
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files;

    fn entry(path: &Path) -> FileEntry {
        let meta = std::fs::metadata(path).unwrap();
        FileEntry {
            path: path.to_path_buf(),
            rel: path.file_name().unwrap().into(),
            rel_lower: path.file_name().unwrap().to_string_lossy().to_lowercase(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ctime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            is_dir: meta.is_dir(),
            size: meta.len(),
        }
    }

    /// Scan a temp repo the way the App does.
    fn entries(dir: &Path) -> Vec<FileEntry> {
        files::scan(dir, false, false).unwrap()
    }

    /// A temp dir under the *canonical* temp root: `git rev-parse`
    /// resolves symlinks (macOS `/var` → `/private/var`), so a repo
    /// under the unresolved path would mismatch the prefix match in
    /// [`GitCache::marker_for`].
    fn tempdir() -> tempfile::TempDir {
        let base = std::env::temp_dir().canonicalize().unwrap_or_else(|_| std::env::temp_dir());
        tempfile::Builder::new().tempdir_in(&base).unwrap()
    }

    /// A temp git repo with one commit; `None` when git is missing.
    fn repo(files: &[(&str, &str)]) -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }
        let dir = tempdir();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@t"]);
        git(dir.path(), &["config", "user.name", "t"]);
        for (name, content) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "init"]);
        Some(dir)
    }

    #[test]
    fn marker_formats() {
        let mk = |a, d| GitStatus::Diff { added: a, deleted: d };
        assert_eq!(mk(3, 1).marker(), "+3 -1");
        assert_eq!(mk(3, 0).marker(), "+3");
        assert_eq!(mk(0, 2).marker(), "-2");
        assert_eq!(mk(0, 0).marker(), "+0"); // mode-only change
        assert_eq!(GitStatus::Binary.marker(), "-");
    }

    #[test]
    fn discover_is_none_outside_a_repo() {
        let dir = tempdir();
        assert!(GitCache::discover(dir.path()).is_none());
        // A nonexistent root also yields None, never a panic.
        assert!(GitCache::discover(Path::new("/nonexistent/ashiato-git-test")).is_none());
    }

    #[test]
    fn discover_finds_the_repo_top_from_a_subdir() {
        let Some(dir) = repo(&[("a.txt", "x\n")]) else { return };
        let sub = dir.path().join("deep/nested");
        std::fs::create_dir_all(&sub).unwrap();
        let cache = GitCache::discover(&sub).unwrap();
        assert_eq!(cache.repo_root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn markers_cover_tracked_staged_untracked_and_binary() {
        let Some(dir) = repo(&[
            ("mod.txt", "one\ntwo\n"),
            ("staged.txt", "one\n"),
            ("bin.dat", "\0\x01\x02"),
            ("clean.txt", "keep\n"),
        ]) else {
            return;
        };
        // Unstaged edit (+1), staged edit (+1), binary edit, untracked
        // file, untracked dir.
        std::fs::write(dir.path().join("mod.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join("staged.txt"), "one\ntwo\n").unwrap();
        git(dir.path(), &["add", "staged.txt"]);
        std::fs::write(dir.path().join("bin.dat"), "\0\x01\x02\x03").unwrap();
        std::fs::write(dir.path().join("new.txt"), "a\nb\n").unwrap();
        std::fs::create_dir_all(dir.path().join("ndir")).unwrap();
        std::fs::write(dir.path().join("ndir/f.txt"), "x\n").unwrap();

        let mut cache = GitCache::discover(dir.path()).unwrap();
        cache.refresh_if_listing_changed(&entries(dir.path()));

        let m = |name: &str| cache.marker_for(&dir.path().join(name)).map(|s| s.marker());
        assert_eq!(m("mod.txt").as_deref(), Some("+1"));
        assert_eq!(m("staged.txt").as_deref(), Some("+1"), "staged-only changes count");
        assert_eq!(m("new.txt").as_deref(), Some("+2"), "untracked = its own line count");
        assert_eq!(m("ndir/f.txt").as_deref(), Some("+1"), "?? dir/ expands per file");
        assert_eq!(m("bin.dat").as_deref(), Some("-"), "binary numstat");
        assert_eq!(m("clean.txt"), None, "committed files have no marker");
    }

    #[cfg(unix)]
    #[test]
    fn mode_only_changes_show_plus_zero() {
        use std::os::unix::fs::PermissionsExt;
        let Some(dir) = repo(&[("f.txt", "x\n")]) else { return };
        // chmod does not touch mtime, so the listing signature stays
        // put — the mode change still surfaces via the first query.
        std::fs::set_permissions(
            dir.path().join("f.txt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut cache = GitCache::discover(dir.path()).unwrap();
        cache.refresh_if_listing_changed(&entries(dir.path()));
        assert_eq!(
            cache.marker_for(&dir.path().join("f.txt")).map(|s| s.marker()).as_deref(),
            Some("+0")
        );
    }

    #[test]
    fn unborn_head_marks_everything_untracked() {
        if !git_available() {
            return;
        }
        let dir = tempdir();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("u.txt"), "a\nb\n").unwrap();
        let mut cache = GitCache::discover(dir.path()).unwrap();
        cache.refresh_if_listing_changed(&entries(dir.path()));
        assert_eq!(
            cache.marker_for(&dir.path().join("u.txt")).map(|s| s.marker()).as_deref(),
            Some("+2"),
            "no HEAD: every file is untracked"
        );
    }

    #[test]
    fn refresh_is_gated_on_listing_changes() {
        let Some(dir) = repo(&[("f.txt", "one\n")]) else { return };
        let mut cache = GitCache::discover(dir.path()).unwrap();
        let files = entries(dir.path());
        cache.refresh_if_listing_changed(&files);
        assert_eq!(cache.marker_for(&dir.path().join("f.txt")), None, "clean at commit");
        // Edit the file on disk but keep feeding the OLD listing: the
        // signature gate must suppress the query — no git spawn on a
        // stable listing (spec §5), stale is fine (P4).
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        cache.refresh_if_listing_changed(&files);
        assert_eq!(
            cache.marker_for(&dir.path().join("f.txt")),
            None,
            "an unchanged listing must not re-query git"
        );
        // A fresh listing changes the signature → the query runs and
        // the marker appears.
        cache.refresh_if_listing_changed(&entries(dir.path()));
        assert_eq!(
            cache.marker_for(&dir.path().join("f.txt")).map(|s| s.marker()).as_deref(),
            Some("+2")
        );
    }

    #[test]
    fn parse_status_skips_rename_old_paths() {
        let mut markers = HashMap::new();
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        parse_status(
            b"R  new.txt\0old.txt\0 M mod.txt\0",
            &mut markers,
            &mut dirs,
            &mut files,
        );
        assert_eq!(markers.len(), 2, "new.txt + mod.txt; old.txt consumed");
        assert!(markers.contains_key(Path::new("new.txt")));
        assert!(markers.contains_key(Path::new("mod.txt")));
        assert!(!markers.contains_key(Path::new("old.txt")));
        assert!(dirs.is_empty() && files.is_empty());
    }

    #[test]
    fn parse_status_classifies_untracked_files_and_dirs() {
        let mut markers = HashMap::new();
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        parse_status(
            b"?? new.txt\0?? ndir/\0 M mod.txt\0",
            &mut markers,
            &mut dirs,
            &mut files,
        );
        assert_eq!(files, vec![PathBuf::from("new.txt")]);
        assert_eq!(dirs, vec![PathBuf::from("ndir")]);
        assert_eq!(markers.len(), 1);
        assert!(markers.contains_key(Path::new("mod.txt")));
    }

    #[test]
    fn untracked_status_counts_lines_like_git() {
        let dir = tempdir();
        let f = |name: &str, content: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, content).unwrap();
            p
        };
        let a = f("a.txt", b"x\ny\n");
        let b = f("b.txt", b"x\ny");
        let c = f("c.txt", b"");
        let d = f("d.bin", b"\x00\x01");
        assert_eq!(untracked_status(&a), GitStatus::Diff { added: 2, deleted: 0 });
        assert_eq!(
            untracked_status(&b),
            GitStatus::Diff { added: 2, deleted: 0 },
            "a final unterminated line counts (git's own numstat rule)"
        );
        assert_eq!(untracked_status(&c), GitStatus::Diff { added: 0, deleted: 0 });
        assert_eq!(untracked_status(&d), GitStatus::Binary);
        // Missing/unreadable → Binary (the marker still says "uncommitted").
        assert_eq!(untracked_status(&dir.path().join("nope.txt")), GitStatus::Binary);
    }
}
