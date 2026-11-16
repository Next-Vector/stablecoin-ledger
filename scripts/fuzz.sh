#!/bin/bash
# Fuzzes the ledger-script DSL with cargo-fuzz (needs the nightly toolchain and cargo-fuzz:
# `rustup toolchain install nightly --profile minimal && cargo install --locked cargo-fuzz`).
#
#   ./scripts/fuzz.sh [parse|compile] [seconds]     default: parse for 60 s
#
# The corpus grows under crates/ledger-script/fuzz/corpus/ (git-ignored); the golden test scripts
# are read as seeds. A crash is saved under fuzz/artifacts/<target>/; replay it with
# `cargo +nightly fuzz run <target> <file>` from crates/ledger-script, then add a golden test.
set -euo pipefail
target="${1:-parse}"
seconds="${2:-60}"
cd "$(dirname "$0")/../crates/ledger-script"
mkdir -p "fuzz/corpus/$target"
exec cargo +nightly fuzz run "$target" "fuzz/corpus/$target" \
  tests/golden/ok tests/golden/errors tests/golden/compile/ok tests/golden/compile/errors -- \
  -dict=fuzz/ledger-script.dict -max_len=4096 -max_total_time="$seconds"
