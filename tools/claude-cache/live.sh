#!/usr/bin/env bash
# Live prompt-cache check for the cloaked Claude path. Run by hand against a running
# cliproxy-rs whose Claude login is an OAuth subscription; it is not part of CI and it
# spends real tokens (about 75k input tokens on Opus, most of them cache reads).
#
#   CPA_URL=http://127.0.0.1:8317 CPA_KEY=<client key> tools/claude-cache/live.sh [results.jsonl]
#
# It streams one conversation through /v1/messages the way a coding client does (a
# caller system prompt, tools, a long first user message, a tool call as the first
# reply) and prints Anthropic's own usage for each turn: uncached input, cache reads,
# and cache writes split into the 5-minute and 1-hour pools.
#
# The first reply is a tool call on purpose. An OAuth login moves the caller prompt to
# a system turn just before the first assistant turn. In probes on claude-opus-5-5 on
# 14 October 2026, the next turn did not reuse the cold turn's cache entry for that
# system turn when the assistant turn after it held only text, either made up or the
# model's own reply with its thinking block removed: it read up to the first user
# message and wrote the caller prompt again. A tool call, or the model's reply replayed
# with its thinking block, reused it. The messages sent upstream were byte-identical
# through the system turn in both cases, and so were the tools and the identity block;
# only the billing header differed, as it does on every turn. That points to
# Anthropic's cache handling, not to the proxy.
#
#   cold     the first turn: writes the prefix.
#   warm     the next turn at once: reads everything the cold turn wrote, writes its tail.
#   changed  the warm turn with an edited caller system prompt: reads up to the first
#            user message, writes the rest (an OAuth login moves the caller prompt
#            after it).
#   idle     the turn after warm, IDLE_SECONDS later (default 330, past the 5-minute
#            TTL): still reads everything warm cached, because an OAuth login's
#            breakpoints are written to the 1-hour pool.
#
# Expectations are checked and the script exits 1 when one fails or the usage does not
# show them. With several Claude logins, keep the conversation on one of them (session
# affinity); the cache belongs to the account, so a turn routed to another login reads
# nothing. The key reaches curl on stdin, never on its command line, and is not printed;
# curl ignores ~/.curlrc.
#
# Environment: CPA_URL (default http://127.0.0.1:8317), CPA_KEY (required), MODEL
# (default claude-opus-5-5; a current model, since older ones put the caller prompt
# before the document), IDLE_SECONDS (default 330, between 301 and 3300), DOC_LINES
# (default 600, at least 300; about 30 tokens a line).
set +x
set -euo pipefail

CPA_URL=${CPA_URL:-http://127.0.0.1:8317}
MODEL=${MODEL:-claude-opus-5-5}
IDLE_SECONDS=${IDLE_SECONDS:-330}
DOC_LINES=${DOC_LINES:-600}
OUT=${1:-}
: "${CPA_KEY:?set CPA_KEY to a client key of the server}"
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
fail() { echo "$*" >&2; exit 2; }
[[ $CPA_KEY != *$'\n'* && $CPA_KEY != *$'\r'* ]] || fail "CPA_KEY must be one line"
# A shell variable from here on: curl, jq, sed and sleep do not inherit it.
export -n CPA_KEY
if ! [[ $IDLE_SECONDS =~ ^[0-9]+$ ]] || ((IDLE_SECONDS <= 300 || IDLE_SECONDS > 3300)); then
  fail "IDLE_SECONDS must be between 301 and 3300: past the 5-minute TTL, inside the 1-hour one"
fi
if ! [[ $DOC_LINES =~ ^[0-9]+$ ]] || ((DOC_LINES < 300)); then
  fail "DOC_LINES must be at least 300"
fi
case $MODEL in
claude-3-* | claude-haiku-4* | claude-sonnet-4 | claude-sonnet-4-[0-6]* | claude-opus-4 | claude-opus-4-[0-7]*)
  fail "$MODEL keeps caller prompts in the first user message; use a current model" ;;
esac

# A fresh conversation per run, so the cold turn is cold.
RUN="$(date -u +%Y%m%dT%H%M%SZ)-$RANDOM"
DOC=$(printf 'Release notes draft %s.\n' "$RUN"; seq 1 "$DOC_LINES" |
  sed 's/.*/Item &: the release checklist covers builds, tests, signing, packaging and notes./')
SYSTEM='You are a coding agent working in a large repository. Keep diffs small and explain each change.'
EDITED="$SYSTEM Run the tests before you report."
TOOLS='[{"name":"read_file","description":"Read a file from the repository.","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},{"name":"run_tests","description":"Run the test suite.","input_schema":{"type":"object","properties":{}}}]'
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

body() { # system, number of user turns
  jq -n --arg model "$MODEL" --arg system "$1" --argjson tools "$TOOLS" --arg doc "$DOC" --argjson turns "$2" '
    def text(t): [{type: "text", text: t}];
    [{role: "user", content: text($doc + "\nSummarise the checklist in one line.")},
     {role: "assistant", content: [{type: "tool_use", id: "toolu_01CacheLiveRead", name: "read_file",
                                    input: {path: "CHECKLIST.md"}}]},
     {role: "user", content: [{type: "tool_result", tool_use_id: "toolu_01CacheLiveRead",
                               content: "Builds, tests, signing, packaging and notes."}]},
     {role: "assistant", content: text("The checklist covers builds, tests, signing, packaging and notes.")},
     {role: "user", content: text("Reply with just: done")}] as $all
    | {model: $model, max_tokens: 16, stream: true, system: $system, tools: $tools,
       messages: $all[: (2 * $turns - 1)]}'
}

send() { # label, system, turns
  local label=$1 events usage
  body "$2" "$3" >"$WORK/body.json" || return 1
  if ! events=$(printf 'Authorization: Bearer %s\n' "$CPA_KEY" |
    curl -q -sS -N --fail-with-body -H @- "$CPA_URL/v1/messages" \
      -H 'Content-Type: application/json' -H 'Anthropic-Version: 2023-06-01' \
      --data-binary "@$WORK/body.json"); then
    echo "$label: request failed:" >&2
    printf '%s\n' "$events" | head -c 2000 >&2
    return 1
  fi
  # Usage arrives in message_start and again, cumulative, in message_delta: a later
  # number replaces an earlier one, a null leaves it.
  if ! usage=$(printf '%s\n' "$events" | sed -n 's/^data: //p' | jq -se '
      if any(.[]; .type == "error") then error(tostring) else . end
      | if any(.[]; .type == "message_stop") then . else error("no message_stop") end
      | [(.[] | select(.type == "message_start") | .message.usage), (.[] | select(.type == "message_delta") | .usage)]
      | map(select(. != null) | with_entries(select(.value != null))) | add'); then
    echo "$label: no complete answer:" >&2
    printf '%s\n' "$events" | head -c 2000 >&2
    return 1
  fi
  jq -c --arg step "$label" --arg run "$RUN" --arg model "$MODEL" '{
      step: $step, run: $run, model: $model,
      input: .input_tokens,
      cache_read: .cache_read_input_tokens,
      cache_write: .cache_creation_input_tokens,
      cache_write_5m: .cache_creation.ephemeral_5m_input_tokens,
      cache_write_1h: .cache_creation.ephemeral_1h_input_tokens}' <<<"$usage"
}

results=()
step() { # label, system, turns
  local r
  r=$(send "$@") || exit 1
  results+=("$r")
}
step cold "$SYSTEM" 1
step warm "$SYSTEM" 2
step changed "$EDITED" 2
echo "waiting ${IDLE_SECONDS}s for the idle turn" >&2
sleep "$IDLE_SECONDS"
step idle "$SYSTEM" 3

printf '%s\n' "${results[@]}" | jq -rs '
  (["step", "input", "cache_read", "cache_write", "write_5m", "write_1h"] | @tsv),
  (.[] | [.step, .input, .cache_read, .cache_write, .cache_write_5m, .cache_write_1h] | map(. // "?") | @tsv)' |
  column -t
if [[ -n $OUT ]]; then printf '%s\n' "${results[@]}" >>"$OUT"; fi

# Each turn's cached prefix is everything the turn before it read or wrote.
printf '%s\n' "${results[@]}" | jq -rse --argjson idle "$IDLE_SECONDS" '
  def n: type == "number";
  INDEX(.step) as $r
  | ($r.cold.cache_read + $r.cold.cache_write) as $cold
  | ($r.warm.cache_read + $r.warm.cache_write) as $warm
  | [
      ["Anthropic reported every counter", all(.[]; (.input | n) and (.cache_read | n) and (.cache_write | n)
          and (.cache_write_5m | n) and (.cache_write_1h | n))],
      ["the 5-minute and 1-hour writes add up to the writes", all(.[]; .cache_write_5m + .cache_write_1h == .cache_write)],
      ["cold writes its prefix to the 1-hour pool", $r.cold.cache_write_1h > 0],
      ["warm reads everything cold cached", $r.warm.cache_read >= $cold],
      ["warm writes its new tail", $r.warm.cache_write > 0],
      ["changed reads the shared prefix only", $r.changed.cache_read > 0 and $r.changed.cache_read < $cold],
      ["changed writes the new tail", $r.changed.cache_write > 0],
      ["idle reads everything warm cached after \($idle)s", $r.idle.cache_read >= $warm],
      ["no 5-minute writes", all(.[]; .cache_write_5m == 0)]
    ]
  | (.[] | "\(if .[1] then "ok  " else "FAIL" end) \(.[0])"), (all(.[]; .[1]) | if . then empty else error("a check failed") end)' ||
  exit 1
