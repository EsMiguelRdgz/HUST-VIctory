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

## Offline Synthetic Self-Test

```powershell
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\diag\Run-VictoryDiagnostic.ps1 `
    -SyntheticCommand "powershell.exe" `
    -SyntheticArgs @("-NoProfile", "-Command", "Write-Output 'synthetic ok'; Start-Sleep -Seconds 3") `
    -SkipBuild -MaxDurationSeconds 30 -NoDiscord
```

## Controlled Hardware Diagnostic Run (Manual User Execution Only)

```powershell
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\diag\Run-VictoryDiagnostic.ps1 `
    -TitleExe "<path-to-title-executable>" `
    -AuthorizeGameLaunch `
    -MaxDurationSeconds 600 `
    -MinCommitHeadroomGB 5.0
```
