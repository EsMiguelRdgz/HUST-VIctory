# HUST-VIctory Controlled Diagnostic Harness

`Run-VictoryDiagnostic.ps1` runs either an offline synthetic verification process or an explicitly authorized hardware diagnostic run of `target\diag\victory.exe`.

## Safety Controls

- **Explicit game launch gate**: refuses to run a title executable unless `-AuthorizeGameLaunch` is passed.
- **Sequential execution & GPU health**: refuses parallel launches (`victory`/`game_scarlett_master`), checks NVIDIA adapter status (`Win32_VideoController`) and recent Windows Display TDR reset events (Event ID 4101), and enforces a configurable recovery interval between runs (`-RecoveryIntervalSeconds`, default `60`).
- **Persistent launch history (no daily quota)**: records every hardware launch (timestamp, commit, label, result, elapsed time, memory metrics, run directory) to `%LOCALAPPDATA%\HUST-VIctory\hw-launch-history.json` with no fixed daily launch limit by default (`-MaxLaunchesPer24h 0`).
- **No automatic retries**: executes once and exits cleanly.
- **Commit guard always ON**: never sets `VICTORY_MEMORY_GUARD=0` and stops the run if Windows system commit headroom drops below **5 GB** (configurable via `-MinCommitHeadroomGB`; pre-launch floor **10 GB** via `-MinPreLaunchHeadroomGB`).
- **Cache & OS safety**: never clears shader/pipeline caches and never modifies Windows pagefile, Windows security, or TDR settings.
- **Output isolation**: writes all reports, logs, CSV samples, and optional frame captures outside the repository under `G:\VIctory-Results\diag-<mode>-<timestamp>\`.

- **Synthetic runs are hardware runs by default**: `-SyntheticCommand` without `-SyntheticCpuOnly` gets the same preflight as a title launch: NVIDIA health, no Event 4101 in the last 3 minutes, no running `victory`/title/cargo test binary (`victory_*-*`), at least 10 GB commit headroom, the recovery interval, and an entry in the launch history. `-SyntheticCpuOnly` skips the hardware preflight and instead forces the software adapter in every crate (`VICTORY_WARP=1`, `VICTORY_D3D12_WARP=1`).
  - victory-gpu honours `VICTORY_D3D12_WARP` only with its `diagnostics` feature. So a CPU-only cargo `test`/`bench`/`run` that selects `victory-gpu`, `victory-hle`, `victory` or the whole workspace without it is refused before execution. Add `--features victory-gpu/diagnostics`.
  - A CPU-only run must also prove it stayed on the software adapter. A probe reporting a non-software adapter classifies the run `CpuOnlyAdapterViolation`. A probed crate (`victory-gpu`, `victory-shader`) that the cargo test ran without leaving adapter evidence classifies it `AdapterNotVerified`.
- **Whole process tree**: the harness records every process the child starts (cargo, test binaries, their children). A timeout or commit-guard stop terminates the whole tree, not just `cargo`. Anything still running after the child exits is reported (`StragglersTerminated`) and ended. A tree that cannot be ended is classified `ProcessTreeNotTerminated`. PIDs are matched with creation times, so a reused PID is never killed.
- **Honest outcomes**: synthetic summaries carry cargo's test counts (`TestsPassed/Failed/Ignored`) and every skipped or unsupported dependency line (`SkippedOrUnsupported`). The run is classified:
  - `CleanExitWithSkips` (not `CleanExit`) when anything was skipped;
  - `TestFailures` when a test failed;
  - `NoTestResults` when a cargo run printed no result.
- **Debug layer verified, not assumed**: `-EnableDebugLayer` sets `VICTORY_D3D12_DEBUG=1`; without it the harness removes that variable. The probe tests (`host::tests::probe_reports_adapter_and_debug_layer` in victory-gpu, `emit::valu_tests::probe_reports_adapters_and_debug_layer_support` in victory-shader) write what they observed to `VICTORY_TEST_PROBE_FILE` (`test-probe.jsonl` in the run directory): the adapter, whether it is software, and whether the debug layer is active.
  - The summary reports `DebugLayerByCrate` as `Active`, `NotActive`, `NotEnabledByTestHost` (victory-shader's test host never enables the layer), `NotRequested` or `NoProbe`.
  - A run where the layer was requested but is not shown active is classified `DebugLayerNotVerified`.
  - In victory-gpu tests, `assert_no_debug_errors` now fails when the layer was requested but is missing, instead of passing.

## Offline Synthetic Self-Test (CPU only)

`Test-VictoryDiagnostic.ps1` checks the harness without a GPU or a title:
- the preflight decisions, the output parser, process-tree collection and the debug-layer verdicts, with synthetic inputs;
- termination of a real process tree;
- end-to-end `-SyntheticCpuOnly` runs: a timeout with a grandchild, a skipped dependency, and an unverifiable debug layer;
- that `tools/ci/Test-HardwareSafety.ps1` rejects harness copies with the new guards removed.

Windows CI runs it under Windows PowerShell 5.1.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File G:\VIctory\tools\diag\Test-VictoryDiagnostic.ps1
```

From a PowerShell prompt (`-SyntheticArgs` is an array, so call the script with `&` rather than `-File`):

```powershell
& G:\VIctory\tools\diag\Run-VictoryDiagnostic.ps1 -SyntheticCpuOnly `
    -SyntheticCommand "powershell.exe" `
    -SyntheticArgs @("-NoProfile", "-Command", "Write-Output 'synthetic ok'; Start-Sleep -Seconds 3") `
    -SkipBuild -MaxDurationSeconds 30 -NoDiscord
```

## Synthetic D3D12 Baseline on the Hardware Adapter (Manual User Execution Only)

This runs the repository's own D3D12 tests on the RTX 3070, under the full hardware preflight, with the debug layer, one test at a time. It launches no title. Never pass `--ignored`: the Wave A regression tests are known defects and are WARP-only.

```powershell
& G:\VIctory\tools\diag\Run-VictoryDiagnostic.ps1 -RepoRoot <clean checkout of the commit> `
    -SyntheticCommand cargo `
    -SyntheticArgs @('test','-p','victory-shader','-p','victory-gpu','--features','victory-gpu/diagnostics','--','--test-threads=1') `
    -EnableDebugLayer -SkipBuild -MaxDurationSeconds 900 -NoDiscord -TestLabel "native-d3d12-baseline-<sha>"
```

## Controlled Hardware Diagnostic Run (Manual User Execution Only)

```powershell
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\diag\Run-VictoryDiagnostic.ps1 `
    -TitleExe "<path-to-title-executable>" `
    -AuthorizeGameLaunch `
    -MaxDurationSeconds 600 `
    -MinCommitHeadroomGB 5.0
```

## GitHub Actions (home RTX 3070 runner)

`.github/workflows/home-rtx3070-diagnostic.yml` can start one harness run on the owner's self-hosted runner (`hust-rtx3070`) for an explicitly selected, reviewed commit. It calls this script unchanged, runs `tools/ci/Test-HardwareSafety.ps1` first, and uploads only the sanitized bundle from `tools/ci/Export-DiagnosticArtifacts.ps1`. Setup: `docs/home-gpu-runner-setup.md`.
