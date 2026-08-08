#!/bin/bash
# ashiato herdr plugin action — prefix+m: md ピッカーを横分割で開く。
# トグル: タブに ashiato pane があれば focus、なければ新規作成（増えない）。
# 分割はタブのエージェント pane 基準（パレット経由の invoke では --current が
# パレット overlay 自体を指すため、akp plugin と同じ対策）。
# akapen の s 送信は --send-agent（タブ内唯一エージェントへ herdr 直送。
# 曖昧/不在時はトースト拒否 + コメントはクリップボード）。
# 時間カットオフ（--since）は付けない: 古い md も「Older」クラスタで拾えるように。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OPEN="${ASHIATO_OPEN_CMD:-akapen --send-agent}"

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

EXISTING=$(herdr pane list | jq -r --arg tab "$TAB" \
  '.result.panes[] | select(.tab_id == $tab and .label == "ashiato") | .pane_id' | head -1)
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
herdr pane rename "$PANE_ID" "ashiato"
herdr pane run "$PANE_ID" "ashiato --filter .md --open-cmd '$OPEN {}'"
