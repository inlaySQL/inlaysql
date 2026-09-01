#!/usr/bin/env bash
#
# Interleaved A/B of one `bin/profile` suite across two builds.
#
#   ./bench/profile_ab.sh joins-limit HEAD~1
#   REPS=5 SECONDS=30 ./bench/profile_ab.sh indexed-range 2eeced7
#   ./bench/profile_ab.sh points HEAD~1 -- --rows 50000
#
# Builds `bin/profile` twice — once from the working tree, once from `<ref>` —
# then runs them alternately, one repetition each, and prints both medians with
# their ranges.
#
# # Why this exists
#
# `bench/repeat.sh` has wrapped `run.sh` for a long time, so every SQLite-facing
# figure carries a median and a spread. `bin/profile` had no equivalent, and
# every perf measurement in Track B was interleaved by hand as a result — which
# went wrong the first time it mattered. The `LIMIT`-join cache was measured at
# 1.31x by comparing a "before" run to an "after" run taken about thirty minutes
# apart; re-measured interleaved, the *same before binary* moved from 68k to
# 85-89k ops/s. The machine had moved, not the code. The real figure was 1.42x,
# and the direction of the error was flattering, which is the dangerous kind.
#
# Interleaving does not make a loaded machine quiet. It makes the drift land on
# both sides equally, which is what lets a ratio survive a machine that will not
# hold still — and this repository's machine does not hold still.
#
# The comparison stays honest in two more ways worth stating:
#
# * The control side is re-measured in every repetition, not once at the start.
#   A single control reading is what produced the 1.31x above.
# * Both binaries are built before either is run, so neither pays a cold
#   filesystem cache or a compile that the other does not.

set -euo pipefail

REPS=${REPS:-3}
SECONDS_PER_RUN=${SECONDS:-25}
ROWS=${ROWS:-20000}

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <suite> <baseline-git-ref> [-- extra profile args]" >&2
  echo "  e.g. $0 joins-limit HEAD~1" >&2
  exit 2
fi

SUITE="$1"
BASE_REF="$2"
shift 2
if [[ "${1:-}" == "--" ]]; then
  shift
fi
EXTRA=("$@")

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# `inlaysql-bench` links a bundled SQLite, so its build needs a C compiler that
# can find the system headers. The main checkout usually inherits that from
# whatever shell it is built in; a fresh worktree does not, and the failure is
# a wall of `call to undeclared function 'time'` from sqlite3.c rather than
# anything naming an SDK. Set it here when the platform has one to give.
if [[ -z "${SDKROOT:-}" ]] && command -v xcrun >/dev/null 2>&1; then
    SDKROOT="$(xcrun --show-sdk-path 2>/dev/null || true)"
    export SDKROOT
fi

WORKTREE="$(mktemp -d "${TMPDIR:-/tmp}/inlaysql-ab.XXXXXX")"
cleanup() {
  git worktree remove --force "$WORKTREE" >/dev/null 2>&1 || true
  rm -rf "$WORKTREE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> building the working tree's profile binary"
cargo build --release -q -p inlaysql-bench --bin profile
AFTER="$(mktemp "${TMPDIR:-/tmp}/profile-after.XXXXXX")"
# `cp` onto an existing file keeps that file's mode, and `mktemp` makes one
# without an execute bit, so the copy has to be marked executable explicitly —
# otherwise the first run fails with a bare 126 and no explanation.
cp target/release/profile "$AFTER"
chmod +x "$AFTER"

# A worktree rather than a stash: stashing loses to a tree that is already
# clean because the change under test has been committed, which is exactly the
# case after every commit and is how a "before" binary silently becomes a
# second copy of "after".
echo "==> building ${BASE_REF}'s profile binary"
git worktree add --detach "$WORKTREE" "$BASE_REF" >/dev/null
(cd "$WORKTREE" && cargo build --release -q -p inlaysql-bench --bin profile)
BEFORE="$(mktemp "${TMPDIR:-/tmp}/profile-before.XXXXXX")"
cp "$WORKTREE/target/release/profile" "$BEFORE"
chmod +x "$BEFORE"

if cmp -s "$BEFORE" "$AFTER"; then
  echo "!! the two binaries are byte-identical: ${BASE_REF} and the working tree" >&2
  echo "!! build the same thing, so any difference below is pure noise." >&2
fi

echo "==> $SUITE, $REPS repetitions of ${SECONDS_PER_RUN}s each, interleaved"
echo "    load at start: $(uptime | sed 's/.*load averages*: //')"

before_results=()
after_results=()
for rep in $(seq 1 "$REPS"); do
  for side in before after; do
    binary="$BEFORE"
    [[ "$side" == "after" ]] && binary="$AFTER"
    line="$("$binary" --suite "$SUITE" --rows "$ROWS" --seconds "$SECONDS_PER_RUN" \
      ${EXTRA[@]+"${EXTRA[@]}"} 2>/dev/null | tail -1)"
    ops="$(printf '%s' "$line" | sed -n 's/.*(\([0-9.]*\) ops\/s).*/\1/p')"
    if [[ -z "$ops" ]]; then
      echo "  rep$rep $side: could not read a figure from: $line" >&2
      exit 1
    fi
    printf '  rep%-2s %-7s %s ops/s\n' "$rep" "$side" "$ops"
    if [[ "$side" == "before" ]]; then
      before_results+=("$ops")
    else
      after_results+=("$ops")
    fi
  done
done

summarise() {
  printf '%s\n' "$@" | sort -n | awk -v label="$1" '
    { values[NR] = $1 }
    END {
      middle = (NR % 2) ? values[(NR + 1) / 2] : (values[NR / 2] + values[NR / 2 + 1]) / 2
      printf "%.0f %.0f %.0f", middle, values[1], values[NR]
    }'
}

read -r b_med b_min b_max <<<"$(summarise "${before_results[@]}")"
read -r a_med a_min a_max <<<"$(summarise "${after_results[@]}")"

echo
printf 'before  median %s  (%s - %s)\n' "$b_med" "$b_min" "$b_max"
printf 'after   median %s  (%s - %s)\n' "$a_med" "$a_min" "$a_max"
awk -v b="$b_med" -v a="$a_med" -v bmax="$b_max" -v amin="$a_min" -v bmin="$b_min" -v amax="$a_max" '
  BEGIN {
    printf "ratio   %.3fx\n", a / b
    # Overlapping ranges mean the two sides are not distinguishable at this
    # repetition count, whatever the medians say. Said plainly, because a
    # ratio quoted from overlapping ranges is the mistake this script exists
    # to stop.
    if (amin <= bmax && bmin <= amax) {
      print "        RANGES OVERLAP — not a result at this repetition count."
      print "        Raise REPS, quiet the machine, or accept there is no difference."
    } else {
      print "        ranges do not overlap"
    }
  }'
