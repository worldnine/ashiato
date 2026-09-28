//! 設定ファイル — `$XDG_CONFIG_HOME/ashiato/config.toml`（無ければ `~/.config/…`）。
//!
//! ```toml
//! [theme]
//! dark  = "Catppuccin Mocha"     # two-face の名前か .tmTheme のパス
//! light = "Solarized (light)"
//! ```
//!
//! どのキーも省略できる。**ファイルが無ければ、今までと完全に同じ動作である。**
//! 優先は**フラグ > 設定ファイル > 既定**。層を重ねるのは
//! `Config::parse_with_file`（`src/main.rs`）の仕事で、ここは読むだけ。
//!
//! 形は akapen の `src/config_file.rs` に揃えてある（キーは `[theme]` だけ）。
//!
//! # 壊れたファイルは起動時のコマンドラインエラー
//!
//! 構文の誤り・型違い・**知らないキー**は、ファイルのパスを添えて止まる。
//! 知らないキーを止めるのは、タイプミス（`[theme] drak = …`）を黙って無視
//! しないため（フラグのタイプミスを止めるのと同じ方針）。代わりに、**新しい
//! キーを書いた設定を古い ashiato が読むと起動しない。**
//!
//! # 値の扱い
//!
//! - 空・空白だけの値は、書いていないのと同じ
//! - `[theme]` の値が `~/` で始まれば展開する（`.tmTheme` のパス用）

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// 設定ファイルの名前（[`user_dir`] の下）。
const FILE_NAME: &str = "config.toml";

/// ashiato の設定ディレクトリ: `$XDG_CONFIG_HOME/ashiato`、無ければ
/// `~/.config/ashiato`。
///
/// 空の `XDG_CONFIG_HOME` は無いのと同じ。`HOME` も無ければ `None`
/// （置き場が決まらない = 何も置いていない）。
fn user_dir() -> Option<PathBuf> {
    user_dir_with(|name| std::env::var_os(name))
}

/// [`user_dir`] の本体。環境変数 1 つを引く関数を受け取る（テストで注入する）。
fn user_dir_with(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let base = match env("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env("HOME")?).join(".config"),
    };
    Some(base.join("ashiato"))
}

/// 読んだ設定ファイル。値は空・空白を落とし、`~/` を展開した後のもの。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigFile {
    /// `[theme] dark`（`--theme-dark` の下の層）。
    pub theme_dark: Option<String>,
    /// `[theme] light`（`--theme-light` の下の層）。
    pub theme_light: Option<String>,
}

/// ファイルの形そのもの。**知らないキーは断る**（冒頭の doc）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    theme: Option<RawTheme>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    dark: Option<String>,
    light: Option<String>,
}

/// 空・空白だけは書いていないのと同じ。
fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// `~/` で始まるなら `home` の下に展開する。`home` が無ければそのまま。
fn expand_home(value: String, home: Option<&Path>) -> String {
    match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => value,
    }
}

impl ConfigFile {
    /// 中身から読む（**ファイルも環境も読まない**）。`path` はエラーの表示に、
    /// `home` は `~/` の展開に使う。
    pub fn parse(path: &Path, text: &str, home: Option<&Path>) -> Result<Self> {
        let raw: Raw = toml::from_str(text).with_context(|| path.display().to_string())?;
        let theme = raw.theme.unwrap_or_default();
        Ok(Self {
            theme_dark: non_blank(theme.dark).map(|v| expand_home(v, home)),
            theme_light: non_blank(theme.light).map(|v| expand_home(v, home)),
        })
    }

    /// `path` のファイルを読む。無いのはエラーではない（`Ok(None)`）。あるのに
    /// 読めない・壊れている、はエラーである。
    fn load(path: &Path, home: Option<&Path>) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        Self::parse(path, &text, home).map(Some)
    }

    /// 置き場（[`user_dir`]）のファイルを読む。**実環境と実ファイルを読むのは
    /// ここだけ**（`Config::from_env` から呼ぶ）。
    pub fn discover() -> Result<Option<Self>> {
        let Some(path) = user_dir().map(|dir| dir.join(FILE_NAME)) else {
            return Ok(None);
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::load(&path, home.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ConfigFile> {
        ConfigFile::parse(
            Path::new("/cfg/ashiato/config.toml"),
            text,
            Some(Path::new("/home/u")),
        )
    }

    /// エラーの全文（anyhow の文脈と原因を全部つなげたもの）。
    fn error(text: &str) -> String {
        match parse(text) {
            Ok(file) => panic!("{text:?} が通ってしまった: {file:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn the_directory_is_xdg_config_home_else_dot_config_under_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            user_dir_with(env(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")])),
            Some(PathBuf::from("/xdg/ashiato"))
        );
        // 空の XDG_CONFIG_HOME は無いのと同じ。
        assert_eq!(
            user_dir_with(env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.config/ashiato"))
        );
        assert_eq!(
            user_dir_with(env(&[("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.config/ashiato"))
        );
        assert_eq!(user_dir_with(env(&[])), None, "置き場が決まらない");
    }

    #[test]
    fn an_empty_file_sets_nothing() {
        let file = parse("").unwrap();
        assert_eq!(file, ConfigFile::default());
        // `[theme]` だけあって中身が無いのも同じ。
        assert_eq!(parse("[theme]\n").unwrap(), file);
    }

    #[test]
    fn both_theme_keys_read() {
        let file = parse(
            r#"
[theme]
dark  = "Catppuccin Mocha"
light = "Catppuccin Latte"
"#,
        )
        .unwrap();
        assert_eq!(file.theme_dark.as_deref(), Some("Catppuccin Mocha"));
        assert_eq!(file.theme_light.as_deref(), Some("Catppuccin Latte"));
        // 片側だけでもよい。
        let file = parse("[theme]\nlight = \"Catppuccin Latte\"\n").unwrap();
        assert_eq!(file.theme_dark, None);
        assert_eq!(file.theme_light.as_deref(), Some("Catppuccin Latte"));
    }

    #[test]
    fn a_theme_path_under_home_is_expanded() {
        let file = parse(concat!(
            "[theme]\n",
            "dark = \"~/themes/night.tmTheme\"\n",
            "light = \"Solarized (light)\"\n",
        ))
        .unwrap();
        assert_eq!(
            file.theme_dark.as_deref(),
            Some("/home/u/themes/night.tmTheme")
        );
        assert_eq!(file.theme_light.as_deref(), Some("Solarized (light)"));
        // `~` 単独や `~user/` は展開しない（`~/` だけ）。HOME が無ければそのまま。
        let file = parse("[theme]\ndark = \"~other/x.tmTheme\"\n").unwrap();
        assert_eq!(file.theme_dark.as_deref(), Some("~other/x.tmTheme"));
        let file = ConfigFile::parse(Path::new("c.toml"), "[theme]\ndark = \"~/x.tmTheme\"", None)
            .unwrap();
        assert_eq!(file.theme_dark.as_deref(), Some("~/x.tmTheme"));
    }

    #[test]
    fn blank_values_are_the_same_as_not_writing_them() {
        let file = parse("[theme]\ndark = \"\"\nlight = \"  \"\n").unwrap();
        assert_eq!(file, ConfigFile::default());
    }

    #[test]
    fn an_unknown_key_is_an_error_that_names_the_file_and_the_key() {
        // タイプミスを黙って無視しない。
        let err = error("[theme]\ndrak = \"Dracula\"\n");
        assert!(err.contains("/cfg/ashiato/config.toml"), "{err}");
        assert!(err.contains("drak"), "{err}");
        // 表の外のキーも、知らない表も同じ。
        let err = error("theme_dark = \"Dracula\"\n");
        assert!(err.contains("/cfg/ashiato/config.toml"), "{err}");
        assert!(err.contains("theme_dark"), "{err}");
        let err = error("[colors]\nborder = 1\n");
        assert!(err.contains("colors"), "{err}");
    }

    #[test]
    fn a_wrong_type_is_an_error_that_names_the_file_and_the_key() {
        for (text, key) in [
            ("theme = \"Dracula\"\n", "theme"),
            ("[theme]\ndark = 3\n", "dark"),
            ("[theme]\nlight = [\"a\"]\n", "light"),
        ] {
            let err = error(text);
            assert!(err.contains("/cfg/ashiato/config.toml"), "{err}");
            // toml のエラーは該当行を引用する（キーの名前が入る）。
            assert!(err.contains(key), "{key}: {err}");
        }
    }

    #[test]
    fn broken_syntax_is_an_error_that_names_the_file() {
        let err = error("[theme\ndark = \"x\"\n");
        assert!(err.contains("/cfg/ashiato/config.toml"), "{err}");
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn load_reads_a_real_file_and_a_missing_one_is_not_an_error() {
        // 実環境（~/.config/ashiato/）には触らない。tempdir と明示的なパスだけ。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let home = Path::new("/home/u");
        assert_eq!(ConfigFile::load(&path, Some(home)).unwrap(), None);

        std::fs::write(&path, "[theme]\ndark = \"~/night.tmTheme\"\n").unwrap();
        let file = ConfigFile::load(&path, Some(home)).unwrap().unwrap();
        assert_eq!(file.theme_dark.as_deref(), Some("/home/u/night.tmTheme"));
        assert_eq!(file.theme_light, None);

        // 壊れた中身は、実ファイルのパスを添えたエラー。
        std::fs::write(&path, "[theme]\ndrak = \"x\"\n").unwrap();
        let err = format!("{:#}", ConfigFile::load(&path, Some(home)).unwrap_err());
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("drak"), "{err}");
    }
}
