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
        p.header = path.display().to_string();
        p.rows.push(Line::from(TuiSpan::styled(
            "(directory)",
            Style::default().fg(Color::DarkGray),
        )));
        return p;
    }
    match read_head(path) {
        Some((content, read_truncated)) => {
            // Only the lines the pane can show are highlighted: each
            // source line yields at least one display row, so line
            // height+1 can never appear. The grammar context builds from
            // the top, so cutting the tail is safe — without the cut a
            // 256KB head is tokenized just to throw all but a dozen rows
            // away (visible stutter on every cursor move).
            let (head, line_truncated) = head_lines(&content, height);
            let spans = hl.highlight_with(head, syntax_for(path));
            let mut p = Preview::default();
            p.header = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
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
            p.header = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
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

/// The first `n` lines of `s` (with their newlines) and whether anything
/// was cut.
fn head_lines(s: &str, n: usize) -> (&str, bool) {
    let mut seen = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen >= n {
                return (&s[..i + 1], i + 1 < s.len());
            }
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
        assert_eq!(head_lines("a\nb\nc\n", 2), ("a\nb\n", true));
        assert_eq!(head_lines("a\nb\n", 2), ("a\nb\n", false));
        assert_eq!(head_lines("a\nb", 5), ("a\nb", false));
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
