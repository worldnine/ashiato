//! エージェントセッションログ（JSONL）パーサー。
//!
//! Claude Code（`~/.claude/projects/<slug>/*.jsonl`）と pi（`~/.pi/agent/sessions/<slug>/*.jsonl`）
//! のセッションログから、エージェントが Read したファイルと Edit/Write したファイルを
//! 時刻付きで取り出す。read ビュー（TUI 側）のデータ源。
//!
//! 両 backend のログは「assistant レコードの `message.content[]` に tool 呼び出しが
//! 並ぶ」という共通構造で、違いは content 要素の型（tool_use / toolCall）と tool 名の
//! 大文字小文字だけなので、backend ごとに取得箇所を分けるだけで済む。
//! 実機ログには mode 変更・summary・toolResult など多彩なレコード種別が混在するため、
//! 1 行の異常（壊れた JSON・未知の tool・timestamp 欠落）はその行だけ読み飛ばし、
//! ログ全体を捨てない部分解釈が正しい。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde_json::Value;

/// 同じパスの連続 Read がこの間隔以内なら 1 回に畳む。
/// Claude は大きなファイルを offset 指定で複数回 Read するため、ページネーションの
/// 続き（数秒以内の再 Read）を別カウントにすると read 回数が水増しされてしまう。
pub const FOLD_WINDOW: Duration = Duration::from_secs(60);

/// セッションログの書き手（backend）。ログの置き場所と JSON の形が違う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Claude,
    Pi,
}

/// エージェントによる 1 回の Read（畳み込み後）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRecord {
    pub path: PathBuf,
    pub at: SystemTime,
}

/// エージェントによる 1 回の Edit/Write。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditRecord {
    pub path: PathBuf,
    pub at: SystemTime,
}

/// ログ 1 本ぶん（または backend 横断マージ後）の読み出し結果。時系列順。
#[derive(Clone, Debug, Default)]
pub struct LogData {
    pub reads: Vec<ReadRecord>,
    pub edits: Vec<EditRecord>,
}

/// root に関連するセッションログを発見する。backend ごとに最大 1 つ返す
/// （最新セッションだけ見せれば十分で、古いログまで読むと read ビューが
/// 過去の作業履歴で埋まってしまうため）。
pub fn discover(root: &Path) -> Vec<(Backend, PathBuf)> {
    // base ディレクトリは HOME 直下の固定パス。本体（discover_at）は base を引数で
    // 受ける形に分離してあり、テストは一時ディレクトリを base に渡す（実 HOME に
    // 依存せず、環境変数を汚さない）。
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let claude_base = home.as_deref().map(|h| h.join(".claude").join("projects"));
    let pi_base = home.as_deref().map(|h| h.join(".pi").join("agent").join("sessions"));
    discover_at(root, claude_base.as_deref(), pi_base.as_deref())
}

/// discover の本体。セッションログの base ディレクトリを注入できるように分離。
fn discover_at(root: &Path, claude_base: Option<&Path>, pi_base: Option<&Path>) -> Vec<(Backend, PathBuf)> {
    let mut out = Vec::new();
    for (backend, base) in [(Backend::Claude, claude_base), (Backend::Pi, pi_base)] {
        let Some(base) = base else { continue };
        // session_dir は base 直下のディレクトリ名（slug）を返すので、base と結合して使う
        let slug_dir = base.join(session_dir(root, backend));
        if let Some(path) = latest_session(&slug_dir, base) {
            out.push((backend, path));
        }
    }
    out
}

/// セッション 1 本を選ぶ。(1) slug 一致ディレクトリ内の最新 `*.jsonl`（mtime 最大）、
/// (2) 該当ディレクトリが無い・空なら base 直下の全セッションディレクトリを走査して
/// 最新の 1 本。root 外のセッションが選ばれても filter_root が落とすので許容する
/// （worktree や改名済みディレクトリのログを拾うための保険）。
fn latest_session(slug_dir: &Path, base: &Path) -> Option<PathBuf> {
    if let Some((path, _)) = latest_jsonl(slug_dir) {
        return Some(path);
    }
    // base 直下の各ディレクトリの最新セッションを比較し、全体で mtime 最大のものを返す
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        // ディレクトリ以外（ファイル・シンボリックリンク）はセッション置き場ではない
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        if let Some((path, mtime)) = latest_jsonl(&entry.path())
            && best.as_ref().is_none_or(|(_, t)| mtime > *t)
        {
            best = Some((path, mtime));
        }
    }
    best.map(|(path, _)| path)
}

/// ディレクトリ直下の `*.jsonl` のうち mtime 最大のものを (パス, mtime) で返す。
/// メタデータが取れない・途中で壊れたファイルは無視する（ログの読み込みを
/// 1 本の異常で止めない）。
fn latest_jsonl(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let Ok(mtime) = md.modified() else { continue };
        if best.as_ref().is_none_or(|(_, t)| mtime > *t) {
            best = Some((path, mtime));
        }
    }
    best
}

/// root の絶対パスから backend ごとのセッションディレクトリ名（slug）を組み立てる。
///
/// 実機で確認した slug 規則:
/// - Claude: `/` と `.` を `-` に置換する（`/Users/nagata/ghq/github.com/worldnine/ashiato`
///   → `-Users-nagata-ghq-github-com-worldnine-ashiato`。`.claude` は `-claude` になる）。
/// - pi: 先頭の区切りを 1 つ取り除き、`/`・`\`・`:` を `-` に置換して `--` で挟む
///   （pi 本体 session-manager.js の `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`
///   と同じ規則。`.` は置換しないので `.herdr` がそのまま残る）。
fn session_dir(root: &Path, backend: Backend) -> PathBuf {
    let s = absolutize(root).to_string_lossy().into_owned();
    let slug = match backend {
        Backend::Claude => s.replace(['/', '.'], "-"),
        Backend::Pi => {
            let stripped = s.strip_prefix(['/', '\\']).unwrap_or(&s);
            format!("--{}--", stripped.replace(['/', '\\', ':'], "-"))
        }
    };
    PathBuf::from(slug)
}

/// セッションログ 1 ファイルをパースし、時系列順の reads/edits を返す。
/// 壊れた行・未知レコード・timestamp 欠落はその行だけスキップする（堅牢性）。
pub fn parse_log(path: &Path, backend: Backend) -> anyhow::Result<LogData> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("セッションログを読めません: {}", path.display()))?;
    let mut data = LogData::default();
    for line in content.lines() {
        // 壊れた JSON 行（書きかけ・部分行）は読み飛ばし。1 行の異常でログ全体を
        // 捨てない。
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        // timestamp はレコード直下の ISO8601。欠落・非 ISO8601 のレコードは対象外
        // （時刻のない read は表示の並び順を決められないため）。
        let Some(at) = record_time(&rec) else { continue };
        // tool 呼び出しは assistant レコードにしか現れない。user / toolResult /
        // summary / mode 変更などのレコードはここで落ちる。
        let Some(role) = rec.pointer("/message/role").and_then(Value::as_str) else { continue };
        if role != "assistant" {
            continue;
        }
        let Some(items) = rec.pointer("/message/content").and_then(Value::as_array) else { continue };
        for item in items {
            let Some(kind) = tool_kind(item, backend) else { continue };
            let Some(path_str) = tool_path(item, backend) else { continue };
            // JSON の文字列は常に UTF-8 なので非 UTF-8 パスは構造的に混入しない。
            // 相対パスは root と突き合わせられないのでスキップする。
            let path = PathBuf::from(path_str);
            if !path.is_absolute() {
                continue;
            }
            match kind {
                ToolKind::Read => data.reads.push(ReadRecord { path, at }),
                ToolKind::Edit => data.edits.push(EditRecord { path, at }),
            }
        }
    }
    // ログは時系列で書かれるはずだが、行順に依存せず時刻順に整列する
    // （sort_by_key は安定ソートなので同時刻は行順が保たれる）。
    data.reads.sort_by_key(|r| r.at);
    data.edits.sort_by_key(|r| r.at);
    fold_reads(&mut data.reads);
    Ok(data)
}

/// レコード直下の `timestamp` を SystemTime に変換する。欠落・非 ISO8601 は None。
fn record_time(rec: &Value) -> Option<SystemTime> {
    let ts = rec.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(Into::into)
}

/// 対象 tool の種別（Read と Edit/Write の 2 系統のみ扱う）。
#[derive(Clone, Copy)]
enum ToolKind {
    Read,
    Edit,
}

/// content 要素が対象の tool 呼び出しなら種別を返す。未知の tool 名・
/// tool 呼び出しでない要素（text / thinking 等）は None。
/// tool 名の大文字小文字は backend の流儀そのまま照合する
/// （Claude は Read/Edit/Write、pi は read/edit/write）。
fn tool_kind(item: &Value, backend: Backend) -> Option<ToolKind> {
    let name = item.get("name")?.as_str()?;
    match backend {
        Backend::Claude => {
            if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            match name {
                "Read" => Some(ToolKind::Read),
                "Edit" | "Write" => Some(ToolKind::Edit),
                _ => None,
            }
        }
        Backend::Pi => {
            if item.get("type").and_then(Value::as_str) != Some("toolCall") {
                return None;
            }
            match name {
                "read" => Some(ToolKind::Read),
                "edit" | "write" => Some(ToolKind::Edit),
                _ => None,
            }
        }
    }
}

/// content 要素から tool が操作したファイルパスを取り出す。
/// Claude は `input.file_path`、pi は `arguments.path`。
fn tool_path(item: &Value, backend: Backend) -> Option<&str> {
    let ptr = match backend {
        Backend::Claude => "/input/file_path",
        Backend::Pi => "/arguments/path",
    };
    item.pointer(ptr).and_then(Value::as_str)
}

/// 同一パスの連続 Read を、直前の採用 Read から FOLD_WINDOW 以内なら 1 回に畳む
/// （最初のタイムスタンプを採用）。reads は時刻順ソート済み前提。
/// Claude の offset ページネーションは数秒間隔で同じファイルを Read するため、
/// これを畳まないと read 回数がページ数ぶん水増しされる。
fn fold_reads(reads: &mut Vec<ReadRecord>) {
    let mut last_at: HashMap<PathBuf, SystemTime> = HashMap::new();
    let mut out = Vec::with_capacity(reads.len());
    for r in reads.drain(..) {
        // 時刻が逆行している（ログの時系列が壊れている）場合は畳まずに残す
        // （duration_since が Err になる）。
        let within = last_at
            .get(&r.path)
            .is_some_and(|t| r.at.duration_since(*t).is_ok_and(|d| d <= FOLD_WINDOW));
        if within {
            continue;
        }
        last_at.insert(r.path.clone(), r.at);
        out.push(r);
    }
    *reads = out;
}

/// root 配下に収まるレコードだけ残し、パスを root からの相対パスに変換する。
/// root 外（別プロジェクト・ホーム配下）のファイルは read ビューに混ぜないため。
/// レコードの並び順（時系列順）は維持する。
pub fn filter_root(data: LogData, root: &Path) -> LogData {
    let root = absolutize(root);
    // strip_prefix はコンポーネント境界で比較するので、`/a/b` と `/a/bc` を
    // 誤って親子扱いしない。root 外・`..` を含むパスは Err になり落ちる。
    let strip = |p: &Path| p.strip_prefix(&root).map(Path::to_path_buf).ok();
    LogData {
        reads: data
            .reads
            .into_iter()
            .filter_map(|r| strip(&r.path).map(|path| ReadRecord { path, at: r.at }))
            .collect(),
        edits: data
            .edits
            .into_iter()
            .filter_map(|r| strip(&r.path).map(|path| EditRecord { path, at: r.at }))
            .collect(),
    }
}

/// パスを絶対パス化する。ログのパスは常に絶対パスなので、root 側も絶対で揃えて
/// プレフィックス比較する。canonicalize は使わない（実在しない root でも slug を
/// 計算できる必要がある。シンボリックリンク解決は実機のログ保存規則と一致しない）。
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// fixture 文字列を一時ファイルに書き出して parse_log に渡す。
    /// parse_log はパスを受け取る API なので、include_str! の中身を
    /// 毎回 tempfile に再現する（実ディレクトリに依存しない）。
    fn parse_fixture(backend: Backend, fixture: &str) -> LogData {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(fixture.as_bytes()).unwrap();
        parse_log(f.path(), backend).unwrap()
    }

    /// ISO8601 文字列から期待値の SystemTime を作る（パーサーと同じ解釈）。
    fn at(s: &str) -> SystemTime {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    fn read_paths(data: &LogData) -> Vec<PathBuf> {
        data.reads.iter().map(|r| r.path.clone()).collect()
    }

    /// 一時ディレクトリにセッションファイルを作り、mtime を明示指定する
    /// （ファイル作成順ではなく mtime 順で最新が選ばれることを検証するため）。
    fn write_with_mtime(path: &Path, secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs)))
            .unwrap();
    }

    #[test]
    fn claude_fixture_parses_with_folding() {
        // 正典 fixture。01:00:07 の offset Read は 01:00:05 の Read に畳み込まれる。
        // parse_log 直後は絶対パスのまま（root 外の outside.rs も含む 5 件。
        // filter_root 後の相対パス 4 件は filter_root_keeps_inside_and_drops_outside で検証）
        let data = parse_fixture(Backend::Claude, include_str!("testdata/claude-session.jsonl"));
        let expect = [
            ("/Users/nagata/proj/src/main.rs", "2026-08-11T01:00:05.000Z"),
            ("/Users/nagata/proj/src/files.rs", "2026-08-11T01:00:09.000Z"),
            ("/Users/nagata/proj/README.md", "2026-08-11T01:02:00.000Z"),
            ("/Users/nagata/proj/src/main.rs", "2026-08-11T01:05:00.000Z"),
            ("/Users/nagata/other/outside.rs", "2026-08-11T01:06:40.000Z"),
        ];
        assert_eq!(data.reads.len(), expect.len());
        for (r, (p, t)) in data.reads.iter().zip(expect) {
            assert_eq!(r.path, PathBuf::from(p));
            assert_eq!(r.at, at(t));
        }
        assert_eq!(data.edits.len(), 1);
        assert_eq!(data.edits[0].path, PathBuf::from("/Users/nagata/proj/src/main.rs"));
        assert_eq!(data.edits[0].at, at("2026-08-11T01:01:00.000Z"));
    }

    #[test]
    fn pi_fixture_parses() {
        // 正典 fixture。main.rs の 01:10:00 → 01:12:00 は 120 秒差なので別カウント
        let data = parse_fixture(Backend::Pi, include_str!("testdata/pi-session.jsonl"));
        let expect = [
            ("/Users/nagata/proj/src/main.rs", "2026-08-11T01:10:00.000Z"),
            ("/Users/nagata/proj/README.md", "2026-08-11T01:10:05.000Z"),
            ("/Users/nagata/proj/src/main.rs", "2026-08-11T01:12:00.000Z"),
            ("/Users/nagata/proj/src/files.rs", "2026-08-11T01:14:00.000Z"),
        ];
        assert_eq!(data.reads.len(), expect.len());
        for (r, (p, t)) in data.reads.iter().zip(expect) {
            assert_eq!(r.path, PathBuf::from(p));
            assert_eq!(r.at, at(t));
        }
        assert_eq!(data.edits.len(), 1);
        assert_eq!(data.edits[0].path, PathBuf::from("/Users/nagata/proj/src/main.rs"));
        assert_eq!(data.edits[0].at, at("2026-08-11T01:10:30.000Z"));
    }

    #[test]
    fn filter_root_keeps_inside_and_drops_outside() {
        // claude fixture の root 外 Read（/Users/nagata/other/outside.rs）が落ち、
        // 残りは root からの相対パスになる
        let data = parse_fixture(Backend::Claude, include_str!("testdata/claude-session.jsonl"));
        let filtered = filter_root(data, Path::new("/Users/nagata/proj"));
        assert_eq!(
            read_paths(&filtered),
            [
                PathBuf::from("src/main.rs"),
                PathBuf::from("src/files.rs"),
                PathBuf::from("README.md"),
                PathBuf::from("src/main.rs"),
            ]
        );
        assert_eq!(filtered.edits.len(), 1);
        assert_eq!(filtered.edits[0].path, PathBuf::from("src/main.rs"));
        // 時刻は変換後も維持される
        assert_eq!(filtered.reads[0].at, at("2026-08-11T01:00:05.000Z"));
    }

    #[test]
    fn fold_window_boundary() {
        // ちょうど 60 秒差は畳み込み、60 秒超は別カウント。別パスは影響しない
        let lines = [
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#,
            // ちょうど 60 秒後 → 畳み込まれる
            r#"{"timestamp":"2026-08-11T01:01:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#,
            // 直前の採用（01:00:00）から 120 秒後 → 別カウント
            r#"{"timestamp":"2026-08-11T01:02:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#,
            // 別パスは常に別カウント
            r#"{"timestamp":"2026-08-11T01:02:05.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/b.rs"}}]}}"#,
        ];
        let data = parse_fixture(Backend::Claude, &lines.join("\n"));
        assert_eq!(
            read_paths(&data),
            [PathBuf::from("/p/a.rs"), PathBuf::from("/p/a.rs"), PathBuf::from("/p/b.rs")]
        );
        assert_eq!(data.reads[0].at, at("2026-08-11T01:00:00.000Z"));
        assert_eq!(data.reads[1].at, at("2026-08-11T01:02:00.000Z"));
        assert_eq!(data.reads[2].at, at("2026-08-11T01:02:05.000Z"));
    }

    #[test]
    fn malformed_lines_and_unknown_records_are_skipped() {
        // 実機ログに混在する多様なレコード種別・壊れた行をすべて読み飛ばし、
        // 有効な行だけ拾う
        let lines = [
            "これは JSON ではない", // 壊れた行
            r#"{"timestamp":"2026-08-11T01:00:00.000Z"}"#, // message なし
            r#"{"timestamp":"not-a-date","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#, // 非 ISO8601
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#, // timestamp 欠落
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"user","content":"hello"}}"#, // user レコード
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#, // 未知 tool
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#, // tool でない content 要素
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"toolResult","toolName":"read","content":[]}}"#, // toolResult
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"web_search","arguments":{"query":"x"}}]}}"#, // pi 未知 tool
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"relative/path.rs"}}]}}"#, // 相対パス
            r#"{"type":"summary","summary":"x"}"#, // summary レコード
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/ok.rs"}}]}}"#, // 唯一の有効行
        ];
        let data = parse_fixture(Backend::Claude, &lines.join("\n"));
        assert_eq!(read_paths(&data), [PathBuf::from("/ok.rs")]);
    }

    #[test]
    fn records_sorted_by_time_regardless_of_line_order() {
        // ログの行順が時刻順でなくても、結果は時刻順に整列される
        let lines = [
            r#"{"timestamp":"2026-08-11T01:05:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"/p/z.rs"}}]}}"#,
            r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"read","arguments":{"path":"/p/a.rs"}}]}}"#,
        ];
        let data = parse_fixture(Backend::Pi, &lines.join("\n"));
        assert_eq!(read_paths(&data), [PathBuf::from("/p/a.rs"), PathBuf::from("/p/z.rs")]);
    }

    #[test]
    fn claude_real_world_record_shape() {
        // 実機ログの assistant レコード（レコード直下の type、tool_use の caller、
        // offset/limit などの余分フィールド付き）も解釈できること
        let line = r#"{"parentUuid":"x","isSidechain":false,"message":{"model":"m","id":"i","type":"message","role":"assistant","content":[{"type":"tool_use","id":"t","name":"Read","input":{"file_path":"/p/a.rs","offset":1,"limit":100},"caller":{"type":"direct"}}],"stop_reason":"tool_use"},"type":"assistant","timestamp":"2026-08-11T01:00:00.000Z","sessionId":"s","cwd":"/p"}"#;
        let data = parse_fixture(Backend::Claude, line);
        assert_eq!(read_paths(&data), [PathBuf::from("/p/a.rs")]);
    }

    #[test]
    fn pi_multiple_tool_calls_in_one_content() {
        // 実機の pi ログでは 1 つの assistant メッセージ（1 行）の content に複数の
        // toolCall が並ぶ（bash と read の同時呼び出し等）。配列全体を走査して拾うこと
        let line = r#"{"timestamp":"2026-08-11T01:00:00.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls"}},{"type":"toolCall","id":"c2","name":"read","arguments":{"path":"/p/a.rs"}},{"type":"toolCall","id":"c3","name":"write","arguments":{"path":"/p/b.rs"}}]}}"#;
        let data = parse_fixture(Backend::Pi, line);
        assert_eq!(read_paths(&data), [PathBuf::from("/p/a.rs")]);
        assert_eq!(data.edits[0].path, PathBuf::from("/p/b.rs"));
    }

    #[test]
    fn slug_matches_observed_directories() {
        // 実機で観測した slug（~/.claude/projects と ~/.pi/agent/sessions の
        // 実ディレクトリ名）をそのまま再現できること
        let c = |p: &str| session_dir(Path::new(p), Backend::Claude).to_string_lossy().into_owned();
        // github.com の `.` が `-` になる
        assert_eq!(
            c("/Users/nagata/ghq/github.com/worldnine/ashiato"),
            "-Users-nagata-ghq-github-com-worldnine-ashiato"
        );
        assert_eq!(
            c("/Users/nagata/ghq/github.com/worldnine/akapen"),
            "-Users-nagata-ghq-github-com-worldnine-akapen"
        );
        // `.claude` 配下のワークトリーも `.` が `-` になる（実機: sample-project--claude-worktrees-...）
        assert_eq!(
            c("/Users/nagata/src/tries/2025-11-17/sample-project/.claude/worktrees/keen-foraging-riddle"),
            "-Users-nagata-src-tries-2025-11-17-sample-project--claude-worktrees-keen-foraging-riddle"
        );
        let p = |s: &str| session_dir(Path::new(s), Backend::Pi).to_string_lossy().into_owned();
        // pi は `.` を置換せず、`--` で挟む
        assert_eq!(
            p("/Users/nagata/ghq/github.com/worldnine/ashiato"),
            "--Users-nagata-ghq-github.com-worldnine-ashiato--"
        );
        // `.herdr` がそのまま残る（実機: --Users-nagata-.herdr-worktrees-akapen-...）
        assert_eq!(
            p("/Users/nagata/.herdr/worktrees/akapen/feat-table-relayout"),
            "--Users-nagata-.herdr-worktrees-akapen-feat-table-relayout--"
        );
    }

    #[test]
    fn discover_prefers_slug_dir_and_newest() {
        // slug 一致ディレクトリ内で mtime 最大のセッションが選ばれる（backend ごとに 1 本）
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/Users/nagata/proj"); // 実在しなくてよい（slug 計算にのみ使う）
        let claude_base = tmp.path().join("claude");
        let pi_base = tmp.path().join("pi");

        write_with_mtime(&claude_base.join("-Users-nagata-proj").join("old.jsonl"), 100);
        write_with_mtime(&claude_base.join("-Users-nagata-proj").join("new.jsonl"), 200);
        write_with_mtime(&pi_base.join("--Users-nagata-proj--").join("pi.jsonl"), 300);
        // slug 外のディレクトリは候補にならない（slug 一致が優先される）
        write_with_mtime(&claude_base.join("-Users-nagata-other").join("other.jsonl"), 900);

        let found = discover_at(root, Some(&claude_base), Some(&pi_base));
        assert_eq!(
            found,
            [
                (Backend::Claude, claude_base.join("-Users-nagata-proj").join("new.jsonl")),
                (Backend::Pi, pi_base.join("--Users-nagata-proj--").join("pi.jsonl")),
            ]
        );
    }

    #[test]
    fn discover_falls_back_to_base_scan() {
        // slug 一致ディレクトリが無いときは base 直下の全ディレクトリから
        // mtime 最大のセッションを 1 本選ぶ（root 外のセッションでも filter_root が落とす）
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/Users/nagata/proj");
        let claude_base = tmp.path().join("claude");
        let pi_base = tmp.path().join("pi");

        write_with_mtime(&claude_base.join("-Users-nagata-elsewhere").join("a.jsonl"), 100);
        write_with_mtime(&claude_base.join("-Users-nagata-elsewhere").join("b.jsonl"), 200);
        write_with_mtime(&pi_base.join("--Users-nagata-elsewhere--").join("c.jsonl"), 300);

        let found = discover_at(root, Some(&claude_base), Some(&pi_base));
        assert_eq!(
            found,
            [
                (Backend::Claude, claude_base.join("-Users-nagata-elsewhere").join("b.jsonl")),
                (Backend::Pi, pi_base.join("--Users-nagata-elsewhere--").join("c.jsonl")),
            ]
        );
    }

    #[test]
    fn discover_handles_missing_or_empty_dirs() {
        // base ディレクトリ自体が無い・slug ディレクトリが空 → 何も返さない（エラーにしない）
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("none");
        let empty_base = tmp.path().join("empty");
        std::fs::create_dir_all(empty_base.join("-Users-nagata-proj")).unwrap();
        let found = discover_at(Path::new("/Users/nagata/proj"), Some(&missing), Some(&empty_base));
        assert!(found.is_empty());
        // base 自体が None（HOME 未設定相当）でもパニックしない
        let found = discover_at(Path::new("/Users/nagata/proj"), None, None);
        assert!(found.is_empty());
    }
}
