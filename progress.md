# Current Project Status

- Current milestone: Phase 10 — memory architecture, copy-back performance, bounded parallelism, GPU acceleration, resource budgeting, and robustness
- Current subphase: Phase 10B — partial-diff freshness VERIFIED (package level); fixed a pre-existing flaky/hang-prone watch test; verified work committed; next is snapshot isolation
- Current branch: `hust/nvidia-rtx3070`
- Latest verified commit: `bc9d31b` (Phase 10 commits `941d966`, `2fcebdf`, `bc9d31b`, plus the docs commit that adds this line)
- Current working-tree status: clean after the docs commit
- Active agent: Claude Code
- Model: Claude Opus 5.5
- Last checkpoint timestamp: 2026-11-23 (Claude recovery checkpoint)
- Current build status: PASS (`cargo test --workspace --no-run`)
- Current test status: `victory-gpu` 52/52 pass; `cargo xtask policy` 150 files ok; `cargo test --workspace` 239 passed / 0 failed / 3 ignored

## Active Task

- Objective: Prevent diff-only copy-back from marking a whole 64 KiB shadow page current when concurrent title-CPU writes in that page were preserved but not synchronized into the shadow.
- Current implementation: VERIFIED (package). Claude's recovery audit found that `copy_back_ranges` in the working tree NO LONGER calls `watch::writes`/`copied.fill(now)` (OpenCode made the edit but recorded the task as NOT_STARTED). The missing piece is the full-page MEM_WRITE_WATCH regression test.
- Files involved: `crates/victory-gpu/src/shadow.rs` (copy-back bookkeeping, tests); `crates/victory-gpu/src/watch.rs` read-only reference.
- Expected behavior: GPU-changed bytes and concurrent CPU-changed bytes in one watched page both reach title memory; a later `update` brings the preserved CPU bytes into the shadow and the shadow stays in watched mode.
- Known risks: a later redundant page refresh (copy-back's own WriteProcessMemory dirties the watch bit); correctness first, no performance claim.

## Immediate Resume Instructions

- First action: start Priority B snapshot isolation (see next step).
- Next implementation step: Priority B snapshot isolation. Plan: make `Shadow::snapshot_ranges` return the snapshot `(base_off, Vec<u8>)` instead of storing it in the shared `Shadow.before` slot, take snapshots in `Host::execute` (host.rs ~639) AFTER acquiring `self.submitted` (host.rs ~642) so each submission's baseline is its own and taken right before its GPU work, pass it into `copy_back_ranges`, remove the `before` field. Then checked fence waits: check `WaitForSingleObject` return (host.rs ~654) and `Signal`/`SetEventOnCompletion` errors; on an unconfirmed wait verify `fence.GetCompletedValue() >= submitted` before reading shadows for copy-back.
- Pending tests: focused shadow tests, full `victory-gpu`, modified-file rustfmt, workspace tests/clippy, diagnostics clippy/build, final Phase 10 matrix.
- Known blockers: none; commits require explicit per-commit user confirmation. No hardware logs available.
- Commands required:
  - `cargo xtask policy`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets`
  - `cargo clippy -p victory-gpu -p victory --features diagnostics --all-targets`
  - `cargo build --profile diag -p victory --features diagnostics`

## Completed Tasks

- VERIFIED — Branch and handoff commit recovery (2026-11-23): current branch is `hust/nvidia-rtx3070`; `HEAD` is `7c261c3ce58c135498bd32f7d4ffc5d7a2790ae4`; `main` is at the stated baseline `015d45b`.
- VERIFIED — Working-tree recovery before documentation bootstrap: `git status --porcelain=v2 --branch`, staged/unstaged diff stats, `git diff --stat 7c261c3..HEAD`, and untracked-file enumeration showed no modifications, staged changes, untracked files, or commits after `7c261c3`.
- VERIFIED — Local stopping-point assessment: the reflog ends at the Phase 9 commit `7c261c3`; no local Phase 10 commit, source modification, staged change, or standard untracked artifact was found. Therefore the only evidence-backed resume point is the clean Phase 9 handoff. This does not prove that Antigravity did no work outside this repository.
- VERIFIED — Engineering instructions reviewed: no pre-existing `AGENTS.md`, `CLAUDE.md`, or `progress.md` was found in the workspace. `README.md`, `docs/architecture.md`, and `docs/asset-policy.md` were read completely. Root `progress.md` is now the canonical progress location; no external progress history was available to migrate.
- VERIFIED — Permanent protocol established: root `AGENTS.md` now mandates recovery, independent development, continuous checkpoints, accurate test evidence, focused verified commits with confirmation, non-destructive operation, authorized-use controls, and actionable handoffs.
- VERIFIED — Baseline asset policy (2026-11-23): `cargo xtask policy` exited successfully and reported `asset policy: 149 files ok`.
- VERIFIED — Baseline workspace tests (2026-11-23): `cargo test --workspace` exited successfully: 236 tests passed, 0 failed, and 3 ignored. The ignored tests require external BCPack reference chunks, an XMA reference decoder, and saved XMA streams. All doc-test targets ran with zero tests. Rust emitted repeated incremental-cache hard-link warnings on this G: filesystem and copied files instead; these warnings did not fail the build or tests.
- VERIFIED — Diagnostic-log search (2026-11-23): no `*.log` files were found anywhere in the workspace, and the initial tracked text search found none of the supplied RAM/commit measurements. Runtime logs may exist in the external per-user data directory, which was not accessed. The historical measurements therefore remain unverified and are not an accepted leak baseline.
- VERIFIED — Existing diagnostics inventory: `crates/victory-gpu/src/copystat.rs` already records per-frame copy/refresh/skip bytes, run/query/move/watch timing, page-fault deltas, watched/unchanged bytes, guard fallbacks/faults, texture hashes, and the top copied source ranges. It currently reports page-fault counts but not process private memory, system commit, or DXGI budgets.
- VERIFIED — Baseline workspace clippy (2026-11-23): `cargo clippy --workspace --all-targets` exited successfully. One existing `clippy::undocumented_unsafe_blocks` warning remains at `crates/victory-gpu/src/list.rs:1398`; incremental-cache hard-link fallback warnings were also non-fatal. The unrelated warning has not been changed.
- VERIFIED — Modified-file formatting: `rustfmt --edition 2024 --check crates/victory-gpu/src/pipeline.rs` exited successfully with no diff.
- FAILED — Initial focused shader-cache test run (2026-11-23): `cargo test -p victory-gpu pipeline::tests` compiled successfully and ran 3 tests: `pipelines_keep_their_shaders` and `streams_yield_their_shaders` passed; `cache_deduplicates_live_shaders_without_keeping_them_alive` failed its final count assertion (expected 1, actual 2). The earlier sharing, final-owner-release, and original-key-pruning assertions passed. Root cause: the test overlooked the initial insertion in the 256-miss prune cadence, so two legitimate post-prune entries remained.
- VERIFIED — Shader-cache test fix and focused rerun (2026-11-23): the new test now resets `misses_since_prune` to establish a known pruning boundary without changing implementation logic. `cargo test -p victory-gpu pipeline::tests` then passed all 3 tests with 0 failures; `rustfmt --edition 2024 --check crates/victory-gpu/src/pipeline.rs` again passed. This verifies synthetic cache lifetime/dedup/pruning behavior only, not hardware memory savings.
- VERIFIED — Shader-cache package verification (2026-11-23): `cargo test -p victory-gpu` passed all 50 tests with 0 failures and 0 ignored; `cargo clippy -p victory-gpu --all-targets` exited successfully with only the pre-existing undocumented-unsafe warning at `list.rs:1398`. `git diff --check` also passed.
- VERIFIED — Duplicate-pending focused test (2026-11-23): `cargo test -p victory-gpu shadow::tests` passed all 4 tests, including the new duplicate GPU-write/pending-release/later-refresh regression. `rustfmt --edition 2024 --check crates/victory-gpu/src/shadow.rs` passed.
- VERIFIED — Duplicate-pending full package verification (2026-11-23): `cargo test -p victory-gpu` passed all 51 tests with 0 failures and 0 ignored after the fix.
- VERIFIED — Local Phase 9 copy-back audit (2026-11-23; read-only subagent report incorporated into this canonical log):
  - `1d945cf` caches genuine unmapped guard-view holes and fixes copy-stat frame normalization. `f91a837` adds exact-unique submission ranges, pre-GPU snapshots, diff-only copy-back, deferred COM lifetimes, and aligned-shadow pruning. `7c261c3` is nonfunctional cleanup.
  - Shadow read-back uses a persistently mapped custom write-back/L0 D3D12 buffer read by the CPU after a fence, not a READBACK heap plus `CopyBufferRegion`. Pre-GPU snapshots are CPU `memcpy` operations and are not included in the aggregate `COPY_BACK` timing.
  - Copy-back currently combines range allocation/merging, exact byte comparison, `WriteProcessMemory`, page opening/protection, post-copy watch queries, and snapshot-pool work. Existing counters do not separate these costs; `Skipped`, `SkippedBytes`, `Queries`, and `QueryUs` have no increment sites. No measurements justify choosing a dominant bottleneck yet.
  - Memory findings: `LOOSE` shadows intentionally never free; loose-derived `ALIGNED` shadows have no unregister path; the snapshot pool keeps eight largest buffers by count but has no byte budget; each shadow snapshot covers the min/max envelope, including gaps between sparse ranges. These are retention/amplification risks, not proof of a leak.
  - CONFIRMED SOURCE BUG 1: duplicate GPU-write declarations append duplicate pending ranges but host copy-back is exact-deduplicated and removes only one, allowing stale refresh blockers and pending-vector growth. This is the active fix.
  - CONFIRMED SOURCE BUG 2: after diff-only writes, `copy_back_ranges` marks fully contained pages fresh with `copied.fill(now)` even though preserved concurrent CPU bytes were not copied into the shadow. A later `copy_stale` can reject those title writes. Needs a full-page write-watch regression and conservative bookkeeping.
  - SOURCE RACE RISK 3: one `Shadow.before` snapshot slot is replaced before `Host::execute` acquires `submitted`; overlapping submissions can consume/replace each other's baselines or snapshot during other GPU work. Not yet reproduced or fixed.
  - ROBUSTNESS RISK 4: `Signal`/event errors only warn and `WaitForSingleObject` is unchecked; copy-back and bottom-of-pipe completion still publish after an unconfirmed wait. Not yet reproduced or fixed.
  - Existing diff tests use ordinary `Vec` memory and do not cover full-page write-watch bookkeeping, host pending multiplicity, snapshot isolation/pool byte bounds, or failed fence waits. Correctness fixes should precede parallelism or GPU-side dirty masks.

- VERIFIED — Partial-diff page freshness (Claude, 2026-11-23): added `shadow::tests::copy_back_does_not_mark_watched_pages_fresh` (VirtualAlloc MEM_WRITE_WATCH page, GPU write [64,72) + concurrent title write [200,208), copy-back, then `update` must copy the page and keep watched mode). Mutation check: temporarily restoring the old `copied.fill(now)` block made the test FAIL (`assertion failed: shadow.update(base, PAGE) > 0`); restored fix passes. `cargo test -p victory-gpu shadow::tests` 5/5; `cargo test -p victory-gpu` 52/52; rustfmt check on shadow.rs/pipeline.rs OK; `git diff --check` OK; `cargo clippy -p victory-gpu --all-targets` only the pre-existing `list.rs:1398` warning.
- FAILED → FIXED — First Claude `cargo test --workspace` run (2026-11-23) hung >10 min inside the workspace-feature `victory_gpu-*.exe` test binary and was stopped. Repeated parallel runs of that binary then showed `watch::tests::unmapped_holes_in_guarded_views_are_cached_until_remapped` (Phase 9 test, commit `1d945cf`) failing ~1 in 8–17 runs: `left: [b, b+1] right: [b]`. Root cause: the test assumed the 64 KiB block after its view stays unmapped; a concurrent test allocation could land there and `guard()` would then make that other allocation READ-ONLY (a plausible cause of the hang). Test-only fix: find a free 2-block range (reserve+release), `MapViewOfFileEx` the view at its start, claim the hole with a `MEM_RESERVE` reservation (VirtualProtect always fails on uncommitted reserve), retry if anything races in; also closes the section handle. A first attempt (reserve the block after an ordinary MapViewOfFile) failed 10/60 because views pack against existing allocations. Final version: 0 failures in 80 parallel runs. No production code changed.
- VERIFIED — `cargo xtask policy` (Claude, 2026-11-23): `asset policy: 150 files ok`.
- VERIFIED — Workspace rerun after the watch-test fix (Claude, 2026-11-23): `cargo test --workspace` exit 0, 239 passed, 0 failed, 3 ignored (baseline 236 + 3 new Phase 10 tests). `cargo clippy --workspace --all-targets` exit 0; only the pre-existing `list.rs:1398` undocumented-unsafe warning (plus a victory-shader "1 warning" line that does not reproduce on `cargo clippy -p victory-shader`, likely incremental-cache noise; crate unmodified). rustfmt check on modified files and `git diff --check` passed.

## Incomplete Tasks

- NOT_STARTED — Priority B snapshot isolation: prevent overlapping submissions from replacing/consuming each other's pre-GPU baselines.
- NOT_STARTED — Remaining Priority A: shadow/staging lifetime audit, snapshot-pool byte budgets/accounting, additional bounded allocation, and allocation-failure handling.
- NOT_STARTED — Priority B measurement: separate scanning, snapshot memcpy, comparison, title writes, page protection, fence API/event waits, synchronization, and temporary allocation costs.
- NOT_STARTED — Priority C: evaluate justified bounded CPU parallelism with duplicate-job and memory-growth controls.
- NOT_STARTED — Priority D: evaluate documented D3D12 compute/copy-queue acceleration and GPU-local intermediates where measurements justify them.
- NOT_STARTED — Priority E: process/system/DXGI memory reporting, bounded caches, temporary reuse, and safe reclamation.
- NOT_STARTED — Priority F: evidence-based allocation/null/partial-initialization robustness audit.
- NOT_STARTED — Final Phase 10 verification matrix and manual authorized-workload validation instructions.

## Agent Handoff History

### 2026-11-23 — OpenCode / Kimi K3.1 Pro — recovery and protocol bootstrap

- Agent name: OpenCode.
- Model name: Kimi K3.1 Pro.
- Session start and latest checkpoint time: 2026-11-23T21:06:48-04:00 / 2026-11-23T21:41:00-04:00.
- Starting commit: `7c261c3ce58c135498bd32f7d4ffc5d7a2790ae4`.
- Ending or latest commit: `7c261c3ce58c135498bd32f7d4ffc5d7a2790ae4`.
- Modified files: Created `AGENTS.md` and `progress.md`; modified `pipeline.rs` and `shadow.rs`.
- Implemented changes: Established the canonical protocol; verified weak shader-cache ownership/pruning; implemented but not yet verified exact pending-range deduplication plus a duplicate-write/later-refresh test.
- Technical findings: HEAD initially contained only clean Phase 9 state. No Antigravity Phase 10 artifacts or in-repository hardware logs were found. The local audit confirmed duplicate-pending and partial-diff freshness bugs, plus snapshot-isolation and unchecked-fence-wait risks. Detailed file/flow findings are recorded under Completed Tasks.
- Tests executed: Baseline policy, workspace tests/clippy; focused pipeline tests; full `victory-gpu` tests; package clippy; modified-file rustfmt; `git diff --check`.
- Actual test results: Baseline policy passed 149 files; workspace tests passed 236/failed 0/ignored 3; baseline clippy exited 0 with one old warning. Initial focused pipeline run failed 1 of 3 due to test prune-boundary bookkeeping; after correcting only the test setup, focused tests passed 3/3 and full package tests passed 50/50. Package clippy exited 0 with the same old `list.rs:1398` warning. Formatting/diff checks passed.
- Uncommitted changes: `AGENTS.md`, `progress.md`, and verified `pipeline.rs`; no commit has been authorized or created.
- Remaining implementation tasks: Duplicate pending fix, partial-diff freshness, snapshot isolation, fence-wait robustness, snapshot/memory accounting, then measured optimization work.
- Known regressions: No new runtime regression observed. The initial new-test failure was fixed and rerun. Historical hardware performance claims remain unverified.
- Recommended next action: Implement and test exact pending-range deduplication in `Shadow::writing`, then address the full-page freshness bug before attempting parallel or GPU-side optimization.

### 2026-11-23 — Claude Code / Claude Opus 5.5 — recovery (third Phase 10 agent)

- Starting commit: `7c261c3`; HEAD unchanged, no stashes, nothing staged.
- Outcome so far: freshness fix VERIFIED with mutation-checked regression; pre-existing flaky watch test fixed; workspace 239/0/3. User authorized four commits: `941d966` (weak shader cache), `2fcebdf` (pending dedup + freshness), `bc9d31b` (watch test race), then docs (`AGENTS.md`, `CLAUDE.md`, `progress.md`).
- Recovery findings: Antigravity left no in-repository artifacts (consistent with OpenCode's finding). OpenCode's work is entirely uncommitted: `AGENTS.md`, `progress.md`, `pipeline.rs` weak-ref shader cache (+1 test, verified), `shadow.rs` duplicate-pending dedup (+1 test, verified) AND an unrecorded removal of the unsound `copied.fill(now)` post-copy-back freshness advancement (unverified, no regression test). progress.md previously listed that fix as NOT_STARTED; corrected to IMPLEMENTED_UNVERIFIED.
- Audit of the freshness edit: correct and conservative. With the old code, `watch::writes` inside copy-back consumed the kernel dirty bit for a page containing a preserved concurrent CPU write and stamped the page fresh, so a later `copy_stale` saw `write <= copied` and never pulled the CPU bytes into the shadow. After removal, the next `update` re-queries and copies the page.
- Created `CLAUDE.md` (defers to `AGENTS.md`).
- Resume point: add the full-page write-watch regression, then run focused/full tests.
