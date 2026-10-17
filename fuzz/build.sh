#!/usr/bin/env bash
set -euo pipefail
# Match cargo-fuzz 0.13.2's default ASan/coverage build, while requiring a locked
# offline Cargo resolution. The target is intentionally explicit for host tools.
cd "$(dirname "$0")/.."
fuzz_triple=$(rustc +nightly -vV | sed -n 's/^host: //p')
export RUSTFLAGS='-Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 -Cllvm-args=-sanitizer-coverage-inline-8bit-counters -Cllvm-args=-sanitizer-coverage-pc-table -Cllvm-args=-sanitizer-coverage-trace-compares --cfg fuzzing -Cllvm-args=-simplifycfg-branch-fold-threshold=0 -Zsanitizer=address -Cllvm-args=-sanitizer-coverage-stack-depth -Cdebug-assertions -Ccodegen-units=1'
cargo +nightly build --locked --offline --manifest-path fuzz/Cargo.toml \
    --target "$fuzz_triple" --release --config 'profile.release.debug="line-tables-only"' --bins
