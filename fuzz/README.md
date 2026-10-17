# Fuzzing

The frontend target exercises recovering lexing, parsing and AST destruction.
The KDL target exercises the production byte/depth preflight, KDL v2 parser,
and package/dependency/build-target/plugin schemas through the pure config APIs. It
does not open dependency paths, source files, or create project artifacts.
The decoder target checks acceptance, encoding symmetry and canonical bytes.
The verifier target constructs in-memory instructions with arbitrary operands.
It also constructs bounded typed-call, native-result, closure/capture, global,
branch and Error-propagation cases, with optional structural mutations. The
shared generator has deterministic integration tests; these validate artifacts
without running them. Structured seeds are in `fuzz/seeds/verifier/`.
None executes arbitrary Cog programs or native imports.

Install cargo-fuzz and a nightly toolchain, then run from the project directory:

```sh
cargo +nightly fuzz run frontend -- -max_total_time=60 -max_len=65536 -seed=1
cargo +nightly fuzz run kdl -- -max_total_time=60 -max_len=262145 -rss_limit_mb=1024 -timeout=5 -seed=1
cargo +nightly fuzz run decoder -- -max_total_time=60 -max_len=65536 -seed=1
cargo +nightly fuzz run verifier -- -max_total_time=60 -max_len=36864 -seed=1
```

Use address sanitizer (cargo-fuzz's default), isolated workers, bounded memory,
and a parent deadline. Retain generated corpus and crash artifacts under fuzz/.
Seed the decoder with real encoded programs, including functions and closures;
seed the frontend with examples and adversarial regression cases. Before a release
candidate, record at least 24 CPU-hours per target, including toolchain, seed,
limits, duration, corpus, coverage and findings in the internal audit. Smoke runs
and builds do not satisfy that gate or establish absence of vulnerabilities.

Copy `fuzz/seeds/kdl/*.kdl` into `fuzz/corpus/kdl/` before a KDL campaign; preserve
existing corpus files. Copy `fuzz/seeds/verifier/*` into `fuzz/corpus/verifier/`
as well. The KDL maximum input length includes one byte beyond the
256 KiB configuration budget, exercising rejection at that boundary. Fuzz CI is
opt-in: pushes and pull requests do not run it. A manual workflow dispatch must
explicitly enable fuzz smoke and select the allowed number of simultaneous
single-worker targets. CI retains corpora, crash artifacts and logs and applies
a parent timeout to each target.
Record sanitizer configuration explicitly; disabling LeakSanitizer under a ptrace
restriction does not count as leak-check evidence. Measure CPU time separately
from wall time for the 24 CPU-hour gate.

The 2026-10-17 KDL campaign reached libFuzzer's 1,024 MiB RSS stop after 710.8
child CPU seconds. The saved 134-byte OOM input replayed normally in isolation;
the log shows cumulative libFuzzer/allocator RSS, not a single-input sanitizer
report. A 30-second smoke with
`ASAN_OPTIONS=detect_leaks=0:symbolize=1:quarantine_size_mb=64:thread_local_quarantine_size_kb=1024`
completed at 538 MiB peak RSS. This lowers ASan's freed-object quarantine and
therefore narrows delayed use-after-free detection. Any restarted campaign must
record this tradeoff and its exact environment; it is not leak-check evidence.
Campaign 02 instead retains the default ASan quarantine and restarts its child
every 60 seconds; a 60-second smoke peaked at 795 MiB. This bounds process
lifetimes but still requires monitoring each full campaign chunk.

`bash fuzz/build.sh` builds all four targets with locked/offline Cargo and the
ASan/coverage settings from cargo-fuzz 0.13.2. It requires a cached Linux nightly
toolchain and dependencies. `campaign.py` runs a SHA-256-pinned binary in bounded
chunks until measured child user+system CPU reaches the requested
budget. Record compiler/Cargo/OS versions and exact build flags in a tool-evidence
file; pass its path with `--tool-evidence`. Use separate run/artifact directories
per target/campaign. Do not treat `cpu_budget_completed_pending_review` as a release
pass until all logs, artifacts and coverage limitations have been reviewed.
The runner never removes corpus/artifacts and stops on abnormal exits. A parent
timeout, sanitizer/RSS report or nonzero exit requires investigation before resuming.
The campaign runner intentionally refuses non-Linux hosts. It measures child
user+system CPU with `RUSAGE_CHILDREN`; reported RSS is the cumulative child
high-water mark in KiB for that campaign process, not an isolated per-chunk
measurement. Start one campaign runner per target so that the RSS scope remains
clear. The campaign record labels both accounting methods explicitly.
The runner records a SHA-256 manifest digest of each initial and post-chunk
corpus (filename, size and file digest), along with the base/chunk seeds, binary
digest, exact command, tool-evidence digest, per-chunk logs and coverage summary.
The checked-in/current seed corpus is passed directly to libFuzzer and may grow;
the runner does not prune or rewrite its existing files.
