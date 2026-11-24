# Current Project Status

- Current milestone: Phase 11 — P0 RAM / system-commit exhaustion investigation (supersedes Phase 10 performance tuning)
- Current subphase: Phase 11 offline investigation COMPLETE (A-F); awaiting one controlled, instrumented hardware diagnostic run (see Phase 11 GO/NO-GO)
- Current branch: `hust/nvidia-rtx3070`
- Latest verified commit: `57676e4` (Phase 11 commits: `c7564ce`, `5f2044c`, `83e9969`, `de0870e`, `a773911`, `d197be5`, `df5ac87`, `57676e4`, plus this progress commit)
- Current working-tree status: clean except this checkpoint
- Active agent: Claude Code
- Model: Claude Opus 5.5
- Last checkpoint timestamp: 2026-11-23 (Phase 11 start)
- Current build status: diag build OK (`target/diag/victory.exe`, not launched)
- Current test status: full matrix PASSED at `57676e4` (workspace 267 passed / 0 failed / 7 ignored: 3 external-data + 4 Phase 11 benchmarks; policy 152 ok; clippy both configs only the pre-existing `list.rs:1430` warning; diag build OK)

## Phase 11 trigger (user report, RTX 3070 test 1)

- Windows 11, RTX 3070 8 GB, 32 GB RAM. Task Manager: RAM 26.8/31.9 GB, available 5.1 GB, system commit 49.5/50.5 GB (~1 GB headroom); game unresponsive; manually closed; no new frame captures; no GPU crash confirmed.
- `G:\VIctory-Results
tx3070-test-1.txt` has no application log (transcript only). It shows the run used `VICTORY_D3D12_DEBUG=1` (D3D12 debug layer ON), `VICTORY_D3D12_DRED=1`, `VICTORY_COPY_WORKERS=1`, `VICTORY_LOG=victory_gpu=info,info`, `VICTORY_CAPTURE` set, diag build `target\diagictory.exe`.
- Earlier logs (`phase9-rtx3070/run-20261123-205042/console.txt`, UTF-16LE) predate memstat: only 8 "large buffer copied" lines (>=16 MiB) totalling ~465 MB; no commit data.

## Phase 11 findings (source-level, ranked by expected commit contribution)

Commit charge is what exhausted the system (49.5/50.5 GB). Reservations cost nothing; committed private memory and committed section pages are charged whether touched or not; working set is only what is resident.

1. EXPECTED RESIDENCY — guest title memory. `console::title_memory()` advertises Series X 10 GiB GPU-optimal + 3.5 GiB standard = 13.5 GiB, and the title sizes its pools to it: `XMemVirtualAlloc` passes the title's own MEM_COMMIT through (private commit, write-watched), physical pages (`XMemAllocatePhysicalPages`, 6.1 GiB in one call for GTA VI) are SEC_RESERVE sections committed page by page when mapped/written (`commit()`), title D3D12 heaps come from `resource::Memory::new` (`alloc_zeroed`: fully committed at creation). Up to ~13.5 GiB is legitimate.
2. DUPLICATION — every title buffer the GPU uses gets a full-size host copy (`shadow::find`, CUSTOM write-back L0 = system memory, committed), plus loose 4 MiB windows and aligned copies; every title texture gets a host texture (`resource.rs` create, DEFAULT heap, created eagerly for every title texture; spills to system memory when the 8 GB VRAM budget is exceeded).
3. DUPLICATION (FIXED `a773911`) — every title-written texture kept a permanent full-size UPLOAD-heap staging buffer (system memory, committed) for its whole life, i.e. a second system copy of all sampled title textures. Now lent per submission from a 256 MiB pool and only when contents changed.
4. RETENTION (FIXED `de0870e`) — physical-page sections were one per allocation; committed section pages cannot be decommitted individually, so a pool stayed fully committed until its LAST page was freed. Measured (synthetic, 512 MiB, system commit delta, `bench_physical_page_commit`): after freeing 7/8 of the pages BEFORE +515 MiB retained, AFTER +67 MiB (the 1/8 still allocated); all freed ~+3 MiB both. Now 16 MiB sections.
5. CONFOUNDER — the failing run used `VICTORY_D3D12_DEBUG=1` (debug layer: per-object tracking, shadow descriptor state, stored messages) and `VICTORY_D3D12_DRED=1`; both add memory not present in normal runs and must be off for the next measurement.
6. VISIBILITY GAP (FIXED `5f2044c`) — the memory line was logged only on presented frames, so a stalled title stopped reporting; now a monitor thread logs every 5 s (address-space walk every 30 s) and can write `VICTORY_MEMORY_REPORT_DIR` files.
7. MINOR / BOUNDED — descriptor heaps (~32 MB per 1M-slot heap, now counted), list scratch/uploads (until list reset, now counted), snapshot pool (64 MiB), texture read pool (128 MiB), DirectStorage staging (32 MiB), shader cache (weak refs since Phase 10).
8. NOT A LEAK (verified by tests) — loose/aligned copies are freed when idle (descriptor ownership keeps in-use ones); fence waits are synchronous so no submission retains resources past `Host::execute`; list uploads die at reset.

Uncertain without a run: the real sizes of items 1-2 for GTA VI, whether host textures exceed VRAM and spill, and how much the debug layer added. The new telemetry answers all three.

## Phase 11 measurements (synthetic, this machine, `G:\VIctory-Results\memory-investigation\memory-*.log`)

- Reservation 64 GiB: commit +0 MiB; +256 MiB committed: +256 MiB; released: +0 MiB.
- Physical pages 512 MiB (`bench_physical_page_commit`): allocated +0, mapped +514, unmapped +514, 7/8 freed BEFORE +515 / AFTER +65..68, all freed ~+1..3 MiB.
- Default-heap textures 4x64 MiB (`bench_default_heap_commit`): private +256 MiB, system commit +256 MiB, GPU local +256 MiB -> video memory is commit-backed on this WDDM host; after release +192 MiB stays, flat over 10 create/free cycles (bounded driver reuse, not growth).
- Loose copies 160x4 MiB idle (`bench_copy_reclamation_and_reservations`): peak live 512 MiB = budget; private +514 MiB.
- Texture staging 48x1 MiB (`bench_texture_staging`): old permanent staging 48 MiB vs now 1 MiB lent (pool bound 256 MiB); title memory + host textures still cost ~2x (+102 MiB private).
- Summary for humans: `G:\VIctory-Results\memory-investigation\phase11-summary.md` (not in Git).

## Phase 11 GO/NO-GO

- GO for ONE controlled, instrumented diagnostic run; NO-GO for gameplay testing.
- Required settings: unset `VICTORY_D3D12_DEBUG`, `VICTORY_D3D12_DRED`, `VICTORY_CAPTURE`; `VICTORY_LOG=info`; `VICTORY_MEMORY_REPORT_DIR=G:\VIctory-Results\memory-investigation`; leave the commit guard on (do not set `VICTORY_MEMORY_GUARD=0`); Task Manager open; stop after the first world load or 10 minutes, or when the guard ends the run.
- Collect: the `memory-<unix>.log` file, especially `host textures`, `buffer copies`, `title heaps`, `guest physical`, `address space` lines and GPU local/non-local vs budget.
- Not claimed: that the GTA VI commit exhaustion is fixed. Two duplication/retention mechanisms are fixed and measured synthetically; the dominant expected consumers (title memory, host textures, buffer copies) remain by design until measured.

## Active Task

- Objective: none in progress; Phase 11 offline work complete.

## Immediate Resume Instructions

- First action: when the user provides the controlled-run memory report, record per-category numbers here and rank them.
- Next implementation step (evidence-dependent): if `host textures` dominate, create host textures lazily on first GPU use (`resource.rs` texture creation; all `host_resource()` callers must tolerate creation on demand); if `buffer copies` dominate, evaluate `ID3D12Device3::OpenExistingHeapFromAddress` to let the GPU read title memory directly instead of full host copies (architectural; decision 0007 revision); if `title heaps` dominate, consider routing CreateHeap memory through the title allocator/xmem like committed resources.
- Pending tests: hardware diagnostic run (manual). Benchmarks: `cargo test -p victory-gpu --lib bench_ -- --ignored --test-threads=1 --nocapture`, `cargo test -p victory-hle --lib bench_ -- --ignored --test-threads=1 --nocapture` (set `VICTORY_MEMORY_REPORT_DIR` to also write the report).
- Known blockers: none offline.
- Remaining risks: commit guard ends the run (by design) when the whole system is near its commit limit, including pressure caused by other processes; `XMemGetWorkingSetStatistics.title_used` reports process private bytes (includes host copies, excludes section pages) — may mislead the title's budgeting; host textures for every title texture are created eagerly.
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

- VERIFIED — Standing commit authorization recorded in `AGENTS.md`/`CLAUDE.md` (`54a7346`).
- VERIFIED — Priority B snapshot isolation (`8f834d1`): `Shadow.before` slot removed; `snapshot_ranges` returns an owned `Snapshot` (buffer returned to the pool on drop) taken in `Host::execute` under the `submitted` lock; `copy_back_ranges(ranges, Option<&Snapshot>)`. New test `overlapping_submissions_keep_their_own_snapshots`. `cargo test -p victory-gpu` 53/53; clippy (default and `--features diagnostics`) only the old `list.rs:1398` warning; rustfmt OK.
- VERIFIED — Fence-wait robustness (`e24bc28`): `host::wait_fence` confirms `GetCompletedValue() >= value`, treats `u64::MAX` (device removed), a never-accepted Signal, and stale wakes of the shared auto-reset event as unconfirmed, and polls up to 10 s if the event wait/arming fails. Unconfirmed submissions skip copy-back, log an error with a running count (`UNCONFIRMED_WAITS`), and call new `Shadow::release` for their pending ranges; bottom-of-pipe markers are still written (trade-off: avoids title deadlock; such cases coincide with device removal, which `fail()`s right after). Tests `fence_waits_confirm_only_real_completion` (real D3D12 fence, CPU signals incl. `u64::MAX`) and `released_gpu_writes_allow_refreshes_again`. `cargo test -p victory-gpu` 55/55; clippy both feature sets clean apart from the old warning.

- VERIFIED — Priority 3 memory accounting (`ec38425`): new `crates/victory-gpu/src/memstat.rs` counts live/peak bytes+counts per kind (buffers, loose, aligned, in-flight snapshots, snapshot pool); `Shadow` now carries `kind`/`bytes` and uncounts in `Drop`. Host `Meter` logs `host memory: private/working set; system commit/limit; GPU local/non-local usage/budget (IDXGIAdapter3::QueryVideoMemoryInfo); copies of title memory: ...` every 5 s in all builds. Snapshot pool now capped at 8 buffers AND 64 MiB (largest evicted first); new snapshot buffers use `try_reserve_exact`, falling back to whole-range copy-back on OOM. Tests: `counts_track_live_bytes_and_peaks_without_wrapping`, `process_and_system_memory_are_reported`, `snapshot_pool_keeps_within_its_byte_budget`, `aligned_shadows_are_counted_until_released`, `video_memory_reports_a_local_budget`. `cargo test -p victory-gpu` 60/60; 40 parallel stress runs 0 failures; clippy (both feature sets) only old warning. Incident: an over-broad `rustfmt crates/victory-gpu/src/lib.rs` reformatted untouched `capture.rs`/`queue.rs`; both were restored byte-for-byte from HEAD before committing (never pass lib.rs to rustfmt — it follows `mod` declarations).

- VERIFIED — Priority 4/5 investigation + reclamation (`ba4cb15`). Findings: (1) descriptor slots (`descriptor/host.rs` `make_on`) were created on shadow host buffers without holding an `Arc`, which is why loose/aligned copies could never be freed safely; every other GPU-address use (root views, VB/IB, table refreshes) holds the `Arc` via the list. (2) `loose()` looked up windows by exact `(start,len)`, so a request spanning a window boundary created an overlapping duplicate copy of the same title memory (memory amplification + divergent copies). (3) Every title buffer object gets a full-size host copy in a CUSTOM write-back L0 heap (system-memory commit ≈ total size of title buffers used by draws) — architectural, unchanged, a prime suspect for high commit; measure with the new `host memory` line. Changes: `HostHeap.owners: HashMap<u32, Arc<Shadow>>` keeps loose/aligned copies a slot shows (`make_owned`, set in `mirror`/`make_at`, cleared in `make_null`/`make_null_for`/`release`); buffer-object copies keep free-at-unregister lifetime. `reclaim()` frees idle copies (strong_count==1, no pending writes) LRU-first past `LOOSE_BUDGET` 512 MiB / `ALIGNED_BUDGET` 256 MiB (unmeasured defaults); loose lookup now uses containment. Tests: `reclaim_frees_only_idle_copies_oldest_first`, `loose_requests_inside_a_window_share_it`, `slots_keep_the_copies_they_need_alive`. `cargo test -p victory-gpu` 63/63; 40 stress runs 0 failures; clippy both feature sets only the old warning.

- VERIFIED — Workspace at `ba4cb15`: `cargo test --workspace` 250 passed / 0 failed / 3 ignored.
- VERIFIED — Priority 6 copy-back (`6e0728c`): snapshots hold merged written ranges only (segments), not the min..max envelope; `copy_back_timed` returns write microseconds; frame-rate line now prints `copy-back X ms Y MB, of it writes Z ms; snapshots A ms B MB`. Test `snapshots_hold_only_the_written_ranges`. 64/64.
- VERIFIED — Priority 7 bounded CPU parallelism (`ac7525f`): new `crates/victory-gpu/src/par.rs` (`workers()` = hw threads-1 capped at 4, env `VICTORY_COPY_WORKERS`; `for_each` runs on caller below threshold, else scoped threads + atomic index, nothing retained). `Batch::flush` uses it for batches >= 8 MiB (`PARALLEL_REFRESH`, unmeasured). Copy-back intentionally left serial (nested shadows may write the same title bytes). Tests in `par::tests`. 67/67.
- FAILED → FIXED — Stress after `ac7525f` showed `watch::tests::written_pages_are_reported_to_every_asker` failing 2/200 (`before.is_empty()` at watch.rs:315). Cause: my Phase 10 test `copy_back_does_not_mark_watched_pages_fresh` freed its write-watched page; the process-wide watch journal keeps entries for freed pages, so the reused address looked written. Fixed in test only (`e9d3629`: keep the page). 0/250 afterwards. Production impact of stale journal entries: only extra copies, never skipped ones (new shadows start with copied=0).

- VERIFIED — Texture memory (`bd1e569`): finding — each title-written texture in watched memory kept a full-size `Last.read` buffer for good in addition to its full-size upload staging buffer (two permanent system-memory copies per texture besides the GPU texture). Reads now use a shared pool (4 buffers / 128 MiB, initialized, no clearing needed since `copy_mapped` defines every byte, zeros for unmapped pages). `memstat` gains `TextureStaging` and `TextureRead`. Test `read_buffers_are_pooled_within_bounds`. Remaining copy: the per-texture upload staging buffer is still permanent (needed for re-upload without re-detile); removing it is a candidate after measurement.
- VERIFIED — Parallel detile (`318c2ca`): textures >= 8 MiB detile subresources on `par::workers()`; a failing subresource no longer aborts the rest. Test `detiling_fills_every_subresource` (512x512x2 slices x3 mips x16 B, SW_64KB_D) checks every row; mutation (skip subresource 4) made it FAIL; restored code passes. 69/69, 50 stress runs 0 failures.
- DEFERRED — Priority 8 GPU acceleration evaluation (Claude, 2026-11-23): (a) GPU detiling: `victory_formats::texture::Layout` addressing is table-driven (per-column `column()` + per-row `row()` offsets, mip tail, slices), so a compute shader port is feasible: upload raw tiled bytes, run a CS that reads `ByteAddressBuffer` with column/row offset tables and writes the linear footprint buffer (or the texture via UAV), then the existing `CopyTextureRegion`. Blockers: the crate has no HLSL→DXIL build path (would need committed HLSL source plus a reproducible compiled blob or runtime compiler), XOR swizzles are unsupported already, and there is no measurement showing detile CPU time matters. (b) GPU-side copy-back dirty masks: would replace the CPU diff of snapshots with a CS comparing pre/post copies on the GPU and emitting a run list; only worthwhile if `copy-back … of it writes` is small relative to total copy-back time (i.e. comparison dominates). (c) Copy queue for staging→texture uploads: copies already ride the direct list; moving them needs cross-queue fences and gains nothing while `Host::execute` waits synchronously. Decision: defer all three until the manual hardware run reports `textures` ms and the copy-back split; keep CPU paths as fallback if implemented.

- VERIFIED — Full Phase 10 verification matrix at `318c2ca` (Claude, 2026-11-23): `cargo xtask policy` → `asset policy: 152 files ok`; `cargo test --workspace` → 256 passed, 0 failed, 3 ignored (the 3 need external reference data, as at baseline); `cargo clippy --workspace --all-targets` exit 0, only pre-existing `list.rs:1398` undocumented-unsafe warning; `cargo clippy -p victory-gpu -p victory --features diagnostics --all-targets` exit 0, same single warning; `cargo build --profile diag -p victory --features diagnostics` exit 0 → `target/diag/victory.exe` (not launched).

- VERIFIED — Priority F title-sized allocations (`dc93210`): audit of `vec!`/`with_capacity`/`resize` in host/list/draw/resource/queue/shadow/texture/descriptor found three title-controlled infallible allocations: `allocate_upload` (now `try_reserve_exact`, returns null as documented), `SetRenderTargets` one-range handle list (count capped at D3D12's 8), root constants `resize` to title offset (writes ending past 65,536 DWORDs dropped). Others are bounded by API limits, internal sizes, or debug-layer message sizes. Tests `title_sized_upload_allocations_fail_softly`, `root_constant_offsets_are_bounded`. Workspace 258/0/3.

- VERIFIED — Phase 11 instrumentation (`5f2044c`, `83e9969`): memstat kinds for host textures, list uploads, descriptor heaps, title heaps, guest physical; D3D12 lifetime tracking via private-data tracker (test proves uncount at destruction); available physical; address-space walk (test: 256 MiB reserve vs commit vs release); pressure floor; monitor thread; report file. 74/74 at the time.
- VERIFIED — Phase 11 physical-page chunking (`de0870e`): before/after numbers in Phase 11 findings #4; tests `partly_freed_physical_pages_return_their_commit` (mutation: CHUNK=1<<30 makes it FAIL) and `physical_page_commit_is_accounted`. victory-hle 83/0/1.
- VERIFIED — Phase 11 transient texture staging + pressure relief (`a773911`): test `title_texture_contents_are_uploaded_when_they_change` on the host device (mutation dropping `upload.record` FAILS it), `pressure_relief_frees_idle_copies_and_pools`; 76/76; 30 stress runs 0 failures. A first version of the pressure test was flaky in parallel (heap address reuse let another test's never-freed loose window cover it); fixed by a synthetic address range.

- VERIFIED — Phase 11 synthetic benchmarks + concurrent release test (`df5ac87`) and commit guard (`57676e4`, test `the_commit_guard_relieves_before_it_stops`).
- VERIFIED — Phase 11 full matrix at `57676e4`: policy 152 ok; workspace 267/0/7; clippy workspace and diagnostics only the pre-existing `list.rs:1430` warning; diag build OK.

## Incomplete Tasks

- IN_PROGRESS — Remaining Priority A: shadow lifetime audit DONE (see Completed); snapshot pool bounded DONE; remaining: staging/scratch/upload allocations in `host.rs` (`list.scratch`, texture uploads) audit and allocation-failure handling.
- IN_PROGRESS — Priority B measurement: snapshot memcpy, compare vs title-write split DONE (`6e0728c`); page protection/watch query time already in diagnostics copystat; remaining: authorized hardware run to read the numbers.
- IMPLEMENTED_UNVERIFIED (hardware) — Priority C bounded CPU parallelism for refresh batches (`ac7525f`); unit-verified, speedup unmeasured.
- DEFERRED — Priority D: evaluated (see Completed Tasks); awaiting hardware measurements.
- IMPLEMENTED_UNVERIFIED (hardware) — Priority E: reporting (`ec38425`), bounded pools (`ec38425`, `bd1e569`), reclamation (`ba4cb15`) done and unit-tested; effect on real commit unmeasured.
- VERIFIED — Priority F allocation robustness (see Completed Tasks, `dc93210`).
- VERIFIED (software) / PENDING (hardware) — Final verification matrix passed at `318c2ca`; manual authorized-hardware validation instructions written (see Manual validation) and not yet performed.

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
- Session 2 (same agent, after user granted standing commit authorization): committed `54a7346`, `8f834d1`, `e24bc28` as recorded in Completed Tasks. Resume at Priority 3 accounting.
- Session 2 continued: committed `6e0728c`, `e9d3629`, `ac7525f`, `341fbd6`, `24dd7ab`, `bd1e569`, `318c2ca`. Phase 10 remains open pending the authorized hardware run.
- Session 3 (user: continue without hardware): committed `dc93210` (allocation guards); assessed the shared-shadow refresh race (no fix needed). All remaining Phase 10 work needs the authorized hardware run.

### 2026-11-23 — Claude Code / Claude Opus 5.5 — Phase 11 memory investigation

- Starting commit: `05d6207`; tree clean; AGENTS.md/CLAUDE.md/progress.md present.
- In progress: see Phase 11 sections above.
- Phase 11 outcome: offline investigation complete; commits `c7564ce`..`57676e4`; GO for one controlled instrumented diagnostic run, NO-GO for gameplay testing; RAM problem NOT claimed fixed.
