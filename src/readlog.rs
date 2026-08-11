//! エージェント（Claude Code / pi）のセッションログから Read/Edit を抽出する。
//!
//! **スタブ実装**。本実装は別 worktree で並行開発中で、マージ時に差し替わる。
//! 公開 API は契約で固定されており、呼び出し側（main.rs / files.rs）は
//! このモジュールの下記関数・型だけを使う（FOLD_WINDOW はパーサー内部の
//! 畳み込み定数で、UI 側は触らない）。
//!
//! 扱うログ形式は 2 つ（ログの置き場でバックエンドを判別する）:
//! - Claude Code: `~/.claude/projects/<slug>/<session>.jsonl`。1 行ごとに
//!   `message.content[]` 内の `tool_use`（name = Read/Edit/Write、
//!   input.file_path）を見る。
//! - pi: `~/.pi/agent/sessions/<slug>/<session>.jsonl`。同様に
//!   `toolCall`（name = read/edit/write、arguments.path）を見る。
//!
//! 意味論（契約）:
//! - reads/edits は時系列順。
//! - 同一パスの Read が [`FOLD_WINDOW`]（60 秒）以内に連続したら 1 回に
//!   畳む（グループの最初の時刻を採用。Claude の offset ページネーション
//!   対策）。Edit/Write は畳まない。
//! - [`filter_root`] は root 配下のレコードだけ残し、相対パスに変換する。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

/// 同一パスの Read を畳み込むウィンドウ（契約で固定）。
pub const FOLD_WINDOW: Duration = Duration::from_secs(60);

/// ログのバックエンド（ログの置き場から判別）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// Claude Code（`~/.claude/projects/`）。
    Claude,
    /// pi（`~/.pi/agent/sessions/`）。
    Pi,
}

/// 1 回の Read（畳み込み後）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRecord {
    /// 読まれたファイルのパス（`filter_root` 前は絶対パス、後は相対パス）。
    pub path: PathBuf,
    /// Read 時刻。
    pub at: SystemTime,
}

/// 1 回の Edit/Write。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    pub path: PathBuf,
    pub at: SystemTime,
}

/// 1 ログ分の抽出結果。reads/edits は時系列順。
#[derive(Clone, Debug, Default)]
pub struct LogData {
    pub reads: Vec<ReadRecord>,
    pub edits: Vec<EditRecord>,
}

/// root のエージェントログを探して (backend, パス) のリストを返す
/// （パス順で決定的）。スタブはホーム配下のログ置き場を再帰的に歩くだけ
/// で、root に紐づくログの絞り込みは本実装の仕事（`filter_root` が
/// パス単位で落とすので、結果の意味論は同じ）。
pub fn discover(root: &Path) -> Vec<(Backend, PathBuf)> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        // Claude Code のセッションログ（`<slug>/<session>.jsonl`）。
        for p in walk_jsonl(&home.join(".claude").join("projects")) {
            out.push((Backend::Claude, p));
        }
        // pi のセッションログ（`<slug>/<session>.jsonl`）。
        for p in walk_jsonl(&home.join(".pi").join("agent").join("sessions")) {
            out.push((Backend::Pi, p));
        }
    }
    let _ = root; // スタブ: root は本実装のプロジェクト絞り込み用
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// `dir` 配下の `*.jsonl` を再帰的に集める（隠しディレクトリはスキップ —
/// Claude の `.deleted_dirs` など、ゴミ箱に入ったセッションは数えない）。
fn walk_jsonl(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            out.extend(walk_jsonl(&path));
        } else if name.ends_with(".jsonl") {
            out.push(path);
        }
    }
    out
}

/// ログ 1 本をパースする。行単位の JSON で、内容を解釈できない行
/// （書きかけの末尾行・他ツールの行・summary 行など）は読み飛ばす —
/// ログは追記されながら読まれるので、部分行で失敗してはならない。
pub fn parse_log(path: &Path, backend: Backend) -> Result<LogData> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut data = LogData::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // 書きかけの末尾行
        };
        // タイムスタンプの無い行（summary 等）は対象外。
        let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(at) = parse_ts(ts) else { continue };
        // content が配列の行だけがツール呼び出しを持ち得る
        // （user メッセージの content は文字列のことがある）。
        let Some(items) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for item in items {
            let Some(ty) = item.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(name) = item.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            // Claude は tool_use + input.file_path、pi は toolCall +
            // arguments.path。バックエンドに応じて読む場所を選ぶ。
            let path = match (backend, ty) {
                (Backend::Claude, "tool_use") => item.pointer("/input/file_path"),
                (Backend::Pi, "toolCall") => item.pointer("/arguments/path"),
                _ => None,
            };
            let Some(path) = path.and_then(|p| p.as_str()) else {
                continue;
            };
            match name {
                "Read" | "read" => data.reads.push(ReadRecord {
                    path: PathBuf::from(path),
                    at,
                }),
                "Edit" | "Write" | "edit" | "write" => data.edits.push(EditRecord {
                    path: PathBuf::from(path),
                    at,
                }),
                _ => {} // Bash / WebFetch 等は対象外
            }
        }
    }
    data.reads = fold_reads(data.reads);
    Ok(data)
}

/// RFC3339 文字列（`2026-08-11T01:00:05.000Z`）を `SystemTime` に。
fn parse_ts(s: &str) -> Option<SystemTime> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    // オーバーフロー（1970 年以前）は対象外の行として読み飛ばす。
    UNIX_EPOCH.checked_add(Duration::from_secs(dt.timestamp() as u64))
}

/// 同一パスの Read を [`FOLD_WINDOW`] 以内に連続していたら畳む。
/// アンカーは「そのパスの直近の Read」（連続判定）、出力はグループの
/// 最初の時刻（契約: 最初の時刻を採用）。時系列順の入力を想定。
fn fold_reads(reads: Vec<ReadRecord>) -> Vec<ReadRecord> {
    // path → そのパスの直近の Read 時刻（畳み込み判定のアンカー）。
    let mut last: HashMap<PathBuf, SystemTime> = HashMap::new();
    let mut out = Vec::with_capacity(reads.len());
    for r in reads {
        if let Some(&prev) = last.get(&r.path) {
            // 過去時刻への逆転（時計の揺れ）は畳む側に倒す。
            let gap = r.at.duration_since(prev).unwrap_or(Duration::ZERO);
            if gap <= FOLD_WINDOW {
                continue;
            }
        }
        last.insert(r.path.clone(), r.at);
        out.push(r);
    }
    out
}

/// root 配下のレコードだけ残し、パスを root 相対に変換する。
/// root 外のパス（別プロジェクトの読込）は落とす。
pub fn filter_root(data: LogData, root: &Path) -> LogData {
    let strip = |p: &PathBuf| p.strip_prefix(root).ok().map(|r| r.to_path_buf());
    LogData {
        reads: data
            .reads
            .into_iter()
            .filter_map(|r| {
                Some(ReadRecord {
                    path: strip(&r.path)?,
                    at: r.at,
                })
            })
            .collect(),
        edits: data
            .edits
            .into_iter()
            .filter_map(|e| {
                Some(EditRecord {
                    path: strip(&e.path)?,
                    at: e.at,
                })
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/testdata")
            .join(name)
    }

    fn ts(s: &str) -> SystemTime {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    /// 正典（fixture の期待結果）: claude は offset Read（01:00:07）を
    /// 畳み込み、01:05:00 の再 read は 60 秒超なので別グループ。
    #[test]
    fn parses_claude_fixture_with_folding() {
        let data = parse_log(&fixture("claude-session.jsonl"), Backend::Claude).unwrap();
        // パース直後は root 外の読込（/Users/nagata/other/outside.rs）も
        // 含む（正典の期待結果は filter_root 後のリスト）。
        assert_eq!(data.reads.len(), 5);
        assert_eq!(
            data.reads[4].path,
            PathBuf::from("/Users/nagata/other/outside.rs")
        );
        // 正典: root = /Users/nagata/proj。offset Read（01:00:07）は
        // 畳み込み、01:05:00 の再 read は 60 秒超なので別グループ。
        let data = filter_root(data, Path::new("/Users/nagata/proj"));
        let reads: Vec<(String, SystemTime)> = data
            .reads
            .iter()
            .map(|r| (r.path.display().to_string(), r.at))
            .collect();
        assert_eq!(
            reads,
            vec![
                ("src/main.rs".to_string(), ts("2026-08-11T01:00:05Z")),
                ("src/files.rs".to_string(), ts("2026-08-11T01:00:09Z")),
                ("README.md".to_string(), ts("2026-08-11T01:02:00Z")),
                ("src/main.rs".to_string(), ts("2026-08-11T01:05:00Z")),
            ]
        );
        assert_eq!(
            data.edits,
            vec![EditRecord {
                path: PathBuf::from("src/main.rs"),
                at: ts("2026-08-11T01:01:00Z"),
            }]
        );
    }

    #[test]
    fn parses_pi_fixture() {
        let data = parse_log(&fixture("pi-session.jsonl"), Backend::Pi).unwrap();
        let reads: Vec<(String, SystemTime)> = data
            .reads
            .iter()
            .map(|r| (r.path.display().to_string(), r.at))
            .collect();
        assert_eq!(
            reads,
            vec![
                (
                    "/Users/nagata/proj/src/main.rs".to_string(),
                    ts("2026-08-11T01:10:00Z")
                ),
                (
                    "/Users/nagata/proj/README.md".to_string(),
                    ts("2026-08-11T01:10:05Z")
                ),
                (
                    "/Users/nagata/proj/src/main.rs".to_string(),
                    ts("2026-08-11T01:12:00Z")
                ),
                (
                    "/Users/nagata/proj/src/files.rs".to_string(),
                    ts("2026-08-11T01:14:00Z")
                ),
            ]
        );
        assert_eq!(
            data.edits,
            vec![EditRecord {
                path: PathBuf::from("/Users/nagata/proj/src/main.rs"),
                at: ts("2026-08-11T01:10:30Z"),
            }]
        );
    }

    /// 畳み込みは「同一パスの連続した Read が FOLD_WINDOW 以内か」で
    /// 判定する（ページネーション対策）。tempfile に書き出して
    /// parse_log 経由で検証する（実時間依存なし）。
    #[test]
    fn fold_merges_consecutive_reads_within_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        // JSON は手組みせず json! で組み立てる（ブレースのエスケープ間違いを防ぐ）。
        let line = |ts: &str, path: &str| {
            serde_json::json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "name": "Read", "input": { "file_path": path } }
                    ],
                },
                "timestamp": ts,
            })
            .to_string()
        };
        let mut s = String::new();
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:05Z", "/p/a.rs")));
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:06Z", "/p/a.rs"))); // 1 秒後 → 畳む
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:59Z", "/p/a.rs"))); // 59 秒後 → 畳む
        s.push_str(&format!("{}\n", line("2026-08-11T01:02:00Z", "/p/a.rs"))); // 60 秒超 → 別グループ
        s.push_str(&format!("{}\n", line("2026-08-11T01:02:30Z", "/p/b.rs")));
        s.push_str(&format!("{}\n", line("2026-08-11T01:03:00Z", "/p/b.rs"))); // 30 秒後 → 畳む
        s.push_str(&format!("{}\n", line("2026-08-11T01:03:01Z", "/p/b.rs"))); // 1 秒後 → 畳む
        std::fs::write(&f, s).unwrap();
        let data = parse_log(&f, Backend::Claude).unwrap();
        let reads: Vec<(String, SystemTime)> = data
            .reads
            .iter()
            .map(|r| (r.path.display().to_string(), r.at))
            .collect();
        assert_eq!(
            reads,
            vec![
                ("/p/a.rs".to_string(), ts("2026-08-11T01:00:05Z")), // グループ先頭を採用
                ("/p/a.rs".to_string(), ts("2026-08-11T01:02:00Z")),
                ("/p/b.rs".to_string(), ts("2026-08-11T01:02:30Z")),
            ]
        );
    }

    /// Edit/Write は畳まない（契約: 畳むのは Read のみ）。
    #[test]
    fn edits_are_not_folded() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        let line = |ts: &str, name: &str| {
            serde_json::json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "name": name, "input": { "file_path": "/p/a.rs" } }
                    ],
                },
                "timestamp": ts,
            })
            .to_string()
        };
        let mut s = String::new();
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:05Z", "Edit")));
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:06Z", "Edit")));
        s.push_str(&format!("{}\n", line("2026-08-11T01:00:07Z", "Write")));
        std::fs::write(&f, s).unwrap();
        let data = parse_log(&f, Backend::Claude).unwrap();
        assert!(data.reads.is_empty());
        assert_eq!(data.edits.len(), 3, "Edit/Write は 1 秒間隔でも畳まない");
    }

    #[test]
    fn filter_root_keeps_only_under_root_and_makes_paths_relative() {
        let data = parse_log(&fixture("claude-session.jsonl"), Backend::Claude).unwrap();
        let data = filter_root(data, Path::new("/Users/nagata/proj"));
        let reads: Vec<String> = data
            .reads
            .iter()
            .map(|r| r.path.display().to_string())
            .collect();
        // root 外の /Users/nagata/other/outside.rs は落ちる。
        assert_eq!(
            reads,
            vec![
                "src/main.rs".to_string(),
                "src/files.rs".to_string(),
                "README.md".to_string(),
                "src/main.rs".to_string(),
            ]
        );
        assert!(data.reads.iter().all(|r| !r.path.is_absolute()));
        assert_eq!(
            data.edits,
            vec![EditRecord {
                path: PathBuf::from("src/main.rs"),
                at: ts("2026-08-11T01:01:00Z"),
            }]
        );
    }

    /// 書きかけの末尾行（追記中に読まれる想定）で失敗しない。
    #[test]
    fn parse_log_tolerates_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        let line = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"/p/a.rs"}}]},"timestamp":"2026-08-11T01:00:05Z"}"#;
        std::fs::write(&f, format!("{line}\n{{\"type\":\"mess")).unwrap();
        let data = parse_log(&f, Backend::Pi).unwrap();
        assert_eq!(data.reads.len(), 1);
        assert_eq!(data.reads[0].path, PathBuf::from("/p/a.rs"));
    }

    /// discover は存在しないディレクトリに対して空を返す（クラッシュしない）。
    #[test]
    fn discover_handles_missing_home_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::var_os("HOME");
        // HOME を空の tempdir に向ける: どちらのログ置き場も無い → 空。
        // （並行テストへの影響を避けるため最後に必ず戻す。）
        // SAFETY: HOME はこのテスト専用に差し替える（discover が読むのは
        // ここだけ。他のテストは HOME に依存しない）。
        unsafe { std::env::set_var("HOME", dir.path()) };
        let logs = discover(Path::new("/some/root"));
        assert!(logs.is_empty());
        // tempdir に pi 形式のログ置き場を作ると見つかる。
        let sessions = dir.path().join(".pi/agent/sessions/slug");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("s.jsonl"), "{}").unwrap();
        let logs = discover(Path::new("/some/root"));
        assert_eq!(logs, vec![(Backend::Pi, sessions.join("s.jsonl"))]);
        match old {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
