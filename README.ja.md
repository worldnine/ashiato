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
| `--alt-open-cmd <cmd>` | `o` キーで起動するコマンド（`--open-cmd` と同じ `{}` / `{1}`.. プレースホルダ・ブロック → 再スキャン。未指定時は `o` は無効） |
| `--filter <text>` | 初期フィルタ（`/` と同じマッチング。例: `--filter .md`） |
| `--preview <on\|off\|auto>` | プレビューペイン。`auto`（既定）は 80 列未満で非表示 |
| `--theme <name>` | syntect テーマ（two-face 埋め込み名 or `.tmTheme` パス。既定 `Catppuccin Mocha`、light 検出時 `Solarized (light)`） |
| `--light` / `--dark` | light/dark の明示指定（既定: OSC 11 で自動検出） |
| `--files` | TUI なしで時間順パス一覧を出力（fzf 等へのデータソース） |
| `--format <path\|tsv>` | `--files` の出力形式（tsv = `日時<TAB>名前<TAB>パス`） |
| `--since <today\|yesterday\|Nd\|Nw>` | 時間カットオフ（`--files` の絞り込み + TUI の初期表示） |
| `--view <mtime\|read>` | 起動時ビュー（`read` = エージェントが Read したファイルをライブ表示。`r` キーで切替） |
| `--output` | 既定動作（stdout 出力）の明示指定。`--open-cmd` と排他 |

`.gitignore` を尊重し（git リポジトリ外でも有効）、常時除外リスト（`.git/`、`node_modules/`、`target/`、`__pycache__/`、`.DS_Store`）を併用します。`.claude/` などのエージェント関連ディレクトリは**除外しません** — エージェントが生成したファイルこそレビュー対象だからです。

## キー

| キー | 動作 |
|---|---|
| `j` / `k`, `g` / `G`, `PgUp` / `PgDn`, `Ctrl+u` / `Ctrl+d` | 移動 |
| `Space` | ファイルを選択/解除 |
| `Enter` | 選択パスを stdout に出力して終了 — または `--open-cmd` を起動・ブロック・再スキャン |
| `o` | `--alt-open-cmd` を起動・ブロック・再スキャン（`--alt-open-cmd` 未指定時は無効） |
| `y` | フルパスをクリップボードへコピー |
| `/` | インクリメンタルフィルタ（相対パスへの substring 一致） |
| `\` | フィルタのテキストを保持したまま ON/OFF トグル（`:nohlsearch` モデル） |
| `t` | ソート巡回 |
| `r` | ビュー切替: mtime ビュー ↔ read ビュー（フッタに `[read]` バッジ） |
| `Ctrl+h` | ドットディレクトリの表示切替 |
| `d` | ディレクトリの表示切替 |
| `u` | 「未コミット変更のあるファイルのみ」表示のトグル（git リポジトリ内のみ。未コミット行は右側に `+N -M`） |
| `q` | 終了 |

一覧は 2 秒ごとに再スキャンされ、エージェントが別ペインで編集したファイルがライブで浮かび上がります。カーソル・選択はパスで追従します。

### read ビュー（エージェントが読んだファイル）

`r` キーでもう一つのビューに切り替わります: **あなたのエージェントが Read ツールで読んだファイル**。データ源はエージェント自身のセッションログ — Claude Code（`~/.claude/projects/<slug>/<session>.jsonl`）と pi（`~/.pi/agent/sessions/<slug>/<session>.jsonl`）。mtime ビューが「何を直したか」なら、read ビューは「どう頭に入れたか」— エージェントの探索経路です。

- 行の時刻は**最後に read された時刻**（相対 / HH:MM 表示）。`×N` バッジ（畳み込み後の read 回数）、`~N` バッジ（bash コマンドにパスが現れた回数 — Read されていないファイルもここで出現）、**read 後に編集されたファイル**の `●` マーカー付き — レビューで一番知りたい組み合わせです。
- **ライブ更新**: 表示中はセッションログを 500ms 周期で監視（git 連携と同じ (mtime, size) シグネチャゲート）。新規ファイルが fresh アクセントで上に浮き、再 read された既知ファイルも浮き上がります。
- read された後に消えたファイルは表示されません。`/` フィルタと `u`（未コミットのみ）もそのまま効きます。
- `--files --view read` で read 順の一覧を fzf に渡せます（`--format tsv` の時刻は read 時刻）。

```sh
ashiato --view read .            # エージェントが今まさに読んでいるもののライブビュー
ashiato . --view read --open-cmd "akapen {} --send-agent"  # 読んだものをレビュー
```

### アウェイ差分（ターミナルフォーカス）

ashiato は xterm フォーカスレポート（DECSET 1004）でターミナルのフォーカスを追跡します（kitty / WezTerm / Ghostty / alacritty / xterm / Terminal.app / iTerm2 / Windows Terminal に対応。tmux は `focus-events on` が必要）。フォーカスイベントを送らないターミナルではこの機能は休止します。

- **フォーカスが外れている間**にファイルが変更されると、行の色はそのままに、**触られていないファイルの時間だけが一段沈み**（DarkGray + DIM）、触られたファイルの時間が対比で浮かび上がります。フッターには `[away: N changed]`。変更は戻ってくるまでスタックされていきます（初見順・重複なし）。
- **フォーカスが戻ると**、表示は即座に通常の一覧へ戻ります（点滅なし — mtime ソートで変更ファイルは既に上に浮いています）。

変更の検出は `(mtime, size, is_dir)` シグネチャで行われます — 3 つとも不変の編集（例: `touch -r` で mtime を戻しつつ同サイズで上書き）は変更として見えません。プレビューキャッシュも同じシグネチャのため、そのような編集は次の本当の変更まで古いプレビューが残ることがあります。

### git 連携

スキャンルートが git ワークツリー内にある場合、未コミット変更のあるファイルにマークが付きます（仕様: [git-integration-spec.md](https://github.com/worldnine/akapen/blob/main/docs/git-integration-spec.md) §4）:

- 未コミットの行は右側に `+N -M` と相対時間を併記（`+7 now` のように）— diff の行規模と「未コミット」の印を 1 要素に融合しつつ、時間ブラウザとしての時間表示は潰しません（どちらも DIM 段）。幅が狭いときは時間のほうを先に落とします（新しさは mtime ソートとクラスタ見出しが担う）。コミット済みの行は従来どおり時間表示のみ。
- untracked（`??`）は未コミット扱い。その `+N` はファイル自身の行数です。バイナリは `-` だけを表示。
- `u` キーで「未コミット変更のあるファイルのみ」表示にトグル（フッターの `[uncommitted]` が状態を示します）。テキストフィルタ `/`・`--filter` は変更なし。
- リポジトリ外（または git が使えない環境）ではすべて無効 — 行表示もキーも従来どおりです。

`git status` / `git diff --numstat` はファイルシステムの一覧が実際に変わったときだけ実行（シグネチャゲート）されるため、2 秒ごとの再スキャンでツリーが安定している間は git を叩きません。スナップショットは次の実変更まで古いままになることがあります（仕様 P4 のスナップショット方針）。

## レビューループ

`--open-cmd` を指定すると、`Enter` が選択ファイルでレビュアーを起動し、終了までブロックして再スキャンします。編集されたばかりのファイルが再び上に来て、次のラウンドへ:

```sh
ashiato . --open-cmd "akapen {} --send-agent"
```

`--alt-open-cmd` を併用すると `o` キーで別のコマンドも同じフローで起動できます — 直行のレビューループ（Enter = akapen）を保ちつつ、ファイラへも飛べます:

```sh
ashiato . --open-cmd "akapen {} --send-agent" --alt-open-cmd "yazi {}"
# Enter = akapen でレビュー → 再スキャン、o = yazi で開く（q で抜けると再スキャン）
```

## fzf 相互運用（`--files`）

ashiato の収集ロジック（ディレクトリ解決・無視リスト・ソート・時間カットオフ）はそのままデータソースになります:

```sh
ashiato --files | fzf                                   # 時間順 × ファジー検索
ashiato --files --format tsv | fzf --delimiter $'\t' \
  --with-nth 1..2 --preview 'bat --color=always {3}'    # 時間+名前表示、フルパスでプレビュー
ashiato --files --since today | fzf                     # 今日触ったファイルだけ
ashiato --files --view read | fzf                       # エージェントが read したファイル（read 時刻順）
ashiato --files --since 1w --filter .md | xargs akapen  # 今週の md をまとめてレビュー
```

## herdr 連携

ディレクトリ解決の優先順位: 明示引数 → herdr（`HERDR_ENV=1` のとき worktree ルート、なければ同ワークスペースのエージェントの cwd）→ カレントディレクトリ。ashiato 自身は herdr に依存せず、エージェントとの接続はラッパー側の責務です。

このリポジトリはそのまま [herdr プラグイン](herdr-plugin.toml) です: `herdr plugin link <このリポジトリ>` で `ashiato.open` アクション（横分割ピッカー）が登録され、`[[keys.command]] type = "plugin_action"` でキーに割り当てられます。ピッカー内では Enter で akapen、`o` で yazi が開きます（`scripts/ashiato-pane.sh` の `ASHIATO_OPEN_CMD` / `ASHIATO_ALT_OPEN_CMD` で差し替え可）。**流儀**: herdr 連携コードはツールのリポジトリに plugin として同居させる（akapen の `akp` プラグインも同じ）。

## 設計メモ

- Rust + [ratatui](https://ratatui.rs) 0.30。ファイル収集は `ignore` クレート。
- プレビューは先頭 256KB まで。バイナリは file(1) 的な情報表示でクラッシュしません。
- 速度優先プレビュー: j/k 連打（キーリピート）中はプレースホルダ表示でカーソル
  移動は一覧描画のみのコストに。キーを離すと ~40ms でプレビューが追いつきます。
- light/dark は OSC 11 でターミナル背景色を問い合わせて自動判定（応答なしは dark）。
- あえてやらないこと: ツリー表示・プレビュー内スクロール・ファジーマッチ・ABC ソート・アイコンフォント。詳細は [docs/spec.md](docs/spec.md)。

## ライセンス / クレジット

MIT License。

`src/highlight.rs` は [akapen](https://github.com/worldnine/akapen) のハイライタのコピーで、akapen は [herdr-reviewr](https://github.com/persiyanov/herdr-reviewr)（MIT, Dmitry Persiyanov）を翻案しています。
