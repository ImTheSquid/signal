#!/usr/bin/env bash
# Run a Rhai script on the traffic light continuously.
#
# A job's TTL is fixed when it is submitted, from the lock's *remaining* time —
# renewing the lock does not extend a script that is already running. So holding
# the light for a whole set means re-acquiring and resubmitting on a cycle, which
# is what this loop does. Ctrl-C releases the lock so nobody is left blocked.
#
#   scripts/run-follow.sh                      # runs scripts/follow.rhai
#   scripts/run-follow.sh scripts/other.rhai
#
# Token, first match wins:
#   $TRAFFIC_LIGHT_TOKEN
#   --token-file PATH   (file containing just the token)
#   ~/.config/traffic-light/token
#   ~/.claude/traffic-light.json   (JSON with a "token" field)

set -uo pipefail

BASE="${TRAFFIC_LIGHT_BASE:-https://signal.jackhogan.me}"
LOCK_S="${LOCK_S:-3600}"     # per-cycle lock length; the key caps this, and the
                             # ttl in the response is what actually gets waited on.
                             # Long cycles matter: every handover is a chance for a
                             # wifi drop to strand the light on the idle script.
MARGIN_S="${MARGIN_S:-45}"   # resubmit this long before the TTL runs out
SCRIPT=""
TOKEN_FILE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --token-file) TOKEN_FILE="$2"; shift 2 ;;
    --base)       BASE="$2"; shift 2 ;;
    --lock)       LOCK_S="$2"; shift 2 ;;
    -h|--help)    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)            SCRIPT="$1"; shift ;;
  esac
done

here=$(cd "$(dirname "$0")" && pwd)
SCRIPT="${SCRIPT:-$here/follow.rhai}"

if [ ! -f "$SCRIPT" ]; then
  echo "no such script: $SCRIPT" >&2
  exit 1
fi

read_token() {
  if [ -n "${TRAFFIC_LIGHT_TOKEN:-}" ]; then
    printf '%s' "$TRAFFIC_LIGHT_TOKEN"; return
  fi
  for f in "$TOKEN_FILE" "$HOME/.config/traffic-light/token"; do
    if [ -n "$f" ] && [ -f "$f" ]; then
      tr -d ' \t\r\n' < "$f"; return
    fi
  done
  if [ -f "$HOME/.claude/traffic-light.json" ]; then
    python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['token'])" \
      "$HOME/.claude/traffic-light.json"
    return
  fi
  return 1
}

TOKEN=$(read_token) || true
if [ -z "${TOKEN:-}" ]; then
  cat >&2 <<'EOF'
No API key found. Mint one in the admin panel, then either:
  export TRAFFIC_LIGHT_TOKEN=tl_...
  or write it to ~/.config/traffic-light/token   (chmod 600)
EOF
  exit 1
fi
AUTH="Authorization: Bearer $TOKEN"

# JSON-encode the script body. Doing this in python avoids hand-rolled quoting,
# which breaks on the backslashes and quotes real scripts contain.
#
# Deliberately sends the script as written. An earlier version minified it to dodge
# a firmware bug that silently dropped any frame over ~1KB; that bug is fixed in
# the firmware (see crates/wsframe), so the workaround is gone. A light still
# running firmware from before that fix will drop large scripts.
body() {
  python3 -c "import json,sys; print(json.dumps({'script': open(sys.argv[1]).read()}))" "$1"
}

release() {
  echo
  echo "releasing the lock..."
  curl -fsS -X DELETE "$BASE/v1/lock" -H "$AUTH" >/dev/null 2>&1 \
    && echo "released." || echo "release failed (it will expire on its own)."
  exit 0
}
trap release INT TERM

echo "light : $BASE"
echo "script: $SCRIPT"
echo "cycle : ${LOCK_S}s lock, resubmitting ${MARGIN_S}s before expiry"
echo

# POST and echo "<http_code> <body>", so a 409 can be told from a network error
# without issuing the request twice.
post() {
  curl -sS -X POST "$1" -H "$AUTH" -H 'Content-Type: application/json' \
    -d "$2" -w ' %{http_code}' 2>/dev/null | awk '{code=$NF; $NF=""; print code, $0}'
}

# Sleep such that a trapped signal is acted on immediately. Bash defers traps
# until the running command finishes, so a bare `sleep 285` would swallow Ctrl-C
# for minutes; backgrounding it and waiting does not.
nap() {
  sleep "$1" &
  wait $! 2>/dev/null
}

while true; do
  read -r code lock <<<"$(post "$BASE/v1/lock" "{\"duration_s\": $LOCK_S}")"
  if [ "$code" = "409" ]; then
    # Somebody else legitimately holds it; back off rather than fight.
    echo "$(date +%H:%M:%S) cannot lock (HTTP 409): $lock"
    nap 10
    continue
  fi
  if [ "$code" != "201" ]; then
    # Network trouble: the light is on the idle script while we dither, so
    # retry hard.
    echo "$(date +%H:%M:%S) lock failed (HTTP $code): $lock"
    nap 2
    continue
  fi

  # Re-read every cycle so editing the script takes effect on the next
  # resubmission rather than needing a restart.
  read -r code job <<<"$(post "$BASE/v1/script" "$(body "$SCRIPT")")"
  if [ "$code" != "202" ]; then
    echo "$(date +%H:%M:%S) submit failed (HTTP $code): $job"
    nap 2
    continue
  fi

  ttl=$(printf '%s' "$job" | python3 -c "import json,sys; print(json.load(sys.stdin).get('ttl_ms',0)//1000)" 2>/dev/null || echo "$LOCK_S")
  id=$(printf '%s' "$job" | python3 -c "import json,sys; print(json.load(sys.stdin).get('jobId','?')[:8])" 2>/dev/null || echo '?')
  echo "$(date +%H:%M:%S) running $id for ${ttl}s"

  wait_s=$((ttl - MARGIN_S))
  [ "$wait_s" -lt 5 ] && wait_s=5
  nap "$wait_s"
done
