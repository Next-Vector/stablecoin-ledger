#!/bin/bash
# Tests for the git hooks in .githooks/ and their auto-enabling.
# Runs every case in a throwaway clone of the working tree; exits non-zero on the first failure.
# Usage: ./scripts/test-hooks.sh
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
failures=0

pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; failures=$((failures + 1)); }

# A clone with the working tree's hooks and build script copied in, so uncommitted changes are tested.
fresh_clone() {
  local dir=$1
  git clone -q "$REPO" "$dir"
  rm -rf "$dir/.githooks"
  cp -r "$REPO/.githooks" "$dir/.githooks"
  cp "$REPO/crates/ledger-core/build.rs" "$dir/crates/ledger-core/build.rs" 2>/dev/null || true
  git -C "$dir" config user.email test@example.invalid
  git -C "$dir" config user.name test
}

expect_msg() {
  local verdict=$1 message=$2
  printf '%s\n' "$message" > "$WORK/msg"
  if "$REPO/.githooks/commit-msg" "$WORK/msg" >/dev/null 2>&1; then actual=accept; else actual=reject; fi
  if [ "$actual" = "$verdict" ]; then pass "commit-msg $verdict: $(head -1 "$WORK/msg")"; else fail "commit-msg should $verdict: $(head -1 "$WORK/msg")"; fi
}

# --- commit-msg -------------------------------------------------------------
expect_msg accept "feat(ledger-core): add journal (#8)"
expect_msg accept "ci: run checks on ubuntu-latest (#2)"
expect_msg accept $'fix(api): handle empty body (#20)\n\nThe parser returned 500 on an empty request body.'
expect_msg accept $'docs: add task list (#5)\n# Please enter the commit message for your changes.'
expect_msg accept "Merge branch 'dev' into m1-ledger-core"
expect_msg accept "Revert \"feat(ledger-core): add journal (#8)\""
expect_msg accept "fixup! feat(ledger-core): add journal (#8)"
expect_msg reject "add journal (#8)"
expect_msg reject "feat(ledger-core): add journal"
expect_msg reject "feature: add journal (#8)"
expect_msg reject "feat(Ledger Core): add journal (#8)"
expect_msg reject "feat(ledger-core):add journal (#8)"
expect_msg reject $'feat(ledger-core): add journal (#8)\n\nCo-Authored-By: Someone <someone@example.com>'
expect_msg reject $'feat(ledger-core): add journal (#8)\n\nFirst paragraph.\n\nSecond paragraph.'
expect_msg reject "feat(ledger-core): $(printf 'x%.0s' $(seq 1 100)) (#8)"

# --- pre-push ---------------------------------------------------------------
clone="$WORK/push"
fresh_clone "$clone"
git init -q --bare "$WORK/remote.git"
git -C "$clone" remote set-url origin "$WORK/remote.git"
git -C "$clone" config core.hooksPath .githooks

if git -C "$clone" push -q origin HEAD:refs/heads/main >/dev/null 2>&1; then
  fail "pre-push should reject a push to main"
else
  pass "pre-push rejects a push to main"
fi

if git -C "$clone" push -q origin HEAD:refs/heads/feature >/dev/null 2>&1; then
  pass "pre-push accepts a feature branch after audit and deny"
else
  fail "pre-push should accept a feature branch"
fi

# --- auto-enable ------------------------------------------------------------
clone="$WORK/auto"
fresh_clone "$clone"
if [ -n "$(git -C "$clone" config --get core.hooksPath)" ]; then
  fail "fresh clone should start without core.hooksPath"
fi
(cd "$clone" && env -u CI CARGO_TARGET_DIR="$WORK/target" cargo build -q -p ledger-core >/dev/null 2>&1)
if [ "$(git -C "$clone" config --get core.hooksPath)" = ".githooks" ]; then
  pass "first build enables core.hooksPath=.githooks"
else
  fail "first build should enable core.hooksPath=.githooks"
fi

clone="$WORK/ci"
fresh_clone "$clone"
(cd "$clone" && CI=true CARGO_TARGET_DIR="$WORK/target-ci" cargo build -q -p ledger-core >/dev/null 2>&1)
if [ -z "$(git -C "$clone" config --get core.hooksPath)" ]; then
  pass "build in CI leaves core.hooksPath unset"
else
  fail "build in CI should not touch core.hooksPath"
fi

echo
if [ "$failures" -gt 0 ]; then echo "$failures hook test(s) failed"; exit 1; fi
echo "all hook tests passed"
