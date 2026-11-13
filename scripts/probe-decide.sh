#!/bin/sh
# Measures a decision endpoint (POST <ENGRAM_DECIDE_URL>/systemone): cost, parallelism, limits, behaviour.
# Needs curl, jq, ENGRAM_DECIDE_URL, ENGRAM_DECIDE_MODEL; ENGRAM_DECIDE_KEY goes to curl on stdin only.
# STATES sets the cost row sizes in words; exit 0 when cost, questions, repeat and behaviour answered 200.
set -eu

case ${1:-} in
-h | --help)
  sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
  ;;
esac
for v in ENGRAM_DECIDE_URL ENGRAM_DECIDE_MODEL; do
  eval "val=\${$v:-}"
  [ -n "$val" ] || { echo "probe-decide.sh: set $v" >&2; exit 2; }
done
for t in curl jq; do
  command -v "$t" >/dev/null 2>&1 || { echo "probe-decide.sh: $t is needed" >&2; exit 2; }
done

STATES=${STATES:-"0 400 2400 4800 7500"}
url="${ENGRAM_DECIDE_URL%/}/systemone"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# post BODY OUT: prints "<status> <seconds>"; the body of the reply lands in OUT.
cat > "$tmp/post" <<'W'
#!/bin/sh
body=$1 out=$2
if [ -n "${ENGRAM_DECIDE_KEY:-}" ]; then
  printf 'Authorization: Bearer %s\n' "$ENGRAM_DECIDE_KEY" |
    curl -s -o "$out" -w '%{http_code} %{time_total}\n' -X POST "$ENGRAM_DECIDE_URL_FULL" \
      -H 'Content-Type: application/json' -H @- --data-binary "@$body" 2>/dev/null || true
else
  curl -s -o "$out" -w '%{http_code} %{time_total}\n' -X POST "$ENGRAM_DECIDE_URL_FULL" \
    -H 'Content-Type: application/json' --data-binary "@$body" 2>/dev/null || true
fi
W
ENGRAM_DECIDE_URL_FULL=$url
export ENGRAM_DECIDE_URL_FULL

# words N: N words of one repeated sentence (eight words each).
words() {
  awk -v n="$1" 'BEGIN { for (i = 0; i < n / 8; i++) printf "The quick brown fox jumps over a dog. " }'
}
# state_file N: the state of N words in a file.
state_file() {
  words "$1" > "$tmp/state.$1"
  echo "$tmp/state.$1"
}
# body STATEFILE QUESTIONS_JSON OUT
body() {
  jq -nc --arg model "$ENGRAM_DECIDE_MODEL" --rawfile state "$1" --argjson q "$2" \
    '{model: $model, state: $state, questions: $q}' > "$3"
}
noul() {
  jq -nc --argjson n "$1" '[range($n)] | map({key: "q\(.)", value: {type: "noul", instructions: "Is water wet?"}}) | from_entries'
}
# call STATEFILE QUESTIONS_JSON: sets status, secs, out
call() {
  body "$1" "$2" "$tmp/req"
  res=$(sh "$tmp/post" "$tmp/req" "$tmp/out")
  status=${res%% *} secs=${res#* }
  [ -n "$res" ] || { status=000 secs=0; }
  [ -s "$tmp/out" ] || : > "$tmp/out"
}
need200() { [ "$status" = 200 ] || fail=1; }

echo "== Cost: one question, by state size"
best=
for w in $STATES; do
  call "$(state_file "$w")" "$(noul 1)"
  need200
  tokens=$(jq -r '.usage.input_tokens // empty' "$tmp/out" 2>/dev/null || true)
  tokens=${tokens:--}
  printf '%6s words  input_tokens %-6s %ss  status %s\n' "$w" "$tokens" "$secs" "$status"
  if [ "$status" = 200 ] && [ "$tokens" != "-" ] && [ "$tokens" -gt 0 ]; then
    best="$tokens $secs"
  fi
done
if [ -n "$best" ]; then
  # shellcheck disable=SC2086
  set -- $best
  awk -v t="$1" -v s="$2" 'BEGIN { printf "ms per input token (largest answered): %.2f\n", s * 1000 / t }'
else
  echo "ms per input token: none answered"
fi

echo
echo "== Questions: 2400-word state"
s2400=$(state_file 2400)
for n in 1 2 4; do
  call "$s2400" "$(noul "$n")"
  need200
  printf '%2s questions  %ss  status %s\n' "$n" "$secs" "$status"
done

echo
echo "== Parallel: 16 requests of 400 words"
body "$(state_file 400)" "$(noul 1)" "$tmp/par.req"
for p in 1 4 8; do
  : > "$tmp/par.res"
  start=$(date +%s.%N)
  # shellcheck disable=SC2016
  seq 16 | xargs -P "$p" -I{} sh -c 'sh "$1" "$2" "$3.$4" >> "$5"' _ "$tmp/post" "$tmp/par.req" "$tmp/par.out" {} "$tmp/par.res"
  end=$(date +%s.%N)
  counts=$(cut -d' ' -f1 "$tmp/par.res" | sort | uniq -c | awk '{ printf "%s x%s ", $2, $1 }')
  printf '%2s at a time  %ss wall  %s\n' "$p" "$(awk -v a="$start" -v b="$end" 'BEGIN { printf "%.1f", b - a }')" "$counts"
done

echo
echo "== Repeat: the same request twice"
body "$s2400" "$(noul 1)" "$tmp/rep.req"
sh "$tmp/post" "$tmp/rep.req" "$tmp/rep.1" > "$tmp/rep.s1"
sh "$tmp/post" "$tmp/rep.req" "$tmp/rep.2" > "$tmp/rep.s2"
for f in 1 2; do
  [ "$(cut -d' ' -f1 "$tmp/rep.s$f")" = 200 ] || fail=1
done
if [ "$(jq -S '.answers' "$tmp/rep.1" 2>/dev/null)" = "$(jq -S '.answers' "$tmp/rep.2" 2>/dev/null)" ] && [ -s "$tmp/rep.1" ]; then
  echo identical
else
  echo differs
fi

echo
echo "== Limits (status, then the status Measured recorded)"
empty=$tmp/state.empty
: > "$empty"
levels=$(jq -nc '{s: {type: "score", instructions: "How wet?", criteria: [range(11) | "level \(.)"]}}')
call "$empty" "$levels"
printf '11 score levels            %s  (400)\n' "$status"
call "$empty" '{"q0": {"type": "noul"}}'
printf 'no instructions             %s  (400)\n' "$status"
call "$(state_file 12000)" "$(noul 1)"
printf '12000-word state            %s  (500)\n' "$status"

echo
echo "== Behaviour"
question='Which port does the service use?'
bears="Does this excerpt contain information that helps answer the question: «$question»?"
bq=$(jq -nc --arg t "$bears" '{bears: {type: "noul", instructions: $t}}')
printf 'The service listens on port 8443 since the move.' > "$tmp/pass.yes"
printf 'The office plants are watered on Tuesdays.' > "$tmp/pass.no"
for k in yes no; do
  call "$tmp/pass.$k" "$bq"
  need200
  v=$(jq -r '.answers.bears.noul // empty' "$tmp/out" 2>/dev/null || true)
  printf 'bears, passage %-3s %s  status %s\n' "$k" "${v:--}" "$status"
done
fwd=$(jq -nc '{c: {type: "choice", instructions: "Which port does the service use?", criteria: {"8443": "the new port", "8080": "the old port"}}}')
rev=$(jq -nc '{c: {type: "choice", instructions: "Which port does the service use?", criteria: {"8080": "the old port", "8443": "the new port"}}}')
for d in forward reversed; do
  if [ "$d" = forward ]; then q=$fwd; else q=$rev; fi
  call "$tmp/pass.yes" "$q"
  need200
  pk=$(jq -r '.answers.c.choice // empty' "$tmp/out" 2>/dev/null || true)
  pr=$(jq -c '.answers.c.probabilities // empty' "$tmp/out" 2>/dev/null || true)
  printf 'choice %-8s pick %s  probabilities %s  status %s\n' "$d" "${pk:--}" "${pr:--}" "$status"
done

exit "$fail"
