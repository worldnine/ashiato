# revpick — prototype

プロジェクト全体のファイルを更新順（mtime 降順）でフラット表示する TUI ピッカー。
revpick-spec.md の実装プロトタイプ。最終形は mdcomment リポジトリの
`src/bin/revpick.rs` だが、まずこのディレクトリで単体プロジェクトとして実装・検証する。

## ビルド / 実行

```bash
cargo build --release
./target/release/revpick [directory] [flags]
```

| flag | 説明 |
|---|---|
| `--sort mtime\|ctime` | 初期ソート基準（`t` で mtime↓→mtime↑→ctime↓→ctime↑） |
| `--show-hidden` | 起動時にドットディレクトリを表示（`Ctrl+h` で切替） |
| `--show-dirs` | 起動時にディレクトリを表示（`d` で切替） |
| `--open-cmd <cmd>` | Enter で起動するコマンド（**既定なし**）。`{}` / `{1}`.. プレースホルダ可。未指定時は Enter が選択パスを stdout に出力して即終了 |
| `--filter <text>` | 初期フィルタ（`/` と同じマッチング。例: `--filter .md` で md のみ）。`\` で一時解除・復帰、`/` で編集・解除可 |
| `--preview <on\|off\|auto>` | プレビューペイン。`auto`（既定）は 80 列未満で非表示 |
| `--theme <name>` | プレビューの syntect テーマ（two-face 埋め込み名 or `.tmTheme` パス。既定: `Catppuccin Mocha`。light 自動検出時は `Solarized (light)`） |
| `--light` / `--dark` | light/dark の明示指定（既定: **自動検出**。OSC 11 でターミナル背景色を問い合わせ、応答の輝度で判定。応答なしは dark） |
| `--files` | TUI なしで時間順パス一覧を出力（fzf 等へのデータソース） |
| `--format <path\|tsv>` | `--files` の出力形式（tsv = `日時<TAB>名前<TAB>パス` の3フィールド） |
| `--since <today\|yesterday\|Nd\|Nw>` | 時間カットオフ（`--files` の絞り込み + TUI の初期表示にも適用） |
| `--output` | 出力モードの明示指定（既定動作と同じ）。`--open-cmd` と排他 |

ディレクトリ解決: 引数 → herdr（`HERDR_ENV=1` → `herdr worktree list` の
`source.repo_root` → `herdr agent list` の同ワークスペース cwd）→ カレント。

stdout がパイプの場合、TUI は `/dev/tty` に描画するので
`revpick . | xargs mdcomment` がエスケープ汚染なしで動く。

## 構造

```
src/main.rs      — エントリ、フラグ解析、App 状態、イベントループ、描画、キーバインド
src/files.rs     — ignore クレートでの収集、ソート、時間クラスタ、フィルタ（テスト済み）
src/preview.rs   — 右ペイン: syntect ハイライト / バイナリは file(1) 的表示
src/theme.rs     — OSC 11 背景色検出 + light/dark UI 色（mdcomment view.rs の定数コピー）
src/highlight.rs — mdcomment からコピーした共用モジュール（そのまま merge 可能）
src/herdr.rs     — herdr ディレクトリ解決（JSON パース）
src/clipboard.rs — pbcopy / wl-copy / xclip / xsel（mdcomment export.rs 由来）
```

## 配置方針（2026-08-05 決定）

revpick は **mdcomment リポジトリには移さない**。standalone のまま維持する。

理由: 設計の前提が**疎結合**。revpick は汎用ピッカーであり、mdcomment との
繋がりは `--open-cmd` の既定値（コマンド名の文字列）だけ。実行時には
`sh -c "mdcomment ..."` で spawn するのみで中身を知らず、`--open-cmd "vim -p {}"`
などに差し替え可能。同じリポジトリに置くとビルド・依存・開発サイクルが結合し、
「独立したツール」という前提と矛盾する。

- `src/highlight.rs` は mdcomment の `src/highlight.rs` からの**コピー**。
  変更時は手動で同期する:
  `cp /Users/nagata/src/tries/2026-08-01-mdcomment-hermes/src/highlight.rs src/`
- ズレても「プレビューの色が少し違う」だけで壊れない（許容コスト）
- **将来の目安**: このコピーへの変更要求が3回目になったら、そのときに
  highlight だけを小さな共通クレートへ切り出す（YAGNI。それまではやらない）

## 仕様との対応（テスト項目）

| 仕様テスト | 場所 |
|---|---|
| 1. ディレクトリ解決（cwd / herdr） | `resolve_root` + herdr.rs テスト、実機で herdr 解決確認済み |
| 2. mtime 降順表示 | `sort_entries` + テスト |
| 3. クラスタリング | `cluster_of` + 境界テスト（固定 now） |
| 4. `t` ソート巡回 | `Sort::next` + テスト |
| 5. `/` インクリメンタルフィルタ | `matches_filter` + テスト（`/src/`, `/.md` 例対応） |
| 6. Space → Enter → mdcomment 起動 | tmux 実機確認済み（ブロック→復帰） |
| 7. 終了後再スキャン | `rescan` — 子プロセス中に作ったファイルが一覧に反映されるのを実機確認 |
| 8. `y` クリップボード | pbcopy 実機確認済み |
| 9. `Ctrl+h` | Backspace(0x7f) / Ctrl+h(0x08) 両対応、実機確認済み |
| 10. `d` | 実機確認済み（`📁 src/` が出る） |
| 11. 出力モード（既定 / `--output`） | パイプでエスケープ汚染なし、実機確認済み |
| 12. `.gitignore` | `scan_respects_gitignore` テスト（`require_git(false)` で非 git ディレクトリでも有効） |
| 13. `.claude/` 等が常時除外されない | `scan_applies_always_on_ignores_and_hidden_toggle` テスト |
| 14. 画像/バイナリでクラッシュしない | `binary_head_reports_file_info_without_crashing` テスト |
| 15. herdr 不在で起動する | `missing_herdr_binary_yields_none` + フォールバック |
| 16. OSC 11 応答のパース（長/短形式、`rgba:`、`#rrggbb`、ゴミ拒否） | `parse_osc11` + テスト |
| 17. `--light` / `--dark` パース、既定は自動検出 | `theme_and_light_flags_parse` |
| 18. light/dark UI 色が mdcomment と同一定数 | `ui_colors_follow_mdcomment_constants` |
| 19. 行レイアウトの短縮（dir 優先 → ベース名、文字境界安全） | `fit_path` / `truncate_*` + テスト |

## 実装上の決定（spec の解釈）

- **ファイル行にアイコンなし**（2026-08-05: 📄/📁 絵文字を廃止。拡張子で判別）
- **行レイアウト**: `> dir/ベース名  14:23` — ディレクトリ部分はグレー、ベース名は
  テーマの前景色（カーソル行は Cyan）。**表示優先度: ベース名 > ディレクトリ > 日時**。
  日時は余白があるときだけ右寄せ（クラスタ区切りが時間の文脈を担うため欠けてもよい）。
  パスが長いときはディレクトリを末尾コンポーネントから保持したまま `…/` で短縮、
  それでも収まらなければベース名を `…` で短縮
- **ライブ更新**: 2 秒ごとに再スキャンし、変化があれば一覧を自動置き換え（エージェントの編集が反映される）。カーソル・選択はパスで追従
- **`--preview` の既定は auto**（80 列未満で非表示、リストが全幅に）
- **`--files` で fzf と相互運用**（2026-08-05 追加）: `revpick --files --format tsv | fzf --delimiter $'\t' --with-nth 1..2 --preview 'bat --color=always {3}'` で「時間順 × ファジー検索」。tsv は `日時<TAB>名前<TAB>パス` の3フィールドで、`--with-nth 2` ならファイル名だけ表示も可。revpick は時間ブラウザであり検索は fzf に委ねられる
- **テーマ自動検出**（2026-08-05 追加）: `--theme` は mdcomment と同じ（two-face 名 or
  `.tmTheme` パス）。light/dark は起動時に OSC 11（`ESC ] 11 ; ?`）でターミナル背景色を
  問い合わせて判定 — `--light` / `--dark` で明示指定が最優先、応答しないターミナル
  （Terminal.app 等）は dark。TUI の選択行背景・ボーダーは mdcomment の `--light` と
  同一の定数で swap、ファイル名はテーマのデフォルト前景色で描画。カーソル行のみ
  Cyan アクセント（mdcomment の view モードと同じ `fg(Cyan).bg(選択背景)`）
- **`--open-cmd` の既定はなし**（2026-08-05 決定・spec 改訂済み）。未指定時
  Enter = 選択パスを stdout 出力して即終了（fzf モデル）。mdcomment の
  「起動 → ブロック → 再スキャン」ループは `--open-cmd mdcomment` を明示した
  時だけ有効になる opt-in で、既定は誰も連れてこない。`--output` は
  既定動作の明示的エイリアス。

- **隠し = ドットディレクトリのみ**。`.gitignore` は spec の例レイアウトで
  表示されているためドットファイルは常に表示（`.DS_Store` は常時除外リストで対処）。
- **フィルタは `/` を剥がして相対パスに substring 一致**（大文字小文字無視）。
  `/src/`・`/main.rs`・`/.md` の例がすべて成立する。
- **クラスタ境界**: Today → Yesterday → This week（今週月曜〜）→ Last week
  （先週月曜〜）→ This month（今月1日〜）→ Older の順で判定。先週月曜が今月1日より
  後の期間（月末〜月跨ぎ）は Last week が優先される。
- **プレビューは先頭 256KB まで**、キャッシュは (path, mtime, size, 幅, 高さ) キー。
- イベントループは mdcomment と同じ 100ms ポーリング + 1フレーム64イベント上限。
