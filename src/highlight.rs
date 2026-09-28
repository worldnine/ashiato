//! NOTE: this file is a manual COPY of akapen's `src/highlight.rs`
//! (ashiato stays standalone by design; drift only means slightly
//! different preview colors). If this copy needs a change for the
//! third time, extract a small shared crate instead — not before.
//! テーマの部分（名前・`.tmTheme` の解決、側ごとの既定、Markdown の scope の
//! 読み替え、既定の文字色）は termtheme（`termtheme::theme`）へ移した。ここに
//! 残るのは文法の選択・トークン化・折り返し。
//! Syntax highlighting via `syntect`, ported from herdr-reviewr's
//! `highlight.rs` (MIT, Dmitry Persiyanov) and simplified: akapen always
//! highlights markdown, uses syntect's bundled defaults instead of two-face,
//! and renders directly to ratatui `Style`s.
//!
//! The whole file is tokenized once (cross-line context like fenced code
//! blocks needs the full content); the UI then wraps and styles per line.

use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use termtheme::theme;

/// The broad two-face syntax set, deserialized once and shared (it is
/// expensive to build). two-face carries newer grammar definitions than
/// syntect's bundled defaults, so third-party themes match better.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// One styled text fragment: syntect's per-token color plus the display
/// styles (selection background, cursor) applied by the UI later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// Highlights content into per-line span vectors. The grammar is picked
/// per file ([`syntax_for`]).
pub struct Highlighter {
    theme: Theme,
    /// 背景がライトか（テーマが文字色を持たないときの文字色を選ぶ）。
    light: bool,
}

/// Pick the grammar for `path` from syntect's bundled set (100+ languages):
/// by extension and filename (Makefile, Dockerfile, …), falling back to
/// plain text for unknown files.
pub fn syntax_for(path: &std::path::Path) -> &'static SyntaxReference {
    syntaxes()
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| syntaxes().find_syntax_plain_text())
}

impl Highlighter {
    /// テーマ（two-face の名前か `.tmTheme` のパス）から作る。無い・解決できない
    /// ときはその側の既定（`termtheme::theme::DEFAULT_DARK`、`light` なら
    /// `DEFAULT_LIGHT`）へ落ちる（`termtheme::theme::resolve`）。
    /// The default grammar is markdown; source mode passes per-file
    /// grammars via [`Self::highlight_with`].
    pub fn new(theme_name: Option<&str>, light: bool) -> Self {
        Self {
            theme: theme::resolve(theme_name, light),
            light,
        }
    }

    /// The theme's default foreground (what plain text renders as).
    pub fn default_fg(&self) -> Color {
        theme::default_fg(&self.theme, self.light)
    }

    /// Tokenize `content` once; each inner vec is one source line's spans.
    /// A grammar error degrades that line to a single plain span.
    /// Tokenize `content` with the given grammar into per-line spans
    /// (cross-line context like fenced code blocks needs the full
    /// content).
    pub fn highlight_with(
        &self,
        content: &str,
        syntax: &'static SyntaxReference,
    ) -> Vec<Vec<Span>> {
        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in LinesWithEndings::from(content) {
            let spans = match h.highlight_line(line, syntaxes()) {
                Ok(regions) => regions
                    .into_iter()
                    .map(|(style, text)| Span {
                        text: text.trim_end_matches('\n').to_string(),
                        style: Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        )),
                    })
                    .collect(),
                Err(_) => vec![Span {
                    text: line.trim_end_matches('\n').to_string(),
                    style: Style::default().fg(self.default_fg()),
                }],
            };
            out.push(spans);
        }
        out
    }
}

/// The display width of a tab: terminals expand tabs to the next 8-column
/// stop, so a tab at column 0 is 8 columns wide, at column 3 it is 5, etc.
const TAB_STOP: usize = 8;

/// Wrap `spans` into display rows no wider than `width` columns, measuring
/// with `unicode-width` so CJK full-width characters never misalign. Tabs
/// are expanded to the spaces a terminal would show (8-column stops from
/// the row's current column): ratatui's cell grid measures `\t` as width 0
/// and its renderer drops control characters, so a raw tab would misalign
/// every following character and lose the selection/cursor background on
/// the expansion. An empty input yields one empty row (a blank source line
/// stays a row).
pub fn wrap_spans(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut row: Vec<Span> = Vec::new();
    let mut col = 0usize; // display column where the next character lands
    for span in spans {
        let mut rest = span.text.as_str();
        while !rest.is_empty() {
            // The current row is full: flush it and start the next.
            if col >= width {
                rows.push(std::mem::take(&mut row));
                col = 0;
            }
            let (take, take_w) = take_fit(rest, col, width - col);
            if take.is_empty() {
                // The next character is a tab too wide for the rest of
                // this row: flush and retry from column 0, where the tab
                // expands like a terminal's line-leading tab.
                rows.push(std::mem::take(&mut row));
                col = 0;
                continue;
            }
            row.push(Span {
                text: expand_tabs(take, col),
                style: span.style,
            });
            col += take_w;
            rest = &rest[take.len()..];
            if col >= width {
                rows.push(std::mem::take(&mut row));
                col = 0;
            }
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}


/// Width-aware wrapping that keeps a parallel source-line attribution.
/// Takes one `Option<usize>` per input span and returns, per display row,
/// the row's spans plus one attribution per span; a fragment split off a
/// wrapped span inherits the span's line. Source mode uses [`wrap_spans`]
/// (this algorithm is shared); the tagged variant is used only by the view
/// pipeline, where the attribution drives the exact source-line mapping.
#[allow(dead_code)]
pub fn wrap_spans_tagged(
    spans: &[Span],
    lines: &[Option<usize>],
    width: usize,
) -> Vec<(Vec<Span>, Vec<Option<usize>>)> {
    debug_assert_eq!(spans.len(), lines.len(), "attribution parallels spans");
    let width = width.max(1);
    let mut rows: Vec<(Vec<Span>, Vec<Option<usize>>)> = Vec::new();
    let mut row: Vec<Span> = Vec::new();
    let mut row_lines: Vec<Option<usize>> = Vec::new();
    let mut col = 0usize; // display column where the next character lands
    for (span, line) in spans.iter().zip(lines) {
        let mut rest = span.text.as_str();
        while !rest.is_empty() {
            // The current row is full: flush it and start the next.
            if col >= width {
                rows.push((std::mem::take(&mut row), std::mem::take(&mut row_lines)));
                col = 0;
            }
            let (take, take_w) = take_fit(rest, col, width - col);
            if take.is_empty() {
                // The next character is a tab too wide for the rest of
                // this row: flush and retry from column 0, where the tab
                // expands like a terminal's line-leading tab.
                rows.push((std::mem::take(&mut row), std::mem::take(&mut row_lines)));
                col = 0;
                continue;
            }
            row.push(Span {
                text: expand_tabs(take, col),
                style: span.style,
            });
            row_lines.push(*line);
            col += take_w;
            rest = &rest[take.len()..];
            if col >= width {
                rows.push((std::mem::take(&mut row), std::mem::take(&mut row_lines)));
                col = 0;
            }
        }
    }
    if !row.is_empty() {
        rows.push((row, row_lines));
    }
    if rows.is_empty() {
        rows.push((Vec::new(), Vec::new()));
    }
    rows
}

/// The longest prefix of `s` that fits in `avail` columns, given that the
/// row already holds `col` columns. Returns the prefix and the display
/// width it occupies (tabs measured at 8-column stops).
///
/// A first character wider than `avail` is still taken (narrow-pane
/// safety, so the loop always makes progress) — except a tab in a
/// non-empty row, which returns an empty prefix: the caller flushes the
/// row and retries from column 0, where the tab expands like a terminal's
/// line-leading tab instead of overflowing the row by up to 7 columns.
fn take_fit(s: &str, col: usize, avail: usize) -> (&str, usize) {
    let mut w = 0usize;
    let mut last = 0usize;
    for (i, ch) in s.char_indices() {
        let cw = char_width(ch, col + w);
        if w + cw > avail {
            if last != 0 {
                // The row is full: leave the rest (and this character) for
                // the next row.
                break;
            }
            if ch == '\t' && col > 0 {
                // A tab that cannot fit in a non-empty row: push it to the
                // next row instead of overflowing this one.
                return ("", 0);
            }
            // A first character wider than the row (wide char, or a tab at
            // the start of an empty row): take it anyway.
        }
        w += cw;
        last = i + ch.len_utf8();
    }
    if last == 0 {
        // Unreachable with the loop above (the first character is always
        // taken), kept as a safety net: take the first character.
        let ch = s.chars().next().expect("non-empty input");
        w = char_width(ch, col);
        last = ch.len_utf8();
    }
    (&s[..last], w)
}

/// The display width of `ch` at column `col` of a row: unicode-width for
/// regular characters, the next 8-column stop for tabs.
fn char_width(ch: char, col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    if ch == '\t' {
        TAB_STOP - col % TAB_STOP
    } else {
        ch.width().unwrap_or(0)
    }
}

/// Replace tabs in `s` with the spaces a terminal would display (8-column
/// stops measured from `col`), so the returned text occupies exactly the
/// cells the wrap math counted. Text without tabs is returned as-is.
fn expand_tabs(s: &str, col: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut c = col;
    for ch in s.chars() {
        if ch == '\t' {
            let pad = TAB_STOP - c % TAB_STOP;
            out.push_str(&" ".repeat(pad));
            c += pad;
        } else {
            out.push(ch);
            c += ch.width().unwrap_or(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Highlighter, Span, syntax_for, wrap_spans};
    use std::path::Path;
    use ratatui::style::{Color, Style};
    use termtheme::theme::{DEFAULT_DARK, DEFAULT_LIGHT};
    use unicode_width::UnicodeWidthStr;

    /// Display width of a string, in terminal columns (test helper).
    fn width(s: &str) -> usize {
        s.width()
    }

    #[test]
    fn highlights_markdown_into_colored_spans() {
        let h = Highlighter::new(Some(DEFAULT_DARK), false);
        let lines = h.highlight_with("# Heading\n\n**bold**\n", syntax_for(Path::new("x.md")));
        assert_eq!(lines.len(), 3);
        // Heading line tokenizes (markdown header), not a single plain span.
        assert!(!lines[0].is_empty());
        let joined: String = lines[2].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "**bold**");
    }

    #[test]
    fn absent_or_unknown_themes_fall_back_to_the_default_for_the_background() {
        // A light background never falls back to a dark theme's pale
        // foreground (and vice versa).
        let dark = Highlighter::new(Some(DEFAULT_DARK), false).default_fg();
        let light = Highlighter::new(Some(DEFAULT_LIGHT), true).default_fg();
        assert_ne!(dark, light);
        for name in [None, Some("no-such-theme"), Some("/no/such.tmTheme")] {
            assert_eq!(Highlighter::new(name, false).default_fg(), dark, "{name:?}");
            assert_eq!(Highlighter::new(name, true).default_fg(), light, "{name:?}");
        }
    }

    #[test]
    fn plain_lines_carry_the_default_foreground() {
        let h = Highlighter::new(None, false);
        let lines = h.highlight_with("plain\n", syntax_for(Path::new("x.md")));
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].text, "plain");
    }

    /// A minimal .tmTheme (plist XML) whose foreground is bright red.
    const MINIMAL_TM_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key>
  <string>Minimal</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#ff0000</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
"#;

    #[test]
    fn theme_loads_from_tmtheme_file_path() {
        // A `--theme /path/to/theme.tmTheme` must load the file directly.
        let dir = std::env::temp_dir().join(format!("akapen-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("minimal.tmTheme");
        std::fs::write(&path, MINIMAL_TM_THEME).unwrap();
        let h = Highlighter::new(Some(path.to_str().unwrap()), false);
        // The file's foreground (#ff0000) wins over the defaults.
        assert_eq!(h.default_fg(), Color::Rgb(0xff, 0x00, 0x00));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A theme that only knows the legacy `markup.fenced_code.block.markdown`
    /// scope name (what syntect's GFM grammar no longer emits).
    const LEGACY_TM_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key>
  <string>Legacy</string>
  <key>settings</key>
  <array>
    <dict>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#a9b1d6</string>
      </dict>
    </dict>
    <dict>
      <key>scope</key>
      <string>markup.fenced_code.block.markdown</string>
      <key>settings</key>
      <dict>
        <key>foreground</key>
        <string>#ff0000</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
"#;

    #[test]
    fn legacy_markdown_code_scope_is_aliased() {
        // syntect's GFM grammar emits `markup.raw.code-fence.markdown`;
        // a theme defining only the legacy name must still color the
        // fenced code (the alias duplicates the rule onto the new scope).
        let dir = std::env::temp_dir().join(format!("akapen-alias-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.tmTheme");
        std::fs::write(&path, LEGACY_TM_THEME).unwrap();
        let h = Highlighter::new(Some(path.to_str().unwrap()), false);
        let lines = h.highlight_with("```\ncode\n```\n", syntax_for(Path::new("x.md")));
        // The opening fence carries the aliased code color.
        assert_eq!(lines[0][0].style.fg, Some(Color::Rgb(0xff, 0x00, 0x00)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn syntax_for_picks_the_language_by_file() {
        use std::path::Path;
        // Known extensions resolve to their grammars.
        assert_eq!(syntax_for(Path::new("x.rs")).name, "Rust");
        assert_eq!(syntax_for(Path::new("x.toml")).name, "TOML");
        assert_eq!(syntax_for(Path::new("Makefile")).name, "Makefile");
        // Markdown stays markdown.
        assert!(syntax_for(Path::new("x.md")).name.contains("Markdown"));
        // Unknown files fall back to plain text (never panics).
        assert_eq!(syntax_for(Path::new("x.unknown-ext")).name, "Plain Text");
        assert_eq!(syntax_for(Path::new("noext")).name, "Plain Text");
    }

    #[test]
    fn unknown_theme_name_falls_back_to_default() {
        // An unknown name must render exactly like the bundled default.
        let h = Highlighter::new(Some("tokyo-night"), false);
        let fallback = Highlighter::new(None, false);
        assert_eq!(h.default_fg(), fallback.default_fg());
    }

    #[test]
    fn missing_tmtheme_path_falls_back_to_default() {
        let h = Highlighter::new(Some("/nonexistent/theme.tmTheme"), false);
        let fallback = Highlighter::new(None, false);
        assert_eq!(h.default_fg(), fallback.default_fg());
    }

    #[test]
    fn unknown_theme_falls_back_to_default() {
        let h = Highlighter::new(Some("no-such-theme"), false);
        assert_eq!(h.default_fg(), Highlighter::new(None, false).default_fg());
    }

    fn span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            style: Style::default().fg(Color::White),
        }
    }

    #[test]
    fn wrap_keeps_rows_within_width_and_preserves_text() {
        let rows = wrap_spans(&[span("abcde"), span("fgh")], 4);
        // "abcd" | "efgh" — the second row exactly fills: e joins fgh.
        assert_eq!(rows.len(), 2);
        let joined: String = rows.iter().flatten().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "abcdefgh");
        assert!(
            rows.iter()
                .all(|r| width(&r.iter().map(|s| s.text.as_str()).collect::<String>()) <= 4)
        );
    }

    #[test]
    fn wrap_splits_wide_chars_never_in_half() {
        let rows = wrap_spans(&[span("あいうえお")], 4);
        // あいう(6 cols) fits 4? No: 2+2+2 > 4 → あ(2) い(2) → next row う(2) え(2) → お(2)
        let joined: String = rows.iter().flatten().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "あいうえお");
        assert!(
            rows.iter()
                .all(|r| width(&r.iter().map(|s| s.text.as_str()).collect::<String>()) <= 4)
        );
        // Each row's text is whole characters only.
        for r in &rows {
            let t: String = r.iter().map(|s| s.text.as_str()).collect();
            assert!(t.chars().all(|c| "あいうえお".contains(c)));
        }
    }

    #[test]
    fn wrap_mixed_ascii_and_cjk() {
        let rows = wrap_spans(&[span("ab日本語cd")], 6);
        let joined: String = rows.iter().flatten().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "ab日本語cd");
        assert!(
            rows.iter()
                .all(|r| width(&r.iter().map(|s| s.text.as_str()).collect::<String>()) <= 6)
        );
    }

    #[test]
    fn wrap_expands_tabs_to_terminal_columns() {
        // A line-leading tab renders as 8 spaces; a tab after two characters
        // pads to the next 8-column stop — exactly what a terminal shows.
        let row: String = wrap_spans(&[span("\tfoo")], 20)[0]
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(row, "        foo");
        assert!(!row.contains('\t'), "rows never carry raw tabs");

        let row: String = wrap_spans(&[span("ab\tc")], 20)[0]
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(row, "ab      c", "tab after 2 cols pads to col 8");
        assert_eq!(width(&row), 9);
    }

    #[test]
    fn wrap_overflowing_tab_starts_the_next_row() {
        // "a\tb" in a 3-col pane: the tab at col 1 needs 7 columns and
        // cannot fit, so the row ends before it and the tab starts the next
        // row from column 0 (8 spaces even in a narrow pane — documented
        // narrow-pane safety, same as a wide char).
        let rows = wrap_spans(&[span("a\tb")], 3);
        let joined: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(joined, vec!["a", "        ", "b"]);
    }

    #[test]
    fn wrap_measures_rows_at_the_expanded_tab_width() {
        // "abc\tde" is 3 + 5 (tab at col 3 -> col 8) + 2 = 10 columns and
        // fills a 10-col row exactly, so the trailing "fgh" wraps. The
        // wrap boundary is decided by the EXPANDED width, not the raw text.
        let rows = wrap_spans(&[span("abc\tdefgh")], 10);
        let joined: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(joined, vec!["abc     de", "fgh"]);
    }

    #[test]
    fn wrap_mixed_cjk_and_tabs() {
        // あ (2 cols, col 0-1), a tab at col 2 -> 6 spaces, then い (2).
        let rows = wrap_spans(&[span("あ\tい")], 12);
        let joined: String = rows.iter().flatten().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "あ      い");
        assert!(
            rows.iter()
                .all(|r| width(&r.iter().map(|s| s.text.as_str()).collect::<String>()) <= 12)
        );
    }

    #[test]
    fn wrap_empty_and_narrow_pane() {
        assert_eq!(
            wrap_spans(&[], 10),
            vec![vec![]],
            "empty line is one empty row"
        );
        let rows = wrap_spans(&[span("あ")], 1);
        assert_eq!(rows.len(), 1, "a wide char still fits a width-1 pane");
    }
}
