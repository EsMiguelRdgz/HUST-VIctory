# Phase 14.0: Independent Graphics Baseline and Upstream Assessment

Assessment date: 2026-11-24. Read-only assessment: no production code changes, no external code merged, no title launched, no hardware workload run.

This report separates five kinds of statement. **Confirmed** means verified in our source, tests or logs. **Hypothesis** is an inference not yet tested. **Implemented** means already in HUST. **Unverified claim** is something a commit message, README or earlier progress entry states that we could not confirm. **Blocker** is something that stops the work.

## A. Baseline

| Item | State |
|---|---|
| Baseline | `hust/phase12-memory` = tag `hust-baseline-validation-final-2026-11-24` = `94257a2b95e335c2a10be363d8311846b770579c`. The tree is clean, local and origin match, and `git fsck` is clean. |
| Diagnostic executable | Rebuilt from that exact revision: `target\diag\victory.exe`, 6,809,600 bytes, SHA-256 `6E4E480022BF37E2744E1C4186B9D77B127133645356E0C6EDE8A800D5A06D6F`. Not launched. |
| Production code since the last visual run | **Confirmed unchanged.** The last visual validation was Test #40 at `9d4286b`. Every Rust line changed since then lies inside a `#[cfg(test)]` item or a test-only module, apart from doc comments on test-only items. Checked line by line against `git diff 9d4286b..94257a2`. Test #40 therefore still describes current production behaviour. |
| What did change since then | CI workflows, the diagnostic harness, test probes, the Wave A regression tests and documentation. |

### Most recent verified rendering state (Test #40, RTX 3070, `9d4286b`, 152 s, 0 hangs or crashes)

**Performance and memory**
- **Frame rate:** 1.1–1.5 fps (650–950 ms per frame). Of that, 263–324 ms is preparation (copies 217–276 ms) and 156–180 ms is GPU time.
- **Work per frame:**
  - draws: 179–278 run of 202–294 requested;
  - mesh: 427–525 of 455–570;
  - dispatches: only 1,259–1,560 of 2,594–3,502.
  - **1,116–2,261 dispatches are dropped every frame** (`no_pipe`).
- **Untranslated shaders (156):** 130 compute, 20 pixel, 6 mesh. The main reasons:
  - 34 an address in an SGPR that is not a root parameter;
  - 30 a cross-lane wave operation in divergent control flow;
  - 20 "no DXIL part";
  - 17 split-wave64 LDS without a uniform fence;
  - 11 a 64-bit image atomic;
  - 11 a scalar memory read as data;
  - 6 an address from a phi.
- **Memory at the end:** VIctory about 12.0 GB (private 7.9 GB), GPU local 1.48 of 7.25 GB, host textures 1.30 GB, deferred textures 4.66 GB at zero cost.

**Visual evidence**
- Frames were measured by mean and maximum luminance, analysed locally only and never published.
  - **Every 3D-scene frame Test #40 captured (1560–1620) is black:** mean luminance 0.24–0.37 out of 255, maximum 1–2.
  - Frames 1630 and 1640 show only the UI and credits overlay over a black scene.
- **Correction of earlier claims (Unverified claim → corrected).** Earlier progress entries describe "textured 3D Vice City opening scene" frames in Tests #28, #31 and #34. The bright frames in those runs (mean luminance 49–62) show **stylized flat title artwork** in a letterboxed frame. There is **no captured evidence of a visibly rendered 3D world** in any run.
- **Hypothesis:** Tests #39 and #40 were the first runs after the `s_buffer_load` fix (`6409451`), and their captured frames 1560–1620 are all black. Earlier runs (#28, #31, #34) showed title artwork in that range. Because capture is indexed by frame number and the frame rate roughly tripled, the same frame number lands at a different moment of a time-driven sequence. This could be timing or a regression; it is undecided.

### Synthetic validation (section C)
- **WARP harness run on the tagged baseline:** `G:\VIctory-Results\diag-synthetic-cpu-20261124-223750`. Classified `CleanExitWithSkips`:
  - 227 passed, 0 failed, 6 ignored;
  - victory-gpu debug layer `Active`;
  - software adapter only, no adapter violations, nothing left running.
- **Earlier native RTX 3070 baseline:** at `9a8d8c4`, which has the same production code: 113/0/4 and 110/0/0, no TDR.
- **What the synthetic suites verify numerically:**
  - VALU semantics, wave64, heap guard and structured `s_buffer_load` compute results, on hardware and WARP;
  - texture upload, detiling and read-back;
  - lazy-texture and eviction behaviour;
  - the 2-plane MPO compositor's blending.
- **Confirmed gap:** the only end-to-end visual tests, the four `diff_tests` that draw translated shaders and compare them with PC-compiled ones, need `VICTORY_CONSOLE_DXC`, a console shader compiler. That compiler isn't available here, so these tests **have never run in this environment**. The harness now reports them as skipped by name.
- **Confirmed gap:** there is no way to save synthetic renders as images. `capture.rs` writes only frames the title presents. No screenshots of synthetic scenes therefore exist.
- **Confirmed limitation:** the harness's memory samples cover the `cargo` parent process, not the test binaries.

## B. External repositories (read-only, `gh api`; identities verified)

| | Marvisak/VIctory (upstream) | fxceless/victory-fork |
|---|---|---|
| Identity | id 1469344551, public, not a fork, GPL-3.0, `main` | id 1470579726, public, not a fork, GPL-3.0, `main`. **The same repository as `fxceless/victory-gta6`, renamed** (same id; the old name redirects). |
| Latest commit | `940d32f69b22265690a0b5a15d6ded572ba33baf` (2026-11-25 01:36Z) | `c96b4f2b8365bf43d54ab074940923ef8267f4c6` (2026-11-25 01:22Z). Its CI fails at Clippy on this commit. |
| Since our last audit | 450 commits ahead of `d2a4bae` | 12 commits ahead of `004bf69a` |
| 1A bindless `s_buffer_load` | **Implemented, confirmed in source.** Heap constant buffers are read as raw views, with a VIEW_STRIDES table, `buffer_guard` and 6 tests in `stride_tests.rs` (including `a_constant_buffer_through_the_heap_reads_as_a_raw_buffer`). | Implemented: per-slot stride root SRV (`slot_stride`). No dedicated test. |
| 1B format-aliased placed textures | Implemented: `alias_source` / `alias_copy`, with one shared host texture per memory. | Implemented: `alias_of` plus a THROUGH buffer copy. No test. |
| 1C `image_gather4_l` | Partial: `gather_at_level`, immediate sampler and whole LOD only. | Implemented, approximate ("shortcut"). |
| 1D implicit LOD outside pixel shaders | Not merged (open PR #1237). | Implemented as level 0 ("shortcut"). |
| SGPR / phi addresses (40 of our shaders) | Implemented: a title-memory page table (`gpu/memory.rs`). | Implemented: raw addresses through a page directory. |
| 64-bit image atomics (11) | Implemented | Implemented |
| Divergent cross-lane (30), split-wave64 LDS (17) | Not implemented: requires native 64-lane waves (`WaveSize(64)`), which an NVIDIA wave32 host doesn't have. | Partly. The device-loss cause was narrowed to a "wide" barrier-in-branch form, now off by default; `own_bits` stays on. |
| "no DXIL part" (20) | Unchanged | Unchanged |
| MEM_PRIVATE refresh cost (about 164 ms per frame) | Different design: a guard-based change journal, no GetWriteWatch. Plain private memory is still copied whole. | Wave C `ask_once`, unchanged. |
| Hardware-safety harness, process-tree kill, adapter and debug-layer probes, WARP known-defect tests | None | None |

**Overlapping independent work, already implemented in HUST:**
- the heap guard and null tail slot (HUST adopted upstream PR #483; the fork's `31714deb` is equivalent);
- structured `s_buffer_load` (`6409451`);
- wave64 on wave32 by emulation (adapted from upstream PR #189);
- copy-back ordering (PR #1322);
- PLACED_TEXTURES eviction and the `watch.rs` remap race fix (`b8266dd`). The fork still has that race in `title_memory_remapped`, so do not take the fork's `watch.rs`.

**Previous research now obsolete:**
- the "victory-gta6" name;
- "`6a2d9f99` causes the device loss". This is narrowed to the gated wide form, and the cure is claimed, not verified.
- all line references in `docs/fxceless-integration-audit.md`;
- that audit's Wave A designs as the only reference: upstream has independent, tested 1A, 1B and 1C designs;
- our planning for root-parameter-only addressing and a 64-bit-atomic fallback;
- the Phase 13 "indirect deferred" concern (upstream runs indirect work by default with clamps).

**Risks to HUST's memory management. Any port must be re-implemented, never cherry-picked:**
1. **Upstream's `watch.rs` rewrite and page table** need map/unmap hooks on every lazy commit. They assume host copies that persist, which conflicts with HUST's demand-committed physical pages and its shadow reclaim and pressure relief.
2. **Upstream shares one host texture per memory** (`Arc`/`Weak`), which conflicts with lazy textures and eviction. Take only the alias copy.
3. **The fork's defaults** (1024 MB card buffers, 256 lists in flight, early texture uploads, bindless re-checks through `copy_mapped`) would break the 8 GB VRAM and low-commit discipline. Its re-checks would also commit untouched physical pages.
4. **A permanent copy buffer per alias** needs memstat accounting and eviction.

**Licensing:** both projects are GPL-3.0(-or-later), compatible with HUST. Two provenance flags on the fork:
- It says it used XCL (GPL-2.0-only) "as reference only". That cannot be verified.
- Third-party PRs were squashed without per-author history.

Policy stays as before: re-implement independently from described behaviour, copy no code, and credit the source of the idea.

**Unverified claims, from commit messages and READMEs:**
- the fork: 43 % fewer DXIL instructions (about 20 ms per frame), "no hang or device loss in normal runs", black skin and hair fixed by re-checks, depth-bounds visuals;
- upstream: 2.8–4.5 GB per frame less copy-back traffic.

## D. Phase 14 roadmap (prioritized)

### Confirmed defects (fix first; each has a failing WARP regression test)
1. **1A: bindless `s_buffer_load` through a CBV handle removes the device** when the slot holds a buffer SRV. Test: `wave_a_1a_*`, WARP only.
   - Preferred direction: independently re-implement upstream's idea of a raw view for constant buffers in the heap, plus a per-slot stride/kind table. It must keep `heap_guard` and the `6409451` structured rule.
   - Gate: the 1A test passes on WARP with the debug layer before any hardware run.
2. **1B: a format-aliased placed texture misses GPU writes.** Test: `wave_a_1b_*`.
   - Direction: an alias copy only, with no shared host texture, with a bounded VRAM budget, memstat accounting and eviction.

### High-value gaps (confirmed by logs; their effect on the visuals is a hypothesis)
3. **Dropped compute work** (1,116–2,261 dispatches per frame). Address translation coverage in this order:
   - 64-bit image atomics (11; self-contained);
   - SGPR/phi addresses (40), through a page-table-style memory path. This is a high-risk interaction with shadow reclaim and needs a design document first.
   - "no DXIL part" (20): investigate the cause first; nobody has addressed it.
   - Divergent cross-lane and split-wave64 LDS (47) stay hard on wave32 hosts; plan them later.
4. **A dark or black 3D scene** (confirmed in the captures). **Hypothesis:** dropped compute plus 1A and 1B. **Do first:** decide the Test #39/#40 black-frame question with *time-indexed* capture (by elapsed seconds or scene events, not frame numbers) in one authorized run before and after the 1A and 1B fixes.

### Validation infrastructure gaps (confirmed)
5. **No runnable full-scene synthetic visual test.** Add HUST-authored synthetic GFX10 vertex and pixel programs drawn on WARP and the hardware adapter and checked against analytic expected pixels. These don't need the console compiler. Also add test-only PNG export of those renders as reviewable evidence.
6. **Memory telemetry of test binaries is missing:** sample the whole process tree, not just `cargo`.

### Performance (confirmed cost; designs are hypotheses)
7. **MEM_PRIVATE ring-buffer refresh** (about 164 ms per frame) and GetWriteWatch "watches" (about 133 ms per frame). Neither external repository solves it for HUST's memory model. Evaluate per-submission write-watch coalescing, the fork's `ask_once` idea re-implemented, after correctness items 1–4.

### Deferred
- 1C: 0 hits in HUST logs.
- 1D: no evidence in HUST logs.
- DXIL clean-up passes: unverified gains, large risk.
- Ray tracing and BVH.
- The frozen technical debt recorded in `progress.md`: Job Object, workspace skips, the victory-shader debug layer, formatting.

### Remaining blockers
- Hardware runs need explicit owner authorization. Known-defect tests stay WARP-only.
- There is no console shader compiler (`VICTORY_CONSOLE_DXC`), so the existing visual `diff_tests` cannot run. Item 5 is the substitute.
- Frame capture is indexed by frame number, so runs at different frame rates cannot be compared visually (item 4).
