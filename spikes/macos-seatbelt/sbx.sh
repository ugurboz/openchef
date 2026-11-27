#!/bin/bash
# sbx.sh STATE1 STATE2 STATE3 CMD... — runs CMD in /private/tmp/oc-s1/repo under policy.sb.
HERE="$(cd "$(dirname "$0")" && pwd)"
s1=$1 s2=$2 s3=$3; shift 3
cd /private/tmp/oc-s1/repo || exit 1
exec sandbox-exec -f "$HERE/policy.sb" -D WORKTREE=/private/tmp/oc-s1/repo -D TMP=/private/tmp/oc-s1/tmp \
  -D HOME="$HOME" -D STATE1="$s1" -D STATE2="$s2" -D STATE3="$s3" env TMPDIR=/private/tmp/oc-s1/tmp/ "$@"
