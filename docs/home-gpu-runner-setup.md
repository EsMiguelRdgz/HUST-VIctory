# Home RTX 3070 runner setup

This sets up the owner's Windows PC as a GitHub Actions **self-hosted runner**
for `.github/workflows/home-rtx3070-diagnostic.yml`. Day-to-day development
and the hosted Windows CI (`.github/workflows/windows-ci.yml`) need none of
this.

## How it fits together

| Piece | Where it runs | What it may do |
|---|---|---|
| Claude Code (cloud) | Anthropic container | Edit code, run Linux-side checks, push to branches. Never launches the title. |
| `Windows CI` workflow | GitHub-hosted `windows-latest` | Policy, rustfmt, Clippy, WARP shader/GPU tests, diag build. No hardware GPU. |
| `Home RTX 3070 diagnostic` workflow | `preflight` on GitHub-hosted Ubuntu, `hardware` on **your PC** | One harness run of one reviewed commit, started by you. |

Things that stay on your PC and are never uploaded: the title executable and
title files, the results directory (`G:\VIctory-Results\diag-hw-*`), captured
frames, unfiltered logs, shader/pipeline caches, and the Discord webhook. The
workflow reads where the title lives from the runner's local `.env` file
(below). It never downloads files or artifacts. It uploads only a sanitized
bundle made by `tools/ci/Export-DiagnosticArtifacts.ps1`: `summary.json`,
`report.md`, `samples.csv`, `memory-*.log`, crash logs, and stdout/stderr with
the title's own output removed. Local paths, your user name and the PC name
are replaced with placeholders.

The workflow cannot weaken the harness. It launches only through
`tools/diag/Run-VictoryDiagnostic.ps1`. That script keeps its guards: the
10 GB pre-launch and 5 GB runtime commit floors, the commit guard, a single
instance, the NVIDIA health and Event 4101 checks, the 60 s recovery
interval, the maximum duration and launch history. Before launching, the
workflow runs `tools/ci/Test-HardwareSafety.ps1` from its own revision
against the selected commit and refuses if any guard is missing. It never
changes TDR, Defender, the pagefile or boot settings. If TDR is disabled
(`TdrLevel=0`), it refuses to run.

## Prerequisites (once)

1. Windows 10/11 x64, RTX 3070, a current NVIDIA driver, and **TDR left at its
   default**. Do not set `TdrLevel`, `TdrDelay` or `TdrDdiDelay`.
2. The repository stays **private**. Self-hosted runners on public
   repositories can run code from strangers' pull requests.
3. The build tools you already use for VIctory: Git for Windows, Visual Studio
   Build Tools (C++ workload, Windows SDK), and rustup. The pinned toolchain
   is installed from `rust-toolchain.toml` automatically. To install it in
   advance, run this in a normal PowerShell window:

   ```powershell
   rustup toolchain install 1.98.1 --target x86_64-pc-windows-msvc --component rustfmt clippy
   ```

4. About 15 GB free on the drive that holds the runner. Each runner keeps its
   own `target\` for incremental `diag` builds.

## Step 1: Get the official registration commands

1. On GitHub, open the repository, then **Settings → Actions → Runners → New
   self-hosted runner**. The direct link is
   <https://github.com/EsMiguelRdgz/HUST-VIctory/settings/actions/runners/new?arch=x64&os=win>.
2. Choose **Windows** and **x64**. The page shows two blocks:
   - **Download**: creates a folder, then downloads and checks the runner zip
     (it includes the SHA-256 to verify).
   - **Configure**: `./config.cmd --url https://github.com/EsMiguelRdgz/HUST-VIctory --token <TOKEN>`.
3. The `<TOKEN>` is short-lived (about one hour) and works only for
   registration. Paste it straight into your local PowerShell. Never put it in
   a file, a commit, an issue or a chat.

GitHub documentation:
<https://docs.github.com/actions/hosting-your-own-runners/managing-self-hosted-runners/adding-self-hosted-runners>.

## Step 2: Install and configure with the unique label

Open a **normal (non-administrator) PowerShell** in your logged-in desktop
session. The example uses `G:\actions-runner-hust`; use any short path outside
the repository and outside the title folder.

```powershell
New-Item -ItemType Directory -Force G:\actions-runner-hust | Out-Null
Set-Location G:\actions-runner-hust
# Paste and run the "Download" commands from the GitHub page here.
```

Then configure it, using the token from the page:

```powershell
.\config.cmd --url https://github.com/EsMiguelRdgz/HUST-VIctory --token <TOKEN-FROM-THE-PAGE> --name hust-rtx3070-home --labels hust-rtx3070 --work _work
```

- `--labels hust-rtx3070` adds the unique label. GitHub also gives the runner
  `self-hosted`, `Windows` and `X64`. The hardware job requires all four.
- If the script asks **"Would you like to run the runner as service?"**, answer
  **N**. A Windows service runs in session 0, which has no interactive
  desktop. The workflow refuses to run there.
- Keep the default runner group. Do not give this machine's label to any other
  runner.

To change the labels later, use **Settings → Actions → Runners →
hust-rtx3070-home → Labels**.

## Step 3: Tell the runner where your local files are

Create `G:\actions-runner-hust\.env` (no extension). The runner reads it when
it starts, and jobs see the values as environment variables. These paths stay
on your PC:

```text
HUST_VICTORY_RUNNER_ID=hust-rtx3070
HUST_VICTORY_TITLE_EXE=D:\path\to\your\title\executable.exe
HUST_VICTORY_RESULTS_ROOT=G:\VIctory-Results
```

- `HUST_VICTORY_RUNNER_ID` must be exactly `hust-rtx3070`. The job checks it to
  confirm it landed on this machine.
- `HUST_VICTORY_TITLE_EXE` is the title you already pass as `-TitleExe`. It
  must be outside the runner's `_work` folder.
- `HUST_VICTORY_RESULTS_ROOT` must already exist. Each run creates
  `diag-hw-<timestamp>` inside it, as manual runs do.

Restart the runner after you edit `.env`.

## Step 4: Repository settings

In **Settings → Secrets and variables → Actions**:

1. On the **Variables** tab, add `HUST_RTX3070_RUNNER_REGISTERED` = `true`.
   Until it says `true`, the preflight refuses every hardware run. Set it to
   `false` to switch hardware runs off without touching the runner.
2. Optional but recommended: a live online check. Create a fine-grained
   personal access token at <https://github.com/settings/personal-access-tokens/new>:
   - **Repository access**: only `EsMiguelRdgz/HUST-VIctory`.
   - **Repository permissions**: **Administration: Read-only**. This permission
     is needed to list runners. Nothing else.
   - Store it as a secret named `HUST_RUNNER_STATUS_TOKEN`, preferably on the
     `home-rtx3070` environment from item 3. The workflow file holds only the
     secret's name, never its value.

   With the token set, the preflight refuses unless a runner labelled
   `hust-rtx3070` is online. Without it, the hardware job's stale-queue guard
   still refuses any job that waited longer than `max_queue_minutes` (default
   5) for the runner. A run dispatched while the PC was off therefore never
   starts by itself later.
3. In **Settings → Environments**, create `home-rtx3070`:
   - **Required reviewers**: yourself. Every hardware run then waits for your
     click before it can start. The stale-queue timer starts only after you
     approve.
   - **Deployment branches**: selected branches, `hust/phase12-memory`.
4. In **Settings → Actions → General → Workflow permissions**, choose **Read
   repository contents**.

## Step 5: Start the runner in your interactive session (each time you test)

```powershell
Set-Location G:\actions-runner-hust
.\run.cmd
```

- Run it from a normal PowerShell or Command Prompt in your own logged-in
  desktop session. Do not run it elevated, as a service, through Task
  Scheduler "run whether user is logged on or not", or over Remote Desktop.
  D3D12 presentation needs your desktop.
- Wait for `Listening for Jobs`. The runner shows **Idle** on the Runners page.
- Close other heavy applications first. The harness refuses to launch below
  10 GB commit headroom.
- Keep the PC awake for the whole run (Settings → System → Power).
- Press Ctrl+C when you finish testing. While the runner is offline, nothing
  can start a hardware run on this PC.

## Step 6: Start a hardware run

The workflows appear in the **Actions** tab once they are on the default
branch, `hust/phase12-memory`.

1. Review the commit, push it to `hust/phase12-memory`, and copy its full
   40-character SHA.
2. Open **Actions → Home RTX 3070 diagnostic → Run workflow**, with the branch
   set to `hust/phase12-memory`.
3. Fill in the form:
   - `commit_sha`: the full SHA.
   - `authorization`: `I AUTHORIZE ONE RTX 3070 RUN`.
   - The other fields: `console` (default `series-s`), `max_duration_seconds`
     (30–600, default 150), DRED, capture, label, Discord and
     `max_queue_minutes`.
4. Approve the `home-rtx3070` deployment if you set required reviewers.

With the GitHub CLI, the same dispatch is:

```powershell
gh workflow run home-rtx3070-diagnostic.yml --ref hust/phase12-memory -f commit_sha=<FULL_SHA> -f "authorization=I AUTHORIZE ONE RTX 3070 RUN" -f console=series-s -f max_duration_seconds=150
```

Only one hardware run executes at a time (concurrency group
`hust-rtx3070-hardware`). A second dispatch waits for the first to finish,
then must still pass the stale-queue guard. The harness's own
`hw-launch-history.json` and recovery interval also apply.

After the run, the job summary shows the classification and key metrics. The
`rtx3070-diagnostic-<run id>` artifact holds the sanitized bundle (14-day
retention). The full run directory stays in `HUST_VICTORY_RESULTS_ROOT`.

## Removing the runner

1. Set `HUST_RTX3070_RUNNER_REGISTERED` to `false`.
2. Stop `run.cmd`.
3. On **Settings → Actions → Runners → hust-rtx3070-home → Remove**, copy the
   removal command, then run it in the runner folder:
   `.\config.cmd remove --token <REMOVAL-TOKEN>`.
4. Delete `G:\actions-runner-hust`. This also removes its `.env`.

## Refusal messages

| Message | Meaning |
|---|---|
| `HUST_RTX3070_RUNNER_REGISTERED is not 'true'` | Step 4.1 is not done, or hardware runs are switched off. |
| `Commit ... is not on hust/phase12-memory` | Push or merge the reviewed commit first. |
| `No online runner labelled hust-rtx3070` | Start `run.cmd` (Step 5). |
| `waited ...s for the home runner` | The job was queued while the runner was offline. Start the runner and dispatch again. |
| `session 0 / non-interactive` | The runner is running as a service. Reconfigure it without the service and use `run.cmd`. |
| `Windows TDR is disabled` | Remove the `TdrLevel=0` override and reboot. |
| `Hardware safety check failed` | The selected commit weakens a harness guard or a workflow rule. Fix the commit. |
| Harness `Refusing launch: ...` | A harness guard: headroom, another instance, a recent display reset, or a missing executable. |
