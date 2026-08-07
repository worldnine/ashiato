//! Preview pane: syntect-highlighted file head (akapen's highlighter),
//! or file(1)-style info for binary/image files. Not scrollable (spec:
//! ashiato is a picker — deep previews belong to akapen).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as TuiSpan};

use crate::highlight::{Highlighter, Span, syntax_for, wrap_spans};

/// How much of a text file the preview will read at most (spec: show the
/// head that fits the pane; huge files must not stall the UI).
const MAX_PREVIEW_BYTES: usize = 256 * 1024;
/// Binary sniff window: NUL in the first 8 KB means "not text".
const BINARY_SNIFF: usize = 8192;

/// One rendered preview: styled rows ready to draw, plus the pane header.
#[derive(Debug, Default)]
pub struct Preview {
    /// The header line (file name / file(1) info for binaries).
    pub header: String,
    /// Body rows (may be empty).
    pub rows: Vec<Line<'static>>,
}

/// Cache key: the preview is re-rendered only when the file, its size or
/// mtime, or the pane geometry changes (e.g. after akapen edited it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewKey {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
    pub width: usize,
    pub height: usize,
}

/// The preview pane's header text: the basename for files, the full path
/// for directories (shared by the renderer and the TUI's speed-first
/// placeholder, so both always agree on what the pane is showing).
pub fn header_for(path: &Path, is_dir: bool) -> String {
    if is_dir {
        path.display().to_string()
    } else {
        path.file_name()
            .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// Render the preview for `entry`, or `None` when nothing is selected.
pub fn render(
    path: &Path,
    size: u64,
    is_dir: bool,
    width: usize,
    height: usize,
    hl: &Highlighter,
) -> Preview {
    let width = width.max(10);
    let height = height.max(1);
    if is_dir {
        let mut p = Preview::default();
        p.header = header_for(path, true);
        p.rows.push(Line::from(TuiSpan::styled(
            "(directory)",
            Style::default().fg(Color::DarkGray),
        )));
        return p;
    }
    match read_head(path) {
        Some((content, read_truncated)) => {
            // Only the cells the pane can show are highlighted: the head
            // is cut at a line boundary once `width*height` characters
            // are used up (and mid-line when a single line overflows the
            // budget — minified bundles). The grammar context builds
            // from the top, so cutting the tail is safe — without the
            // cut a 256KB head is tokenized just to throw all but a
            // dozen rows away (visible stutter on every cursor move:
            // 1.5s on a minified bundle).
            let (head, line_truncated) = head_lines_by_cells(&content, height, width);
            let spans = hl.highlight_with(head, syntax_for(path));
            let mut p = Preview::default();
            p.header = header_for(path, false);
            'wrap: for line in spans {
                for row in wrap_spans(&line, width) {
                    if p.rows.len() >= height {
                        break 'wrap;
                    }
                    p.rows.push(to_tui_line(&row));
                }
            }
            if read_truncated || line_truncated {
                // The marker must stay visible: it replaces the last row
                // when the pane is already full.
                p.rows.truncate(height.saturating_sub(1));
                p.rows.push(Line::from(TuiSpan::styled(
                    "… (truncated)",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            p
        }
        None => {
            // Binary: file(1)-style info.
            let mut p = Preview::default();
            p.header = header_for(path, false);
            p.rows.push(Line::from(TuiSpan::styled(
                binary_info(path, size),
                Style::default().fg(Color::DarkGray),
            )));
            p
        }
    }
}

/// Read the file head as UTF-8; `None` = unreadable or binary (NUL in the
/// sniff window). The flag reports whether the file continues past the
/// read window.
fn read_head(path: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(MAX_PREVIEW_BYTES.min(64 * 1024));
    f.by_ref().take(MAX_PREVIEW_BYTES as u64 + 1).read_to_end(&mut buf).ok()?;
    let sniff = &buf[..buf.len().min(BINARY_SNIFF)];
    if sniff.contains(&0) {
        return None;
    }
    let truncated = buf.len() > MAX_PREVIEW_BYTES;
    if truncated {
        buf.truncate(MAX_PREVIEW_BYTES);
        // The byte-bounded read may have cut a UTF-8 sequence mid-char;
        // from_utf8_lossy would render the stump as '�'.
        trim_partial_utf8(&mut buf);
    }
    Some((String::from_utf8_lossy(&buf).into_owned(), truncated))
}

/// Drop a trailing UTF-8 sequence that the byte cut left incomplete
/// (continuation bytes without enough of them for their lead byte).
fn trim_partial_utf8(buf: &mut Vec<u8>) {
    let mut i = buf.len();
    while i > 0 && buf[i - 1] & 0xC0 == 0x80 {
        i -= 1;
    }
    if i == 0 {
        return;
    }
    let need = match buf[i - 1] {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => return, // ASCII tail: nothing was cut
    };
    if buf.len() - (i - 1) < need {
        buf.truncate(i - 1);
    }
}

/// The head of `s` that fits the pane: at most `height` lines, at most
/// `width*height` characters total, and no line longer than `width*8`
/// chars (cut mid-line when one overflows — minified bundles). The
/// grammar builds from the top, so cutting the tail is safe; without the
/// cuts a 256KB head is tokenized just to throw all but a dozen rows
/// away, and syntect's per-line cost is superlinear, so a giant line
/// must not be tokenized in full (a minified bundle was 1.5s, a markdown
/// file with many short lines is bounded by the line count). Returns the
/// cut and whether anything was cut.
fn head_lines_by_cells(s: &str, height: usize, width: usize) -> (&str, bool) {
    let budget = (height * width).max(1);
    // One source line may wrap to at most this many rows.
    let line_cap = (width * 8).max(1);
    let mut used = 0usize;
    let mut cut = 0usize;
    let mut lines = 0usize;
    for line in s.split_inclusive('\n') {
        if lines >= height {
            return (&s[..cut], true);
        }
        let total = line.chars().count();
        let take = total.min(line_cap);
        if used + take > budget {
            if cut == 0 {
                // The very first line overflows the whole pane: keep its
                // head (char-aligned) so syntect never sees the tail.
                let take = line
                    .char_indices()
                    .nth(budget)
                    .map_or(line.len(), |(i, _)| i);
                return (&s[..take], true);
            }
            return (&s[..cut], true);
        }
        used += take;
        // Advance by the bytes of the kept prefix (capped lines stop here).
        cut += line
            .char_indices()
            .nth(take)
            .map_or(line.len(), |(i, _)| i);
        lines += 1;
        if take < total {
            return (&s[..cut], true);
        }
    }
    (s, false)
}

/// file(1)-style one-liner, e.g. `PNG image data, 640 x 480, 24-bit/color
/// RGBA, non-interlaced, 24KB`. Uses `file -b` when available, else a tiny
/// magic-byte fallback so the preview never crashes and never blocks.
fn binary_info(path: &Path, size: u64) -> String {
    let kind = match Command::new("file").arg("-b").arg(path).output() {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() { magic_info(path) } else { s }
        }
        _ => magic_info(path),
    };
    format!("{kind}, {}KB", (size / 1024).max(1))
}

/// Magic-byte fallback for the common formats, without spawning `file`.
fn magic_info(path: &Path) -> String {
    let mut head = [0u8; 16];
    if std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut head)
        })
        .is_err()
    {
        return "binary file".to_string();
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        "PNG image".to_string()
    } else if head.starts_with(b"\xff\xd8\xff") {
        "JPEG image".to_string()
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "GIF image".to_string()
    } else if head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        "WebP image".to_string()
    } else if head.starts_with(b"%PDF") {
        "PDF document".to_string()
    } else if head.starts_with(b"PK\x03\x04") {
        "ZIP archive".to_string()
    } else {
        "binary file".to_string()
    }
}

/// Convert a highlight span row into a ratatui `Line`.
pub fn to_tui_line(row: &[Span]) -> Line<'static> {
    Line::from(
        row.iter()
            .map(|s| TuiSpan::styled(s.text.clone(), s.style))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;

    #[test]
    fn text_head_is_highlighted_and_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.rs");
        std::fs::write(&p, "fn main() {}\n// note\n").unwrap();
        let hl = Highlighter::new(None, false);
        let prev = render(&p, 25, false, 40, 20, &hl);
        assert_eq!(prev.header, "x.rs");
        assert_eq!(prev.rows.len(), 2);
        assert!(prev.rows[0].to_string().contains("fn main() {}"));
        assert!(prev.rows[1].to_string().contains("// note"));
    }

    #[test]
    fn binary_head_reports_file_info_without_crashing() {
        let dir = tempfile::tempdir().unwrap();
        // A 1x1 PNG, real magic bytes.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        let p = dir.path().join("img.png");
        std::fs::write(&p, png).unwrap();
        let hl = Highlighter::new(None, false);
        let prev = render(&p, png.len() as u64, false, 40, 20, &hl);
        assert!(prev.rows[0].to_string().contains("PNG"), "{}", prev.rows[0]);
        assert!(prev.rows[0].to_string().contains("KB"));
    }

    #[test]
    fn directory_preview_is_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let hl = Highlighter::new(None, false);
        let prev = render(dir.path(), 0, true, 40, 20, &hl);
        assert!(prev.rows[0].to_string().contains("directory"));
    }

    #[test]
    fn rows_are_limited_to_the_pane_height() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("many.txt");
        std::fs::write(&p, (0..50).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let hl = Highlighter::new(None, false);
        let prev = render(&p, 400, false, 80, 10, &hl);
        assert!(prev.rows.len() <= 10);
    }

    #[test]
    fn head_lines_cuts_at_the_pane_height() {
        // Whole lines up to the cell budget, cut at the next boundary.
        assert_eq!(head_lines_by_cells("a\nb\nc\nd\n", 2, 2), ("a\nb\n", true));
        // Budget boundary lands exactly on a line end: no phantom cut.
        assert_eq!(head_lines_by_cells("ab\ncd\n", 2, 3), ("ab\ncd\n", false));
        assert_eq!(head_lines_by_cells("a\nb\n", 5, 10), ("a\nb\n", false));
    }

    #[test]
    fn head_lines_bounds_long_lines_by_the_pane_area() {
        // A minified one-liner: capped to the pane budget (the
        // superlinear syntect cost is what made it a 1.5s render), and
        // the line cap keeps each source line to 8 rows of the width.
        let s = "x".repeat(10_000);
        let (head, cut) = head_lines_by_cells(&s, 5, 10);
        assert!(cut);
        assert_eq!(head.chars().count(), 50); // the pane budget (5x10)
        // CJK wide chars count as chars; the wrap loop caps the rows.
        let s = format!("{}\n", "あ".repeat(100));
        let (head, cut) = head_lines_by_cells(&s, 10, 5);
        assert!(cut);
        assert_eq!(head.chars().count(), 40); // width*8 = 5*8
    }

    #[test]
    fn trim_partial_utf8_drops_a_cut_sequence() {
        // "日" is e6 97 a5; cutting after two bytes leaves a stump.
        let mut buf = b"ab\xe6\x97".to_vec();
        trim_partial_utf8(&mut buf);
        assert_eq!(buf, b"ab");
        // A complete sequence survives.
        let mut buf = "ab日".as_bytes().to_vec();
        trim_partial_utf8(&mut buf);
        assert_eq!(buf, "ab日".as_bytes());
        // Pure ASCII survives.
        let mut buf = b"abc".to_vec();
        trim_partial_utf8(&mut buf);
        assert_eq!(buf, b"abc");
    }

    #[test]
    fn missing_file_renders_binary_placeholder() {
        let hl = Highlighter::new(None, false);
        let prev = render(Path::new("/nonexistent/ashiato-xyz"), 0, false, 40, 20, &hl);
        assert_eq!(prev.rows.len(), 1); // file(1)-style line, no panic
    }
}
