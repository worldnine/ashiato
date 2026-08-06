# revpick 仕様書

**状態:** 設計確定・未実装

---

## 目的

プロジェクト全体のファイルを更新順（mtime 降順）でフラット表示する TUI ピッカー。
ファイルを選んでパスを出力する（既定）。`--open-cmd` 指定時は選択ファイルを
そのコマンドに渡す（mdcomment のレビューループ等）。
herdr 上でも単体でも使える。

---

## 起動

```
revpick [directory] [flags]
```

### フラグ

| フラグ | デフォルト | 説明 |
|---|---|---|
| `--sort mtime\|ctime` | `mtime` | ソート基準 |
| `--show-hidden` | false | 隠し**ディレクトリ**（`.claude/` 等 `.` 始まりのディレクトリ）配下を表示。ドット**ファイル**（`.gitignore` 等）は常に表示（例レイアウト参照。`.DS_Store` のみ常時除外） |
| `--show-dirs` | false | ディレクトリを表示 |
| `--open-cmd <command>` | —（なし） | Enter で起動するコマンド。選択ファイルが引数に渡される。**未指定時は Enter が選択パスを stdout に出力して即終了**（既定動作 = 出力モード） |
| `--filter <text>` | —（空） | 初期フィルタ（`/` と同じマッチング）。例: `--filter .md` で md ファイルのみ表示。起動後は `\` で一時解除・復帰、`/` で編集・解除可 |
| `--preview <on\|off\|auto>` | `auto` | プレビューペインの表示。`auto` はターミナル幅が 80 列未満だと非表示（リストが全幅に） |
| `--files` | — | TUI を開かず、収集・ソート済みのパス一覧を stdout に出力して終了（パイプ / fzf 用のデータソース） |
| `--format <path\|tsv>` | `path` | `--files` の出力形式。`tsv` は `YYYY-MM-DD HH:MM:SS<TAB>basename<TAB>path` の3フィールド（fzf の `--with-nth` / `--nth` / `{1}` `{2}` `{3}` で表示・検索・プレビューを自由に組み合わせられる） |
| `--since <today\|yesterday\|Nd\|Nw>` | — | 時間カットオフ。`--files` では出力を絞り、**TUI では起動時から指定より新しいファイルのみ表示**（プレビュー・クラスタ・`\` トグル等がそのまま効く） |
| `--output` | — | 出力モードの明示指定（既定動作と同じ）。`--open-cmd` と排他 |
| `--theme <name>` | —（`Catppuccin Mocha`） | プレビューの syntect テーマ（two-face 埋め込み名 or `.tmTheme` パス。light 自動検出時は `Solarized (light)`） |
| `--light` / `--dark` | 自動検出 | light/dark の明示指定。OSC 11 でターミナル背景色を問い合わせ、輝度 128 超で light。応答なしは dark |

> **2026-08-06 改訂**: 未知のフラグ・値のないフラグ・不正な値（`--sort bogus`、
> `--since bogus` 等）は**エラーで即終了**する。黙って無視すると typo が
> 「効いたように見える」ため（例: `--outpu` で TUI が開いてしまう）。
>
> **2026-08-05 改訂**: 旧仕様の既定値は `mdcomment` だったが、
> 「revpick は汎用ピッカーであり、開くコマンドは外側（ラッパー）の責務」という
> 疎結合の前提に合わせ、既定コマンドを廃止した。`--open-cmd mdcomment` を
> 明示すれば旧仕様と同じ動作（起動 → ブロック → 再スキャン）になる。

### ディレクトリ解決の優先順位

1. 引数で明示指定: `revpick /path/to/project`
2. herdr 環境変数から自動検出（`HERDR_ENV=1` の場合）:
   - `herdr worktree list` → 現在のワークスペースの worktree パスをルートに
   - なければ `herdr agent list` → 同じワークスペースのエージェントの `cwd` をルートに
3. カレントディレクトリ

### デフォルト無視リスト

`.gitignore` に加えて、以下のパターンを常に除外:

```
.git/
node_modules/
target/
__pycache__/
.DS_Store
```

`.claude/`、`.codex/`、`.pi/` などエージェント関連ディレクトリは**除外しない**。
エージェントが生成したファイルもレビュー対象のため。

---

## UI レイアウト

```
┌── revpick · /path/to/project · 23 files ──────────────────────────┐
│                                                                     │
│  ── Today ─────────────────────────────────────────────────────── │
│  > design.md                      14:23 │  1 // design.md          │
│    src/main.rs                    14:12 │  2                       │
│                                          │  3 ## 設計ドキュメント    │
│  ── Yesterday ─────────────────────────────────────────────────── │
│    README.md                     Aug 3  │  4                       │
│    Cargo.toml                    Aug 3  │  5 これは...            │
│                                          │                         │
│  ── This week ─────────────────────────────────────────────────── │
│    testdata/full.md              Jul 31 │                         │
│                                          │  (プレビュー領域)        │
│  ── Older ─────────────────────────────────────────────────────── │
│    LICENSE                       Jul 15 │                         │
│    .gitignore                    Jun 20 │                         │
│                                                                     │
│  [mtime↓] [hidden] [dirs]             │                            │
│  Space:select  Enter:open  y:copy      │                            │
│  /:filter  \:toggle  t:sort  q:quit    │                            │
└─────────────────────────────────────────────────────────────────────┘
```

- 左ペイン: ファイル一覧（フラットリスト）
- 右ペイン: プレビュー

---

## ファイル一覧（左ペイン）

### クラスタリング

mtime を基準に時間帯でグループ化。空のクラスタは表示しない。

| ラベル | 条件 |
|---|---|
| Today | 今日 00:00 以降 |
| Yesterday | 昨日 00:00 〜 今日 00:00 |
| This week | 今週月曜 00:00 〜 昨日 00:00 |
| Last week | 先週月曜 00:00 〜 今週月曜 00:00 |
| This month | 今月1日 00:00 〜 先週月曜 00:00 |
| Older | それ以前 |

- 区切り線は ANSI DarkGray、セパレータ行はカーソル対象外
- ソートを ctime に切り替えてもクラスタリング基準は常に mtime（ctime クラスタリングはしない）

### ソート

| フラグ / キー | ソート | 巡回順 |
|---|---|---|
| `t` | mtime ↓（デフォルト） | mtime↓ → mtime↑ → ctime↓ → ctime↑ |
| | mtime ↑ | |
| | ctime ↓ | |
| | ctime ↑ | |

### 日時表示

| 経過 | 表示形式 | 例 |
|---|---|---|
| 今日 | `HH:MM` | `14:23` |
| 今年 | `Mon D` | `Aug 3` |
| 去年以前 | `YYYY-MM-DD` | `2025-12-03` |

### ファイルリスト表示

```
<marker> <filename>     <datetime>
```

- `>` がカーソル行、`*` が Space 選択行（2026-08-05: 📄/📁 の絵文字は廃止。
  拡張子でファイル種別は十分判別可能なため、アイコン類は使わない）
- カーソル行は **Cyan fg + グレー背景**（mdcomment の view モードと同一モデル）、
  選択行はグレー背景のみ。グレー背景の色は light/dark で swap（`--light` 参照）
- ファイル名の色はテーマのデフォルト前景色に追従（カーソル行は Cyan）
- 行レイアウト: `<marker> <ディレクトリ部分><ベース名>  <日時>`
  - ディレクトリ部分はグレー（DarkGray）、ベース名はテーマのデフォルト前景色
    （カーソル行は Cyan）。日時は常にグレー
  - **表示優先度: ベース名 > ディレクトリ部分 > 日時**。日時は余白があるときだけ
    右寄せ表示（クラスタ区切りが時間の文脈を担うため、欠けてもよい）
  - パスが長いとき: ディレクトリ部分を末尾側コンポーネントから保持したまま
    `…/` で短縮 → それでも収まらなければベース名の先頭側を `…` で短縮

---

## プレビュー（右ペイン）

### 表示内容

- 選択ファイルの先頭から、プレビュー領域に収まる行数だけ表示
- **スクロール不可**（プレビュー内での j/k 移動はなし）
- syntect によるシンタックスハイライト（mdcomment のコードを共用）
- 画像・バイナリファイルは file(1) 的な情報を表示（例: `PNG image data, 640x480, 24KB`）

### テーマ

- `--theme <name>`: two-face の埋め込みテーマ名（例: `Catppuccin Mocha`）または
  `.tmTheme` ファイルパス（mdcomment と同じ引数）
- light/dark は**自動判定**: 起動時 OSC 11（`ESC ] 11 ; ?`）でターミナル背景色を
  問い合わせ、輝度が 128 を超えれば light。応答しないターミナル（Terminal.app 等）は
  dark。`--light` / `--dark` で明示指定が最優先
- `--theme` 未指定時: light なら `Solarized (light)`、dark なら `Catppuccin Mocha`
- TUI 色（選択行背景・外枠ボーダー）は mdcomment の `--light` と同一の定数で swap。
  ファイル名はテーマのデフォルト前景色で描画（light テーマでは暗色になる）。
  カーソル行のみ Cyan アクセント（mdcomment の view モードと同じモデル）
- 将来的に設定ファイルが入ればそこで指定

### 表示制御（`--preview <on|off|auto>`）

- `on`: 常に表示（既定のレイアウト）
- `off`: 非表示。リストが全幅を使う
- `auto`: ターミナル幅が 80 列未満のときだけ非表示（狭いペインでは
  プレビューより一覧の情報量を優先）

### ライブ更新

- 2 秒ごとに再スキャンし、**一覧に変化があれば自動で表示を置き換える**
  （エージェントが別ペインでファイルを編集している場合、触ったファイルが
  上に浮いてくる）。カーソル・選択はパスで追従。変化がなければ何もしない
- 変化がない間はプレビューキャッシュも保持される

---

## キーバインド

### ナビゲーション

| キー | 動作 |
|---|---|
| `j` / `k` | カーソル上下移動 |
| `g` / `G` | 先頭 / 末尾へ |
| `PgUp` / `PgDn` | 半ページ移動 |
| `Ctrl+u` / `Ctrl+d` | 半ページ移動（同上） |

### 選択・起動

| キー | 動作 |
|---|---|
| `Space` | ファイルを選択/解除（トグル） |
| `Enter` | `--open-cmd` 指定時: 選択ファイルをそのコマンドで起動し、終了までブロック → 一覧を再スキャン。未指定時: 選択パスを stdout に出力して即終了 |
| `y` | 選択ファイル（またはカーソル位置のファイル）のフルパスをクリップボードにコピー |

### フィルタ・ソート

| キー | 動作 |
|---|---|
| `/` | インクリメンタルフィルタ。ファイル名（パス含む）に対してマッチ。空文字で解除 |
| `t` | ソート巡回: mtime↓ → mtime↑ → ctime↓ → ctime↑ |
| `Ctrl+h` / `Backspace` | 隠しディレクトリ表示/非表示トグル（レガシー端末は Ctrl+h を BS 0x08 で送るため、Backspace も同一トグル） |
| `d` | ディレクトリ表示/非表示トグル |
| `\` | フィルタ適用の ON/OFF トグル（vim の `:nohlsearch` モデル。**テキストは保持**され、再トグルで即復帰。`/` で入力を始めると自動で ON に戻る） |

### 終了

| キー | 動作 |
|---|---|
| `q` | 終了 |
| `Esc` | フィルタ入力中: フィルタ解除 / 通常時: 選択解除 |

---

## フィルタの仕様

- `/` でフィルタ入力モードへ
- 入力文字列を含むファイルのみ表示（パスのどの位置でもマッチ）
- クラスタ区切りはフィルタ結果に応じて動的に再生成
- 空文字でフィルタ解除 → 全ファイル表示
- 例: `/main.rs` → `src/main.rs` が表示される
- 例: `/src/` → `src/` 配下のファイルのみ表示
- 例: `/.md` → `.md` 拡張子のファイルのみ表示
- **`\` で適用 ON/OFF をトグル**（`--filter` はそのコンテキストのデフォルトビュー。
  一時的に外して全体を見て、もう一度 `\` で元のビューに戻る。テキストはトグル中も
  保持される。OFF 中はフッタに `[filter:… off]` と表示）。テキストの消去は
  `/` 入力中の Esc だけ（明示的な解除）

---

## 複数選択 → 子プロセス起動のフロー（`--open-cmd` 指定時のみ）

```
1. Space でファイルを選択（複数可）
2. Enter → revpick は子プロセスとして --open-cmd を起動
   例: mdcomment /path/a.md /path/b.rs /path/c.toml
3. revpick は子プロセスの終了を待つ（ブロック）
4. 子プロセス終了 → ファイル一覧を再スキャン
   → エージェントが編集したファイルの mtime が更新され上に来る
5. revpick の TUI に復帰
```

`--open-cmd` 未指定時はこのフロー自体がなく、Enter は出力モード（下記）。

---

## `--files` モード（時間ブラウザのデータソース）

TUI を開かずに「時間順に並んだファイル一覧」を出力する。revpick の
収集ロジック（herdr 解決・gitignore・常時除外・隠し/ディレクトリ切替・
ソート・フィルタ）がそのままデータソースになる。

```bash
revpick --files | fzf                                # 時間順 × ファジー検索
revpick --files --format tsv | fzf --delimiter $'\t' --with-nth 1..2 \
  --preview 'bat --color=always {3}'                 # 時間列 + 名前表示、プレビューはフルパス
revpick --files --format tsv | fzf --delimiter $'\t' --with-nth 2 --nth 2.. \
  --preview 'bat --color=always {3}'                 # ファイル名だけ表示（検索は名前+パス）
revpick --since today --filter .md                   # TUI: 今日の md をプレビュー付きでブラウズ
revpick --files --since today | fzf                  # 今日触ったファイルだけ
revpick --files --since 1w --filter .md | xargs mdcomment  # 今週の md をまとめてレビュー
revpick --files | fzf --bind 'ctrl-r:reload(revpick --files)'  # ライブ更新（手動）
```

位置づけ: revpick は「**時間ブラウザ**」（直近の変更がデフォルトビュー）であり、
検索ツールではない。ファジー検索・高度なプレビューが必要な場合は
`--files` で fzf に委ねる（相互運用）。mdcomment レビューループ
（ブロック → 再スキャン → 状態維持）は revpick 単体の領分。

## 既定動作（出力モード・UNIX フィルタ）

`--open-cmd` 未指定（= 既定）のとき、Enter は選択パスを stdout に出力して終了する。
`--output` はこの既定動作の明示指定（`--open-cmd` と併用不可）。

```
$ revpick .
# TUI が開き、Space で選択 → Enter で選択ファイルパスが stdout に出力されて終了
/path/to/design.md
/path/to/src/main.rs

$ revpick . --show-hidden
# 隠しファイルも表示
```

パイプでの利用:

```bash
revpick . | xargs mdcomment
revpick . | xargs vim -p
revpick --sort ctime | while read f; do echo "==> $f"; cat "$f"; done
```

stdout がパイプの場合、TUI は `/dev/tty` に描画されるため
パイプにはエスケープ列が混入しない。

---

## herdr 連携

### herdr 上の自動ディレクトリ解決

revpick は `HERDR_ENV=1` の場合、起動時に以下を試行:

```bash
# 1. worktree のパスを取得
herdr worktree list 2>/dev/null
# → source.repo_root をルートディレクトリに

# 2. なければエージェントの cwd を取得
herdr agent list 2>/dev/null
# → 同じ HERDR_WORKSPACE_ID のエージェントの cwd をルートに
```

### yazi からの呼び出し

```toml
# ~/.config/yazi/keymap.toml
[[manager.prepend_keymap]]
on   = [ "g", "f" ]
run  = "shell 'revpick . --open-cmd mdcomment' --block"
desc = "Flat mtime picker → mdcomment"
```

### herdr ラッパーでの mdcomment 連携

```bash
#!/bin/bash
# herdr ラッパーが revpick + mdcomment を繋ぐ例。
# mdcomment の `--send-agent` が「現在タブの唯一のエージェント」を自動解決して
# `herdr agent prompt <pane> <text>` で送信する（argv 直接渡し、シェル非経由）。
# 0件・複数で曖昧なら赤トーストで拒否し、コメントは保持される。

revpick . --open-cmd "mdcomment {} --send-agent"
```

> **2026-08-05 決定**: 当初案の `--send-cmd 'herdr pane run ...'` 方式は、
> `herdr pane run` がコマンド引数必須で stdin を読まない（usage エラーになる）ため
> 不採用。mdcomment 側に `--send-agent`（タブ内唯一エージェントへ自動送信、
> herdr agent prompt 経由）を実装し、ラッパーは引数を渡すだけで済むようにした。
> 実機確認済み（2026-08-05: 使い捨てタブのテスト agent に reviewr 形式ブロックが
> 届き ack 応答）。

revpick 自身は herdr 非依存。連携は外側のラッパー（または yazi の opener 設定）の責務。

---

## `--open-cmd` のプレースホルダ

| プレースホルダ | 展開結果 |
|---|---|
| `{}` | 選択ファイルのパスをスペース区切りで全件 |
| `{1}` `{2}` ... | 1番目、2番目...のファイルパス |

```bash
revpick . --open-cmd "mdcomment --theme Nord {}"
revpick . --open-cmd "vim -p {}"
```

---

## 実装

### 技術スタック

- Rust + ratatui 0.30（mdcomment と共通）
- crates: `anyhow`, `ratatui`, `syntect`, `ignore`, `unicode-width`, `chrono`
- mdcomment と同じリポジトリ内、別バイナリ（`src/bin/revpick.rs` またはワークスペースメンバー）

### ファイル構成

```
Cargo.toml           → workspace 化（mdcomment + revpick をメンバーに）
src/
  main.rs            → mdcomment エントリ
  ...
src/bin/
  revpick.rs         → revpick エントリ（または revpick/ ディレクトリ）
```

（standalone の revpick では `src/main.rs` + `src/theme.rs` 構成。`theme.rs` は
OSC 11 背景色検出と light/dark UI 色の解決で、mdcomment `view.rs` の定数をコピー）

### プレビューの syntect 共用

mdcomment の `src/highlight.rs` を共通クレートとして切り出し、両バイナリから利用する。

### ファイル収集

`ignore` クレートの `WalkBuilder` を使用:

```rust
let mut builder = WalkBuilder::new(root);
builder.standard_filters(true);          // .gitignore 対応
builder.filter_entry(|e| {
    !DEFAULT_IGNORES.iter().any(|p| e.path().to_string_lossy().contains(p))
});
```

### mtime 取得とソート

`std::fs::metadata` で mtime/ctime を取得。`chrono` でローカルタイムに変換しクラスタリング。

---

## あえてやらないこと（YAGNI）

| 項目 | 理由 |
|---|---|
| ツリー表示 | yazi / broot の領分。revpick はフラット mtime 専用 |
| プレビュー内スクロール | revpick はピッカー。深いプレビューは mdcomment でやる |
| `.md` のレンダリングプレビュー | syntect ハイライトで十分。レンダリングは mdcomment の view モードで |
| git ステータス表示（`M`/`A`/`?`） | 後回し。mdcomment の git 連携と同時期に検討 |
| `d` で `git diff` 表示 | 同上 |
| 設定ファイル | 設定項目が少ない。使ってから必要になったら追加 |
| Nerd Font アイコン・絵文字 | フォント依存・見た目。拡張子で十分（📄 等の絵文字も 2026-08-05 に廃止） |
| ABC ソート | 普通のファイラでやればいい。revpick は時系列専用 |
| キーバインドのカスタマイズ | キーが少なく vim 準拠。カスタマイズ需要は低い |

---

## テスト項目

1. ディレクトリ指定なし → カレントディレクトリ（または herdr cwd）を再帰的に収集
2. mtime 降順で表示 → 最近のファイルが上に来る
3. クラスタリング → Today / Yesterday / ... が正しく区切られる
4. `t` → ソート巡回が正しく動作
5. `/` → フィルタがインクリメンタルに動作
6. `Space` → 複数選択 → Enter → mdcomment にファイルが渡される
7. mdcomment 終了後 → ファイル一覧が再スキャンされ最新状態に
8. `y` → クリップボードにフルパスが入る
9. `Ctrl+h` → 隠しファイル表示/非表示
10. `d` → ディレクトリ表示/非表示
11. `--open-cmd` 未指定（または `--output`）→ 選択ファイルが stdout に出力され即終了
12. `.gitignore` が効いている（`target/`、`node_modules/` が除外される）
13. `.claude/`、`.codex/` が除外されていない
14. 画像/バイナリファイルでプレビューがクラッシュしない
15. herdr 外でも `herdr` コマンド不在でクラッシュせず起動する
16. OSC 11 応答のパース（`rgb:rrrr/gggg/bbbb` / 短形式 / `rgba:` / `#rrggbb`）が正しい
17. `--light` / `--dark` がパースされ、既定が自動検出（None）になる
18. light/dark の UI 色が mdcomment と同一の定数で解決される
19. 行レイアウトの短縮: ベース名が最優先（ディレクトリ → `…/` 短縮 → ベース名 `…` 短縮）、文字境界安全
