#!/bin/bash
# ashiato herdr plugin action — 横分割で開くピッカー。
# 既定: mtime 順 md ピッカー（ラベル ashiato）。
# `--view read` 付き: read ビュー（ラベル ashiato-read、フィルタなし）—
# エージェントが Read したファイルのライブビュー（Enter = akapen で「読んだ
# ものをレビュー」）。
# トグル: タブに同じラベルの ashiato pane があれば focus、なければ新規作成（増えない）。
# 分割はタブのエージェント pane 基準（パレット経由の invoke では --current が
# パレット overlay 自体を指すため、akp plugin と同じ対策）。
# akapen の s 送信は --send-agent（タブ内唯一エージェントへ herdr 直送。
# 曖昧/不在時はトースト拒否 + コメントはクリップボード）。
# 時間カットオフ（--since）は付けない: 古い md も「Older」クラスタで拾えるように。
set -euo pipefail

# 引数: --view read で read ビュー（ラベル ashiato-read、フィルタなし）。
VIEW="mtime"
if [ "${1:-}" = "--view" ]; then
  VIEW="${2:-mtime}"
fi
LABEL="ashiato"
FILTER=".md"
if [ "$VIEW" = "read" ]; then
  # read ビューはエージェントが読んだもの全部（種類問わず）を見せる。
  LABEL="ashiato-read"
  FILTER=""
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# ashiato は TUI をサスペンドして open-cmd を実行し、終了後にピッカーへ戻る）→ pane は閉じない。
# Esc で抜けるため akapen 側に --esc-quit always を渡す。
# ashiato を終了（q / Ctrl+C）したら、使い捨ての pane ごと閉じる。
OPEN="${ASHIATO_OPEN_CMD:-akapen --send-agent --esc-quit always}"
# o キー（--alt-open-cmd）: 選択ファイルを yazi で開く。q で抜けるとピッカーへ戻る。
# ASHIATO_ALT_OPEN_CMD で差し替え可能（例: "code {}"）。
ALT_OPEN="${ASHIATO_ALT_OPEN_CMD:-yazi {}}"

TAB="${HERDR_TAB_ID:-}"
if [ -z "$TAB" ] && [ -n "${HERDR_PLUGIN_CONTEXT_JSON:-}" ]; then
  TAB=$(printf '%s' "$HERDR_PLUGIN_CONTEXT_JSON" | jq -r '.tab_id // empty')
fi
if [ -z "$TAB" ]; then
  for _ in 1 2; do
    TAB=$(herdr pane current --current 2>/dev/null | jq -r '.result.pane.tab_id // empty') || TAB=""
    [ -n "$TAB" ] && break
    sleep 0.2
  done
fi
[ -n "$TAB" ] || { echo "ashiato: cannot resolve tab" >&2; exit 1; }

AGENT_PANE=$(herdr agent list 2>/dev/null | jq -r --arg tab "$TAB" \
  '.result.agents[] | select(.tab_id == $tab and .agent != null) | .pane_id' | head -1)
SPLIT_TARGET=("--current")
[ -n "$AGENT_PANE" ] && SPLIT_TARGET=("--pane" "$AGENT_PANE")

EXISTING=$(herdr pane list | jq -r --arg tab "$TAB" --arg label "$LABEL" \
  '.result.panes[] | select(.tab_id == $tab and .label == $label) | .pane_id' | head -1)
if [ -n "$EXISTING" ]; then
  if [ -n "$AGENT_PANE" ]; then
    herdr pane focus --direction right --pane "$AGENT_PANE" 2>/dev/null || true
  else
    herdr pane focus --direction right --current 2>/dev/null || true
  fi
  exit 0
fi

RESPONSE=$(herdr pane split "${SPLIT_TARGET[@]}" --direction right --focus)
PANE_ID=$(printf '%s' "$RESPONSE" | jq -r '.result.pane.pane_id')
herdr pane rename "$PANE_ID" "$LABEL"
# ashiato が終了したら `;` の右で pane を閉じる（pane は使い捨て）。
# 終了 = q / Ctrl+C のみ。Esc は akapen を抜けてピッカーへ戻るだけなので閉じない。
CMD="ashiato --view $VIEW"
[ -n "$FILTER" ] && CMD="$CMD --filter $FILTER"
CMD="$CMD --open-cmd '$OPEN {}' --alt-open-cmd '$ALT_OPEN'"
herdr pane run "$PANE_ID" "$CMD; herdr pane close $PANE_ID"
