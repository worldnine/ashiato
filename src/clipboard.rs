//! Clipboard support (ported from akapen's `export.rs`, MIT reviewr).
//!
//! `y` copies the selected files' full paths: pipe into the first
//! available tool — `pbcopy` (macOS), `wl-copy` (Wayland), `xclip`/`xsel`
//! (X11).

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Copy `text` into the system clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let (cmd, args) = CLIPBOARD_TOOLS
        .iter()
        .copied()
        .find(|(cmd, _)| which(cmd))
        .context("no clipboard tool found (install wl-clipboard, xclip, or xsel)")?;
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {cmd}"))?;
    child
        .stdin
        .as_mut()
        .with_context(|| format!("{cmd} stdin unavailable"))?
        .write_all(text.as_bytes())
        .with_context(|| format!("writing to {cmd}"))?;
    if !child
        .wait()
        .with_context(|| format!("waiting for {cmd}"))?
        .success()
    {
        bail!("{cmd} exited non-zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_real_binaries_and_rejects_fakes() {
        assert!(which("sh"), "sh exists on every Unix PATH");
        assert!(!which("definitely-not-a-real-tool-ashiato-xyz"));
    }
}
