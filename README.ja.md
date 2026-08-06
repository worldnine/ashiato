# ashiato

**ashiato**（足跡）— エージェントが触ったファイルの足跡を辿る。

English README is [here](README.md).

プロジェクト全体のファイルを更新順（mtime 降順）でフラット表示する TUI ピッカー。エージェントが編集したばかりのファイルが一番上に浮かび、日ごとのクラスタ（Today / Yesterday、それ以前は日付見出し）で区切られ、シンタックスハイライト付きプレビューで中身を確かめられます。選んだファイルのパスを stdout に出力するか、[akapen](https://github.com/worldnine/akapen) のようなレビュアーへそのまま渡せます。

ashiato は「**時間ブラウザ**」であり、検索ツールではありません。デフォルトビューは常に「最近変わったもの」。ファジー検索は fzf の領分で、ashiato は fzf と相互運用します（後述）。

## インストール

```sh
cargo install ashiato
```

ソースからビルドする場合:

```sh
cargo build --release
# バイナリ: target/release/ashiato
```

## 使い方

```
ashiato [directory] [flags]
```

既定では `Enter` が選択パスを stdout に出力して終了します（fzf モデル）。stdout がパイプの場合、TUI は `/dev/tty` に描画されるため、`ashiato . | xargs akapen` がエスケープ列の混入なしで動きます。

| フラグ | 説明 |
|---|---|
| `--sort mtime\|ctime` | 初期ソート基準（`t` で mtime↓ → mtime↑ → ctime↓ → ctime↑ を巡回） |
| `--show-hidden` | 起動時にドットディレクトリを表示（`Ctrl+h` で切替） |
| `--show-dirs` | 起動時にディレクトリを表示（`d` で切替） |
| `--open-cmd <cmd>` | Enter で起動するコマンド（`{}` / `{1}`.. プレースホルダ可）。終了までブロック → 再スキャン（レビューループ用） |
| `--filter <text>` | 初期フィルタ（`/` と同じマッチング。例: `--filter .md`） |
| `--preview <on\|off\|auto>` | プレビューペイン。`auto`（既定）は 80 列未満で非表示 |
| `--theme <name>` | syntect テーマ（two-face 埋め込み名 or `.tmTheme` パス。既定 `Catppuccin Mocha`、light 検出時 `Solarized (light)`） |
| `--light` / `--dark` | light/dark の明示指定（既定: OSC 11 で自動検出） |
| `--files` | TUI なしで時間順パス一覧を出力（fzf 等へのデータソース） |
| `--format <path\|tsv>` | `--files` の出力形式（tsv = `日時<TAB>名前<TAB>パス`） |
| `--since <today\|yesterday\|Nd\|Nw>` | 時間カットオフ（`--files` の絞り込み + TUI の初期表示） |
| `--output` | 既定動作（stdout 出力）の明示指定。`--open-cmd` と排他 |

`.gitignore` を尊重し（git リポジトリ外でも有効）、常時除外リスト（`.git/`、`node_modules/`、`target/`、`__pycache__/`、`.DS_Store`）を併用します。`.claude/` などのエージェント関連ディレクトリは**除外しません** — エージェントが生成したファイルこそレビュー対象だからです。

## キー

| キー | 動作 |
|---|---|
| `j` / `k`, `g` / `G`, `PgUp` / `PgDn`, `Ctrl+u` / `Ctrl+d` | 移動 |
| `Space` | ファイルを選択/解除 |
| `Enter` | 選択パスを stdout に出力して終了 — または `--open-cmd` を起動・ブロック・再スキャン |
| `y` | フルパスをクリップボードへコピー |
| `/` | インクリメンタルフィルタ（相対パスへの substring 一致） |
| `\` | フィルタのテキストを保持したまま ON/OFF トグル（`:nohlsearch` モデル） |
| `t` | ソート巡回 |
| `Ctrl+h` | ドットディレクトリの表示切替 |
| `d` | ディレクトリの表示切替 |
| `q` | 終了 |

一覧は 2 秒ごとに再スキャンされ、エージェントが別ペインで編集したファイルがライブで浮かび上がります。カーソル・選択はパスで追従します。

## レビューループ

`--open-cmd` を指定すると、`Enter` が選択ファイルでレビュアーを起動し、終了までブロックして再スキャンします。編集されたばかりのファイルが再び上に来て、次のラウンドへ:

```sh
ashiato . --open-cmd "akapen {} --send-agent"
```

## fzf 相互運用（`--files`）

ashiato の収集ロジック（ディレクトリ解決・無視リスト・ソート・時間カットオフ）はそのままデータソースになります:

```sh
ashiato --files | fzf                                   # 時間順 × ファジー検索
ashiato --files --format tsv | fzf --delimiter $'\t' \
  --with-nth 1..2 --preview 'bat --color=always {3}'    # 時間+名前表示、フルパスでプレビュー
ashiato --files --since today | fzf                     # 今日触ったファイルだけ
ashiato --files --since 1w --filter .md | xargs akapen  # 今週の md をまとめてレビュー
```

## herdr 連携

ディレクトリ解決の優先順位: 明示引数 → herdr（`HERDR_ENV=1` のとき worktree ルート、なければ同ワークスペースのエージェントの cwd）→ カレントディレクトリ。ashiato 自身は herdr に依存せず、エージェントとの接続はラッパー側の責務です。

## 設計メモ

- Rust + [ratatui](https://ratatui.rs) 0.30。ファイル収集は `ignore` クレート。
- プレビューは先頭 256KB まで。バイナリは file(1) 的な情報表示でクラッシュしません。
- light/dark は OSC 11 でターミナル背景色を問い合わせて自動判定（応答なしは dark）。
- あえてやらないこと: ツリー表示・プレビュー内スクロール・ファジーマッチ・ABC ソート・アイコンフォント。詳細は [docs/spec.md](docs/spec.md)。

## ライセンス / クレジット

MIT License。

`src/highlight.rs` は [akapen](https://github.com/worldnine/akapen) のハイライタのコピーで、akapen は [herdr-reviewr](https://github.com/persiyanov/herdr-reviewr)（MIT, Dmitry Persiyanov）を翻案しています。
