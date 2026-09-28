//! light/dark の UI の色（akapen と同じ定数）。
//!
//! どちらの側を使うかは、起動時に `--light` / `--dark`、無ければ端末の背景色
//! （OSC 11、`termtheme::background`）で決め、開いている間は端末の配色の知らせ
//! （モード 2031）に追従する（main.rs の `App::follow_scheme`）。背景色の判定は
//! termtheme へ移した（akapen・herdr-kit と写し合っていたもの）。ここに残るのは
//! ashiato の UI の色だけで、akapen の `view.rs`（`selected_bg` / `border_color`）
//! の定数の写し — 2 つのツールが同じ色で入れ替わるように。

use ratatui::style::Color;

/// Selection/cursor background: akapen's `selected_bg` constants
/// (view.rs). Dark = neutral gray a step brighter than ANSI
/// bright-black; light = pale cool gray.
const SELECTED_BG_DARK: Color = Color::Rgb(88, 91, 112);
const SELECTED_BG_LIGHT: Color = Color::Rgb(210, 210, 220);
/// Outer border: akapen's `border_color` constants (view.rs).
const BORDER_DARK: Color = Color::Rgb(127, 132, 156);
const BORDER_LIGHT: Color = Color::Rgb(180, 180, 190);

/// akapen's `selected_bg(light)` — same values, same semantics.
pub fn selected_bg(light: bool) -> Color {
    if light { SELECTED_BG_LIGHT } else { SELECTED_BG_DARK }
}

/// akapen's `border_color(light)`.
pub fn border_color(light: bool) -> Color {
    if light { BORDER_LIGHT } else { BORDER_DARK }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_colors_follow_akapen_constants() {
        assert_eq!(selected_bg(false), Color::Rgb(88, 91, 112));
        assert_eq!(selected_bg(true), Color::Rgb(210, 210, 220));
        assert_eq!(border_color(false), Color::Rgb(127, 132, 156));
        assert_eq!(border_color(true), Color::Rgb(180, 180, 190));
    }
}
