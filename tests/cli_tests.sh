#!/usr/bin/env bash
# Smoke tests for ztch — run from the repo root.
#   bash tests/cli_tests.sh        # all tests
#   bash tests/cli_tests.sh -v     # verbose
#   bash tests/cli_tests.sh <str>  # matching tests only

set -euo pipefail

ZTCH="${ZTCH:-./target/release/ztch}"
VERBOSE=false
[[ $# -gt 0 && "$1" == "-v" ]] && { VERBOSE=true; shift; }
FILTER="${1:-}"

PASS=0 FAIL=0
TMPHOME=""

cleanup() {
    local rc=$?
    [[ -z "$TMPHOME" ]] && exit "$rc"
    for pid in $(jobs -p 2>/dev/null); do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$TMPHOME" 2>/dev/null || true
    exit "$rc"
}
trap cleanup EXIT INT TERM

setup() { TMPHOME="$(mktemp -d)"; export HOME="$TMPHOME"; }

diag() { $VERBOSE && echo "  │ $*"; return 0; }
pass() { echo "  PASS  $1"; PASS=$((PASS+1)); }
fail() { echo "  FAIL  $1"; FAIL=$((FAIL+1)); }

run_ok()   { local l="$1"; shift; "$@" >/dev/null 2>&1 && pass "$l" || fail "$l"; }
run_grep() { local l="$1" p="$2"; shift 2; local o; o=$("$@" 2>&1) || true
    [[ "$o" == *"$p"* ]] && { pass "$l"; diag "$o"; } || { fail "$l"; diag "want: *$p*"; diag "got: $o"; }; }

start_sesh() { $ZTCH start "$1" >/dev/null 2>&1 || true; }
kill_sesh()  { timeout 8 $ZTCH kill "$1" >/dev/null 2>&1 || true; }

# ── tests ────────────────────────────────────────────────────────────

t_help() {
    echo "── help / version ──"
run_ok   "--help"           $ZTCH --help
run_grep "-h" "Usage:"     $ZTCH -h
run_ok   "--version"        $ZTCH --version
}

t_list_empty() {
    echo "── list empty ──"
    for s in "$TMPHOME"/.cache/ztch/*; do
        [[ -S "$s" ]] && timeout 3 $ZTCH kill "$(basename "$s")" >/dev/null 2>&1 || true
    done
    run_grep "list" "(no sessions)" $ZTCH list
}

t_start_kill() {
    echo "── start / kill ──"
    start_sesh t-sk
    run_grep "list shows" "t-sk" $ZTCH list
    kill_sesh t-sk
}

t_kill_missing() {
    echo "── kill missing ──"
    run_grep "does not exist" "does not exist" $ZTCH kill no-such-sesh
}

t_run_exits() {
    echo "── run ──"
    $ZTCH run t-run true &
    local pid=$!
    sleep 0.4
    wait "$pid" 2>/dev/null && pass "run exits" || { fail "run exits"; kill "$pid" 2>/dev/null || true; }
}

t_push() {
    echo "── push ──"
    start_sesh t-push
    echo "data" | run_ok "push" $ZTCH push t-push
    kill_sesh t-push
}

t_rm() {
    echo "── rm ──"
    start_sesh t-rm; kill_sesh t-rm
    run_grep "rm" "removed" $ZTCH rm t-rm
}

t_rm_all() {
    echo "── rm -a ──"
    start_sesh t-rma; kill_sesh t-rma
    run_grep "rm -a" "removed" $ZTCH rm -a
}

t_rm_running() {
    echo "── rm running ──"
    start_sesh t-rmr
    run_grep "is running" "is running" $ZTCH rm t-rmr
    kill_sesh t-rmr
}

t_clear() {
    echo "── clear ──"
    start_sesh t-cl
    run_grep "clear" "cleared" $ZTCH clear t-cl
    kill_sesh t-cl
}

t_tail() {
    echo "── tail ──"
    $ZTCH run t-tail sh -c 'echo TAILTEST; sleep 0.1' &
    local pid=$!
    sleep 0.6
    run_grep "tail" "TAILTEST" $ZTCH tail t-tail -n 5
    wait "$pid" 2>/dev/null || true
}

t_info() {
    echo "── info ──"
    start_sesh t-info
    run_grep "info attached"  "attached: 0"  $ZTCH info t-info
    run_grep "info missing"   "does not exist" $ZTCH info no-such-sesh
    kill_sesh t-info
}

t_detach() {
    echo "── detach ──"
    start_sesh t-det
    run_grep "detach single"      "detached"         $ZTCH detach t-det
    run_grep "session still up"   "t-det"            $ZTCH list
    kill_sesh t-det
}

t_detach_all() {
    echo "── detach -a ──"
    for i in 1 2; do start_sesh "t-dall$i"; done
    run_grep "detach -a"          "detached"         $ZTCH detach -a
    run_grep "sessions still up"  "t-dall1"          $ZTCH list
    for i in 1 2; do kill_sesh "t-dall$i"; done
}

t_detach_missing() {
    echo "── detach missing ──"
    run_grep "missing"            "does not exist"   $ZTCH detach no-such-sesh
}

t_detach_no_session() {
    echo "── detach no session ──"
    run_grep "no session"         "no session specified"  $ZTCH detach
}

t_multi() {
    echo "── multiple sessions ──"
    for i in 1 2 3; do start_sesh "t-m$i"; done
    local c
    c=$($ZTCH list 2>&1 | grep -c "t-m") || true
    [[ "$c" -eq 3 ]] && pass "list shows 3" || fail "list shows 3 (got $c)"
    for i in 1 2 3; do kill_sesh "t-m$i"; done
}

# ── main ─────────────────────────────────────────────────────────────

echo "ztch smoke tests  ($ZTCH)"
echo
setup

TESTS=(t_help t_list_empty t_start_kill t_kill_missing t_run_exits
       t_push t_info t_detach t_detach_all t_detach_missing t_detach_no_session
       t_rm t_rm_all t_rm_running t_clear t_tail t_multi)

for fn in "${TESTS[@]}"; do
    if [[ -z "$FILTER" || "$fn" == *"$FILTER"* ]]; then "$fn"; fi
done

echo
echo "  PASS $PASS  FAIL $FAIL  total $((PASS+FAIL))"
echo
[[ "$FAIL" -eq 0 ]]
