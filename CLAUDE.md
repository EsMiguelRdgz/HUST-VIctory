# Claude Instructions for VIctory

Claude agents working in this repository must follow `AGENTS.md` in full. It is the
universal engineering protocol and takes precedence over Claude defaults.

In particular:

- Read `AGENTS.md` and all of `progress.md` before changing source code, and record
  recovery findings and the resume point in `progress.md` first.
- Keep `progress.md` (repository root) current after every significant finding, source
  change, test run, commit, or blocker; append a handoff entry for every session.
- Verified local commits during Phase 10 are pre-authorized (see `AGENTS.md`); ask for
  explicit confirmation before destructive Git operations, remote pushes, or major scope
  changes. Never fetch, merge, rebase, or import upstream development.
- Never launch the game, another application, or a GPU workload automatically; leave
  exact manual validation instructions instead. Do not claim hardware performance or
  memory improvements without measurements from an authorized hardware run.
- Follow `docs/asset-policy.md`; do not modify proprietary binaries or bypass access
  controls.

Validation infrastructure (baseline tag `hust-baseline-validation-2026-11-24`):

- Synthetic GPU validation goes through `tools/diag/Run-VictoryDiagnostic.ps1`
  (see `tools/diag/README.md`). A `-SyntheticCommand` run is a hardware run with the
  full preflight unless `-SyntheticCpuOnly` is passed; CPU-only cargo runs reaching
  victory-gpu need `--features victory-gpu/diagnostics` (the harness refuses
  otherwise). Hardware runs still need explicit owner authorization.
- The Wave A regression tests (`wave_a_1a_*`, `wave_a_1b_*`) are `#[ignore]` known
  defects and WARP-only: never run them, or `--ignored`, on physical hardware.
- Check harness changes with `tools/diag/Test-VictoryDiagnostic.ps1` (CPU only) and
  `tools/ci/Test-HardwareSafety.ps1`, under Windows PowerShell 5.1 (the home runner's
  edition). Scripts under `tools/` must be ASCII or UTF-8 with BOM.
- GPU tests that return early because a dependency is missing must report it
  (`emit::test_probe::skip` in victory-shader) instead of passing silently.
