#!/usr/bin/env bash
# Spike S2 experiments (task card R1.2). Run on a Linux machine or GitHub runner after
# `cargo build --release` in this directory. Prints one result row per experiment.
set -u

BIN="$(cd "$(dirname "$0")" && pwd)/target/release/landlock-spike"
BASE="${RUNNER_TEMP:-/tmp}/oc-s2"
WT="$BASE/repo"
PRIV_TMP="$BASE/tmp"
SECRET="$HOME/oc-secret.txt"
rm -rf "$BASE" && mkdir -p "$WT" "$PRIV_TMP"
echo "top secret" > "$SECRET"
git -C "$WT" init -q

# Read-only: system, toolchains. Read-write: the worktree, a private temp dir, /dev/null.
RO=(/usr /bin /sbin /lib /lib64 /etc /proc /sys /dev /opt /usr/local "$HOME/.rustup" "$HOME/.cargo")
RW=("$WT" "$PRIV_TMP" /dev/null)
args=()
for d in "${RO[@]}"; do [ -e "$d" ] && args+=(--ro "$d"); done
for d in "${RW[@]}"; do args+=(--rw "$d"); done

ROWS=()
row() { ROWS+=("| $1 | $2 | $3 | $4 |"); printf '%-55s expected=%-4s actual=%-4s %s\n' "$1" "$2" "$3" "$4"; }

# expect NAME ok|fail CMD...  — runs CMD confined
expect() {
  local name=$1 want=$2; shift 2
  local out code got
  out=$("$BIN" run "${args[@]}" -- "$@" 2>&1); code=$?
  if [ $code -eq 0 ]; then got=ok; else got=fail; fi
  local verdict=PASS; [ "$got" = "$want" ] || verdict=UNEXPECTED
  row "$name" "$want" "$got" "$verdict: $(echo "$out" | grep -v '^landlock-spike:' | tail -1 | cut -c1-90 | tr '|' '/')"
}
# observe NAME CMD... — runs CMD unconfined, records the outcome only
observe() {
  local name=$1; shift
  local out code
  out=$("$@" 2>&1); code=$?
  row "$name" "-" "exit $code" "$(echo "$out" | tail -1 | cut -c1-90 | tr '|' '/')"
}

echo "== environment"
uname -a
grep PRETTY_NAME /etc/os-release
"$BIN" abi
"$BIN" run "${args[@]}" -- true 2>&1 | grep landlock-spike
observe "sysctl apparmor_restrict_unprivileged_userns" sysctl -n kernel.apparmor_restrict_unprivileged_userns
observe "sysctl unprivileged_userns_clone" sysctl -n kernel.unprivileged_userns_clone

echo "== filesystem policy"
expect "read secret in \$HOME" fail cat "$SECRET"
expect "write outside (\$HOME)" fail touch "$HOME/oc-escape"
expect "write shared /tmp" fail touch /tmp/oc-s2-escape
expect "write inside worktree" ok touch "$WT/ok"
expect "write private temp dir" ok touch "$PRIV_TMP/ok"
expect "write /dev/null" ok sh -c 'echo hi > /dev/null'
expect "list another repo (\$HOME)" fail ls "$HOME"
expect "read system file" ok cat /etc/os-release
expect "git status in worktree" ok git -C "$WT" status --short

echo "== seccomp and namespaces"
observe "unconfined: unshare --user --map-root-user" unshare --user --map-root-user true
expect "confined: unshare --user (seccomp)" fail unshare --user true
out=$("$BIN" run "${args[@]}" --no-seccomp -- unshare --user true 2>&1); code=$?
row "confined, no seccomp: unshare --user" "-" "exit $code" "$(echo "$out" | tail -1 | cut -c1-90)"
expect "confined: mount tmpfs" fail mount -t tmpfs none "$WT"

echo "== network"
expect "https to api.anthropic.com" ok curl -sS -o /dev/null --max-time 15 https://api.anthropic.com
expect "https to registry.npmjs.org" ok curl -sS -o /dev/null --max-time 15 https://registry.npmjs.org

echo "== verification commands"
mkdir -p "$WT/rs/src" && printf '[package]\nname="t"\nversion="0.1.0"\nedition="2021"\n' > "$WT/rs/Cargo.toml"
printf '#[test]\nfn ok() {}\n' > "$WT/rs/src/lib.rs"
expect "cargo test, default CARGO_HOME" ok cargo test -q --manifest-path "$WT/rs/Cargo.toml"
expect "cargo test, CARGO_HOME in temp" ok env CARGO_HOME="$PRIV_TMP/cargo" cargo test -q --manifest-path "$WT/rs/Cargo.toml"
mkdir -p "$WT/js" && printf '{"name":"t","version":"1.0.0","scripts":{"test":"node -e 0"}}\n' > "$WT/js/package.json"
expect "npm test, default cache" ok npm --prefix "$WT/js" test --silent
expect "npm test, cache in temp" ok env npm_config_cache="$PRIV_TMP/npm" npm --prefix "$WT/js" test --silent
mkdir -p "$WT/go" && printf 'module t\n\ngo 1.21\n' > "$WT/go/go.mod" && printf 'package t\nimport "testing"\nfunc TestOK(t *testing.T) {}\n' > "$WT/go/t_test.go"
expect "go test, default caches" ok sh -c "cd '$WT/go' && go test ./..."
expect "go test, caches in temp" ok sh -c "cd '$WT/go' && GOCACHE='$PRIV_TMP/gocache' GOPATH='$PRIV_TMP/gopath' go test ./..."
mkdir -p "$WT/py" && printf 'import unittest\nclass T(unittest.TestCase):\n    def test_ok(self): pass\n' > "$WT/py/test_t.py"
expect "python unittest" ok sh -c "cd '$WT/py' && python3 -m unittest -q"

echo "== agent CLIs (no login on CI)"
if command -v claude >/dev/null; then
  expect "claude --version" ok claude --version
else row "claude --version" "-" "not tested" "claude not installed"; fi
if command -v codex >/dev/null; then
  expect "codex --version" ok codex --version
else row "codex --version" "-" "not tested" "codex not installed"; fi

echo "== keep-awake (S5)"
observe "systemd-inhibit idle:sleep" systemd-inhibit --what=idle:sleep --who=openchef --why=spike sleep 1

{
  echo "## S2 results — $(grep PRETTY_NAME /etc/os-release | cut -d= -f2) $(uname -r) $(uname -m)"
  echo
  echo "| Experiment | Expected | Actual | Note |"
  echo "|---|---|---|---|"
  printf '%s\n' "${ROWS[@]}"
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}" > "$BASE/results.md"
cat "$BASE/results.md"
