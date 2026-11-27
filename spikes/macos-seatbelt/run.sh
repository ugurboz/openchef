#!/bin/bash
# S1 experiments (task card R1.1). Usage: spikes/macos-seatbelt/run.sh [basic|verify|cli]
# Creates /private/tmp/oc-s1 and ~/oc-secret.txt (a fake secret), never touches real
# CLI configuration except through the CLIs themselves.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
B=/private/tmp/oc-s1
WT=$B/repo
TMPD=$B/tmp
mkdir -p "$WT" "$TMPD" && [ -d "$WT/.git" ] || git -C "$WT" init -q
[ -f "$HOME/oc-secret.txt" ] || echo "top secret" > "$HOME/oc-secret.txt"
NONE=/nonexistent

# sb STATE1 STATE2 STATE3 -- CMD...
sb() {
  local s1=$1 s2=$2 s3=$3; shift 4
  sandbox-exec -f "$HERE/policy.sb" -D WORKTREE="$WT" -D TMP="$TMPD" -D HOME="$HOME" \
    -D STATE1="$s1" -D STATE2="$s2" -D STATE3="$s3" env TMPDIR="$TMPD/" "$@"
}

row() { printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$(echo "$4" | tr '\n|' ' /' | cut -c1-110)"; }
expect() {
  local name=$1 want=$2; shift 2
  local out code got
  out=$(sb $NONE $NONE $NONE -- "$@" 2>&1); code=$?
  [ $code -eq 0 ] && got=ok || got=fail
  row "$name" "$want" "$got $([ "$got" = "$want" ] && echo PASS || echo UNEXPECTED)" "$(echo "$out" | tail -2)"
}

basic() {
  echo "| Experiment | Expected | Actual | Output |"; echo "|---|---|---|---|"
  expect "read secret in \$HOME" fail cat "$HOME/oc-secret.txt"
  expect "list \$HOME/Documents" fail ls "$HOME/Documents"
  expect "read ~/.ssh" fail ls "$HOME/.ssh"
  expect "write \$HOME/oc-escape" fail touch "$HOME/oc-escape"
  expect "write /private/tmp (shared)" fail touch /private/tmp/oc-s1-escape
  expect "write /usr/local" fail touch /usr/local/oc-escape
  expect "write worktree" ok touch "$WT/ok"
  expect "write private TMPDIR" ok touch "$TMPD/ok"
  expect "write /dev/null" ok sh -c 'echo x > /dev/null'
  expect "read system file" ok cat /etc/hosts
  expect "git status in worktree" ok git -C "$WT" status --short
  expect "https api.anthropic.com" ok curl -sS -o /dev/null --max-time 15 https://api.anthropic.com
  expect "inline profile (-p) works" ok sandbox-exec -p '(version 1)(allow default)' true
}

verify() {
  echo "| Experiment | Expected | Actual | Output |"; echo "|---|---|---|---|"
  mkdir -p "$WT/rs/src" && printf '[package]\nname="t"\nversion="0.1.0"\nedition="2021"\n[workspace]\n' > "$WT/rs/Cargo.toml" && printf '#[test]\nfn ok() {}\n' > "$WT/rs/src/lib.rs"
  expect "cargo test (TMPDIR private)" ok cargo test -q --manifest-path "$WT/rs/Cargo.toml"
  expect "cargo test, CARGO_HOME private" ok env CARGO_HOME="$TMPD/cargo" RUSTUP_HOME="$HOME/.rustup" cargo test -q --manifest-path "$WT/rs/Cargo.toml"
  mkdir -p "$WT/js" && printf '{"name":"t","version":"1.0.0","scripts":{"test":"node -e 0"}}\n' > "$WT/js/package.json"
  expect "npm test, default cache" ok npm --prefix "$WT/js" test --silent
  expect "npm test, cache private" ok env npm_config_cache="$TMPD/npm" npm --prefix "$WT/js" test --silent
  mkdir -p "$WT/go" && printf 'module t\n\ngo 1.21\n' > "$WT/go/go.mod" && printf 'package t\nimport "testing"\nfunc TestOK(t *testing.T) {}\n' > "$WT/go/t_test.go"
  expect "go test, default caches" ok sh -c "cd '$WT/go' && go test ./..."
  expect "go test, caches private" ok sh -c "cd '$WT/go' && GOCACHE='$TMPD/gocache' GOPATH='$TMPD/gopath' GOTOOLCHAIN=local go test ./..."
  mkdir -p "$WT/py" && printf 'import unittest\nclass T(unittest.TestCase):\n    def test_ok(self): pass\n' > "$WT/py/test_t.py"
  expect "python unittest" ok sh -c "cd '$WT/py' && python3 -m unittest -q"
}

"${1:-basic}"
