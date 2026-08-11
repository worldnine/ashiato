# ashiato 仕様書

**状態:** 実装済み（実装と同期して更新）

---

## 目的

プロジェクト全体のファイルを更新順（mtime 降順）でフラット表示する TUI ピッカー。
ファイルを選んでパスを出力する（既定）。`--open-cmd` 指定時は選択ファイルを
そのコマンドに渡す（akapen のレビューループ等）。
herdr 上でも単体でも使える。

---

## 起動

```
ashiato [directory] [flags]
```

### フラグ

| フラグ | デフォルト | 説明 |
|---|---|---|
| `--sort mtime\|ctime` | `mtime` | ソート基準 |
| `--show-hidden` | false | 隠し**ディレクトリ**（`.claude/` 等 `.` 始まりのディレクトリ）配下を表示。ドット**ファイル**（`.gitignore` 等）は常に表示（例レイアウト参照。`.DS_Store` のみ常時除外） |
| `--show-dirs` | false | ディレクトリを表示 |
| `--open-cmd <command>` | —（なし） | Enter で起動するコマンド。選択ファイルが引数に渡される。**未指定時は Enter が選択パスを stdout に出力して即終了**（既定動作 = 出力モード） |
| `--alt-open-cmd <command>` | —（なし） | `o` キーで起動するコマンド。`--open-cmd` と同じプレースホルダ・ブロック → 再スキャン。未指定時は `o` は無効キー（`no --alt-open-cmd` をフラッシュ） |
| `--filter <text>` | —（空） | 初期フィルタ（`/` と同じマッチング）。例: `--filter .md` で md ファイルのみ表示。起動後は `\` で一時解除・復帰、`/` で編集・解除可 |
| `--preview <on\|off\|auto>` | `auto` | プレビューペインの表示。`auto` はターミナル幅が 80 列未満だと非表示（リストが全幅に） |
| `--files` | — | TUI を開かず、収集・ソート済みのパス一覧を stdout に出力して終了（パイプ / fzf 用のデータソース） |
| `--format <path\|tsv>` | `path` | `--files` の出力形式。`tsv` は `YYYY-MM-DD HH:MM:SS<TAB>basename<TAB>path` の3フィールド（fzf の `--with-nth` / `--nth` / `{1}` `{2}` `{3}` で表示・検索・プレビューを自由に組み合わせられる） |
| `--since <today\|yesterday\|Nd\|Nw>` | — | 時間カットオフ。`--files` では出力を絞り、**TUI では起動時から指定より新しいファイルのみ表示**（プレビュー・クラスタ・`\` トグル等がそのまま効く） |
| `--output` | — | 出力モードの明示指定（既定動作と同じ）。`--open-cmd` と排他 |
| `--theme <name>` | —（`Catppuccin Mocha`） | プレビューの syntect テーマ（two-face 埋め込み名 or `.tmTheme` パス。light 自動検出時は `Solarized (light)`） |
| `--light` / `--dark` | 自動検出 | light/dark の明示指定。OSC 11 でターミナル背景色を問い合わせ、輝度 128 超で light。応答なしは dark |

> 未知のフラグ・値のないフラグ・不正な値（`--sort bogus`、`--since bogus` 等）は
> **エラーで即終了**する。黙って無視すると typo が「効いたように見える」ため
> （例: `--outpu` で TUI が開いてしまう）。

### ディレクトリ解決の優先順位

1. 引数で明示指定: `ashiato /path/to/project`
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

<!-- TODO: 実スクリーンショット画像に差し替える -->

```
┌── ashiato · /path/to/project · 23 files ──────────────────────────┐
│                                                                     │
│  ── Today ─────────────────────────────────────────────────────── │
│  > design.md                        now │  1 // design.md          │
│    src/main.rs                    12:12 │  2                       │
│                                          │  3 ## 設計ドキュメント    │
│  ── Yesterday ─────────────────────────────────────────────────── │
│    README.md                      13:04 │  4                       │
│    Cargo.toml                     12:58 │  5 これは...            │
│                                          │                         │
│  ── Mon, Aug 3 ────────────────────────────────────────────────── │
│    testdata/full.md                16:40 │                         │
│                                          │  (プレビュー領域)        │
│  ── Fri, Jul 31 ───────────────────────────────────────────────── │
│    LICENSE                        09:15 │                         │
│  ── Sat, Jun 20 ───────────────────────────────────────────────── │
│    .gitignore                     08:02 │                         │
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

mtime を基準に**日ごと**にグループ化。空のクラスタは表示しない。

| ラベル | 条件 |
|---|---|
| Today | 今日 00:00 以降 |
| Yesterday | 昨日 00:00 〜 今日 00:00 |
| `Wed, Aug 4` など | それより前は**日付をそのまま見出し**にする。例: `Mon, Aug 3`、`Fri, Jul 31`。年が違う日は末尾に年を付ける（`Wed, Dec 3, 2025`） |

「This week」「Older」のような期間のまとめは**しない**。日付が見出しになることで、
しばらく経ってから見返したときにも「どの日に触ったか」がそのまま残る。

- 区切り線はフレームと同じボーダー色（タイトル「ashiato」と同色）、セパレータ行はカーソル対象外
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
| < 60秒 | `now` | |
| 1–59分 | `Nm ago` | `5m ago` |
| ≥ 1時間 | `HH:MM` | `14:23` |

相対表記は1時間未満のみ。`ago` が付いていたら「1時間以内に触られた」という
強いルールになり、`2m ago` と `3h ago` の読み分け問題が起きない（2026-08-07:
`Nh ago` を廃止）。1時間以上は時刻のみ表示し、日付の文脈はクラスタ見出しが
担う（`── Wed, Aug 4 ──` の下はその日の時刻だけ表示）。

新しさは**色ではなくイタリック**で表す: 相対表記（`now` / `Nm ago`）だけ
斜体、`HH:MM` は立体。時刻はどちらも脇役スタイル（後述）のまま。
（2026-08-07: 黄色アクセント案・明るさ3段階のRGB計算案をいずれも試した
うえで不採用 — 配色はターミナルに任せる方針に統一）

### ファイルリスト表示

```
<marker> <filename>     <datetime>
```

**配色ポリシー（2026-08-07）**: 一覧の配色はターミナルのパレットに任せる。
明るさは2段構え + 構造色:

| 段 | スタイル | 使いどころ |
|---|---|---|
| 主役 | 色指定なし（端末デフォルト前景色） | ベース名、ONのトグル |
| 脇役 | デフォルト前景色 + DIM（SGR 2） | 日時、ショートカット、OFFのトグル、日付ラベル |
| 構造色 | ボーダー色（テーマ由来・無修飾） | 外枠・タイトル、クラスタ罫線、ディレクトリ部分 |

罫線とディレクトリ部分は外枠と同じボーダー色で「構造」として描き、日付
ラベルは日時と同じ脇役段に置く（2026-08-07: 当初は DarkGray+DIM の
「ほぼ消える」段 → 実機確認で暗すぎて読めないため default+DIM に1段上げ
→ 罫線とディレクトリ部分はさらにタイトルと同じボーダー色に分離。日付
ラベルを罫線と区別するためのイタリックも併せて廃止）。
アクセントは ANSI 色のみ（カーソル Cyan・注意 Yellow・エラー Red）。
RGB を使うのはプレビューのシンタックスハイライト（syntect テーマ由来で
不可避）と、選択背景・枠線（akapen 由来の定数）だけ。
DarkGray が残るのはアウェイ差分の「さらに沈む側」（後述）だけ。

- `>` がカーソル行、`*` が Space 選択行（2026-08-05: 📄/📁 の絵文字は廃止。
  拡張子でファイル種別は十分判別可能なため、アイコン類は使わない）
- カーソル行は **Cyan fg + グレー背景**（akapen の view モードと同一モデル）、
  選択行はグレー背景のみ。グレー背景の色は light/dark で swap（`--light` 参照）
- 行レイアウト: `<marker> <ディレクトリ部分><ベース名>  <日時>`
  - ディレクトリ部分はボーダー色（タイトルと同じ）、日時は DIM、
    ベース名は端末デフォルト前景色（カーソル行は Cyan）。
    1時間以内の日時はさらにイタリック
  - **表示優先度: ベース名 > ディレクトリ部分 > 日時**。日時は余白があるときだけ
    右寄せ表示（クラスタ見出しが日付の文脈を担うため、欠けてもよい）
  - パスが長いとき: ディレクトリ部分を末尾側コンポーネントから保持したまま
    `…/` で短縮 → それでも収まらなければベース名の先頭側を `…` で短縮

---

## プレビュー（右ペイン）

### 表示内容

- 選択ファイルの先頭から、プレビュー領域に収まる行数だけ表示
- **スクロール不可**（プレビュー内での j/k 移動はなし）
- syntect によるシンタックスハイライト（akapen のコードを共用）
- 画像・バイナリファイルは file(1) 的な情報を表示（例: `PNG image data, 640x480, 24KB`）

### テーマ

- `--theme <name>`: two-face の埋め込みテーマ名（例: `Catppuccin Mocha`）または
  `.tmTheme` ファイルパス（akapen と同じ引数）
- light/dark は**自動判定**: 起動時 OSC 11（`ESC ] 11 ; ?`）でターミナル背景色を
  問い合わせ、輝度が 128 を超えれば light。応答しないターミナル（Terminal.app 等）は
  dark。`--light` / `--dark` で明示指定が最優先
- `--theme` 未指定時: light なら `Solarized (light)`、dark なら `Catppuccin Mocha`
- TUI 色（選択行背景・外枠ボーダー）は akapen の `--light` と同一の定数で swap。
  ファイル名はテーマのデフォルト前景色で描画（light テーマでは暗色になる）。
  カーソル行のみ Cyan アクセント（akapen の view モードと同じモデル）
- 将来的に設定ファイルが入ればそこで指定

### 表示制御（`--preview <on|off|auto>`）

- `on`: 常に表示（既定のレイアウト）
- `off`: 非表示。リストが全幅を使う
- `auto`: ターミナル幅が 80 列未満のときだけ非表示（狭いペインでは
  プレビューより一覧の情報量を優先）

### 速度優先（jk 連続遷移）

- プレビューの再描画（ファイル読み込み + syntect）は 1 回あたり数 ms かかるため、
  **入力バースト中（前のイベントから 40ms 以内に次のイベント）は描画を遅延**する:
  プレビューはヘッダ + `…` プレースホルダのみ表示し、カーソル移動は一覧描画
  だけのコストになる（ホールド j/k のキーリピート中も引っかかりなし）
- 意図的な単発キー（直前の入力から 40ms 以上経過）は従来通り**即時描画**される
- キーリリースを検知すると ~40ms で実際のプレビューを描画（追いつく）
- キャッシュ済みファイルへの遷移（jk 往復など）はバースト中でもキャッシュを表示
- **描画コストの上限**: ハイライト対象は「ペインに表示できるセル数」
  （`幅×高さ`）・「`高さ` 行」・「1 行 `幅×8` 文字」のいずれかで切り詰める。
  単一行に収まる minified バンドル（256KB の 1 行など）は syntect の行内コストが
  超線形で、切り詰めなしでは 1 回のプレビュー描画が 1.5 秒かかり、
  ホールド j/k の冒頭キーがそのまま固まっていた（2026-08-07 修正）
- **遅い描画直後のキーもバースト扱い**: 描画が 40ms を超えると次のキーまでの
  間隔がバースト判定を外れ、ホールド全体が「1 キー = 1 全描画」の連鎖（カスケード）
  に陥る。`mark_input` は直前の描画終了から 40ms 以内のキーをバーストとみなす
  （描画中にキューされたキー = ホールド）
- 実装: イベントループのバースト検知（`mark_input` / `BURST_GAP`）と
  `App::preview_lines` の遅延ポリシー、`preview::head_lines_by_cells` の
  切り詰め。`--preview off` 時はそもそも描画しない

### ライブ更新

- 2 秒ごとに再スキャンし、**一覧に変化があれば自動で表示を置き換える**
  （エージェントが別ペインでファイルを編集している場合、触ったファイルが
  上に浮いてくる）。カーソル・選択はパスで追従。変化がなければ何もしない
- 変化がない間はプレビューキャッシュも保持される

### アウェイ差分（ターミナルフォーカス）

- 起動時にフォーカスイベントを有効化（DECSET 1004 / `EnableFocusChange`）。
  フォーカスイベントを送らないターミナルではこの機能は休止（`focused` は
  true のまま、通常表示）
- **フォーカス喪失**（`FocusLost`）: アウェイ差分スタックを空にして計測開始。
  以降の再スキャンで、**新規追加・変更されたファイルのパスがスタックに蓄積**
  （初見順・重複なし。`files::changed_paths` が mtime/size/is_dir で差分検出）
- **アウェイ表示**: 行の色はいつも通り。スタック非空の間、**変更されな
  かったファイルの時刻だけ DarkGray + DIM にさらに沈み**、変更された
  ファイルの時刻は通常の脇役スタイル（DIM）のまま — 時刻列のコントラスト
  だけで新しさが分かる。フッターに `[away: N changed]`。削除されたファイルは
  行が消えるだけ（追跡しない）。（2026-08-07: 全行減光 + `+` マーカー方式は
  目立ちすぎるため廃止）
- **フォーカス復帰**（`FocusGained`）: スタックを破棄して**即座に通常表示に
  戻る**。点滅などの演出はしない（mtime ソートにより変更ファイルは既に上に
  来ている）
- 子プロセス（`--open-cmd`）復帰時は `focused = true` に戻し、スタックを
  クリア（子がターミナルを占有している間にキューに残ったフォーカスイベントが
  誤ってアウェイ判定に繋がらないようにする）

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
| `o` | `--alt-open-cmd` 指定時: 選択ファイルをそのコマンドで起動し、終了までブロック → 一覧を再スキャン（Enter と同じフロー）。未指定時は無効（`no --alt-open-cmd` をフラッシュ） |
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

## 複数選択 → 子プロセス起動のフロー（`--open-cmd` / `--alt-open-cmd` 指定時のみ）

```
1. Space でファイルを選択（複数可）
2. Enter（または `o`）→ ashiato は子プロセスとして --open-cmd（`o` は --alt-open-cmd）を起動
   例: akapen /path/a.md /path/b.rs /path/c.toml
3. ashiato は子プロセスの終了を待つ（ブロック）
4. 子プロセス終了 → ファイル一覧を再スキャン
   → エージェントが編集したファイルの mtime が更新され上に来る
5. ashiato の TUI に復帰
```

`--open-cmd` も `--alt-open-cmd` も未指定時はこのフロー自体がなく、Enter は出力モード（下記）。

---

## `--files` モード（時間ブラウザのデータソース）

TUI を開かずに「時間順に並んだファイル一覧」を出力する。ashiato の
収集ロジック（herdr 解決・gitignore・常時除外・隠し/ディレクトリ切替・
ソート・フィルタ）がそのままデータソースになる。

```bash
ashiato --files | fzf                                # 時間順 × ファジー検索
ashiato --files --format tsv | fzf --delimiter $'\t' --with-nth 1..2 \
  --preview 'bat --color=always {3}'                 # 時間列 + 名前表示、プレビューはフルパス
ashiato --files --format tsv | fzf --delimiter $'\t' --with-nth 2 --nth 2.. \
  --preview 'bat --color=always {3}'                 # ファイル名だけ表示（検索は名前+パス）
ashiato --since today --filter .md                   # TUI: 今日の md をプレビュー付きでブラウズ
ashiato --files --since today | fzf                  # 今日触ったファイルだけ
ashiato --files --since 1w --filter .md | xargs akapen  # 今週の md をまとめてレビュー
ashiato --files | fzf --bind 'ctrl-r:reload(ashiato --files)'  # ライブ更新（手動）
```

位置づけ: ashiato は「**時間ブラウザ**」（直近の変更がデフォルトビュー）であり、
検索ツールではない。ファジー検索・高度なプレビューが必要な場合は
`--files` で fzf に委ねる（相互運用）。akapen レビューループ
（ブロック → 再スキャン → 状態維持）は ashiato 単体の領分。

## 既定動作（出力モード・UNIX フィルタ）

`--open-cmd` 未指定（= 既定）のとき、Enter は選択パスを stdout に出力して終了する。
`--output` はこの既定動作の明示指定（`--open-cmd` と併用不可）。

```
$ ashiato .
# TUI が開き、Space で選択 → Enter で選択ファイルパスが stdout に出力されて終了
/path/to/design.md
/path/to/src/main.rs

$ ashiato . --show-hidden
# 隠しファイルも表示
```

パイプでの利用:

```bash
ashiato . | xargs akapen
ashiato . | xargs vim -p
ashiato --sort ctime | while read f; do echo "==> $f"; cat "$f"; done
```

stdout がパイプの場合、TUI は `/dev/tty` に描画されるため
パイプにはエスケープ列が混入しない。

---

## herdr 連携

### herdr 上の自動ディレクトリ解決

ashiato は `HERDR_ENV=1` の場合、起動時に以下を試行:

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
# ~/.config/yazi/keymap.toml（現行 yazi の [mgr] 構文）
[mgr]
prepend_keymap = [
  # g f: 現在のディレクトリを ashiato で開いて akapen レビューループ
  { on = [ "g", "f" ], run = "shell 'ashiato . --open-cmd akapen' --block", desc = "Flat mtime picker → akapen" },
  # A: ashiato で時間順に選んだファイルへジャンプ（ya emit reveal）
  # 現行ディレクトリを明示指定: 移動後に A を押すと「今いる場所」を再スキャン
  # （引数なしだと herdr 環境で worktree ルートに飛んでしまう）
  { on = "A", run = 'shell --block -- p=$(ashiato "$PWD" 2>/dev/null | head -1); [ -n "$p" ] && ya emit reveal "$p"', desc = "ashiato → reveal" },
  # R: 選択ファイルを akapen でレビュー（種別問わず強制、Esc で yazi に即復帰）
  { on = "R", run = "shell 'akapen %s --send-agent --esc-quit always' --block", desc = "akapen review" },
]
```

種別ごとのディスパッチは `~/.config/yazi/yazi.toml` の opener ルールに任せる
（ashiato 側には増やさない）:

```toml
[opener]
akapen = [
  { run = "akapen %s --send-agent --esc-quit always", desc = "akapen review", block = true },
]

[open]
prepend_rules = [
  # md → akapen（Enter でレビュー、O で edit も選べる）
  # マッチャーキー: yazi 25.5 系は name（v25.12.29 以降は url に改名）
  { name = "*.md", use = [ "akapen", "edit" ] },
]
```

ashiato 側の `o` キー（`--alt-open-cmd`）と組み合わせると、ashiato ⇄ yazi の往復
（Enter = akapen、`o` = yazi）が 1 つのループで回る。`A` は現行ディレクトリを
明示的に渡すため、yazi でディレクトリを移動してから `A` を押すと「今いる場所」
の時間ブラウザが開き、Enter でそのファイルにジャンプして yazi に戻る
（ashiato → yazi → 移動 → ashiato → … のリベースループ）。

### herdr ラッパーでの akapen 連携

```bash
#!/bin/bash
# herdr ラッパーが ashiato + akapen を繋ぐ例。
# akapen の `--send-agent` が「現在タブの唯一のエージェント」を自動解決して
# `herdr agent prompt <pane> <text>` で送信する（argv 直接渡し、シェル非経由）。
# 0件・複数で曖昧なら赤トーストで拒否し、コメントは保持される。

ashiato . --open-cmd "akapen {} --send-agent" --alt-open-cmd "yazi {}"
```

`--alt-open-cmd` は `o` キー用。herdr プラグイン（`scripts/ashiato-pane.sh`）は既定で `yazi {}` を渡し、`ASHIATO_ALT_OPEN_CMD` で差し替え可能。

> **2026-08-05 決定**: 当初案の `--send-cmd 'herdr pane run ...'` 方式は、
> `herdr pane run` がコマンド引数必須で stdin を読まない（usage エラーになる）ため
> 不採用。akapen 側に `--send-agent`（タブ内唯一エージェントへ自動送信、
> herdr agent prompt 経由）を実装し、ラッパーは引数を渡すだけで済むようにした。
> 実機確認済み（2026-08-05: 使い捨てタブのテスト agent に reviewr 形式ブロックが
> 届き ack 応答）。

ashiato 自身は herdr 非依存。連携は外側のラッパー（または yazi の opener 設定）の責務。

---

## `--open-cmd` のプレースホルダ

| プレースホルダ | 展開結果 |
|---|---|
| `{}` | 選択ファイルのパスをスペース区切りで全件 |
| `{1}` `{2}` ... | 1番目、2番目...のファイルパス |

```bash
ashiato . --open-cmd "akapen --theme Nord {}"
ashiato . --open-cmd "vim -p {}"
```

---

## 実装

### 技術スタック

- Rust + ratatui 0.30（akapen と共通）
- crates: `anyhow`, `ratatui`, `syntect`, `ignore`, `unicode-width`, `chrono`
- standalone リポジトリ。akapen とは `--open-cmd` に渡すコマンド名の文字列だけで繋がる疎結合

### ファイル構成

```
src/main.rs      — エントリ、フラグ解析、App 状態、イベントループ、描画、キーバインド
src/files.rs     — ignore クレートでの収集、ソート、時間クラスタ、フィルタ
src/preview.rs   — 右ペイン: syntect ハイライト / バイナリは file(1) 的表示
src/theme.rs     — OSC 11 背景色検出 + light/dark UI 色（akapen と同一定数）
src/highlight.rs — akapen の highlight.rs の手動コピー
src/herdr.rs     — herdr ディレクトリ解決（JSON パース）
src/clipboard.rs — pbcopy / wl-copy / xclip / xsel
```

### プレビューの syntect（akapen とのコード共有）

`src/highlight.rs` は akapen からの**手動コピー**。ズレても「プレビューの色が
少し違う」だけで壊れないため許容する。このコピーへの変更要求が3回目になったら、
そのとき highlight だけを小さな共通クレートへ切り出す（YAGNI。それまではやらない）。

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
| ツリー表示 | yazi / broot の領分。ashiato はフラット mtime 専用 |
| プレビュー内スクロール | ashiato はピッカー。深いプレビューは akapen でやる |
| `.md` のレンダリングプレビュー | syntect ハイライトで十分。レンダリングは akapen の view モードで |
| git ステータス表示（`M`/`A`/`?`） | 後回し。akapen の git 連携と同時期に検討 |
| `d` で `git diff` 表示 | 同上 |
| 設定ファイル | 設定項目が少ない。使ってから必要になったら追加 |
| Nerd Font アイコン・絵文字 | フォント依存・見た目。拡張子で十分（📄 等の絵文字も 2026-08-05 に廃止） |
| ABC ソート | 普通のファイラでやればいい。ashiato は時系列専用 |
| キーバインドのカスタマイズ | キーが少なく vim 準拠。カスタマイズ需要は低い |

---

## テスト項目

1. ディレクトリ指定なし → カレントディレクトリ（または herdr cwd）を再帰的に収集
2. mtime 降順で表示 → 最近のファイルが上に来る
3. クラスタリング → Today / Yesterday / 日付見出しが正しく区切られる
4. `t` → ソート巡回が正しく動作
5. `/` → フィルタがインクリメンタルに動作
6. `Space` → 複数選択 → Enter → akapen にファイルが渡される
7. akapen 終了後 → ファイル一覧が再スキャンされ最新状態に
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
18. light/dark の UI 色が akapen と同一の定数で解決される
19. 行レイアウトの短縮: ベース名が最優先（ディレクトリ → `…/` 短縮 → ベース名 `…` 短縮）、文字境界安全
