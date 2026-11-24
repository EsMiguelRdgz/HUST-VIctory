# VIctory Agent Engineering Protocol

These instructions are mandatory for every development agent. The repository, its local Git history, this file, and `progress.md` are the shared source of truth; do not rely on another agent's private conversation history.

## Session recovery

Before changing source code:

1. Read this file and all of `progress.md`.
2. Inspect the current branch, Git status, recent local commits, staged and unstaged changes, and untracked files.
3. Review work since the latest verified checkpoint and recover incomplete work before starting new implementation.
4. Respect the current milestone and subphase recorded in `progress.md`.
5. Preserve completed implementations, prior investigation reports, historical failures, and rollback capability. Do not discard or overwrite another agent's work.
6. Record recovery findings and the exact resume point in `progress.md` before implementation.

Never use destructive Git operations such as reset, clean, forced checkout, or history rewriting to make a working tree convenient. Do not fetch, merge, rebase, cherry-pick, or otherwise import upstream development. This is an independently maintained local branch; use the local codebase, public specifications, documented APIs, and independently authored code. Preserve licensing and attribution.

## Continuous checkpoints

`progress.md` is canonically located at the repository root. Keep its Current Project Status, Active Task, Immediate Resume Instructions, Completed Tasks, Incomplete Tasks, and Agent Handoff History current.

Update it before a new task or substantial architectural change; after a significant finding, source change, test run, commit, or blocker; and at least every 10 minutes during active implementation where practical. An agent may stop without warning, so checkpoints must describe the actual state rather than wait for milestone completion.

For every task, distinguish NOT_STARTED, IN_PROGRESS, IMPLEMENTED_UNVERIFIED, VERIFIED, BLOCKED, FAILED, and DEFERRED accurately. Record commands and actual results. Never relabel a historical failure as a success without recording the fix and subsequent verification. Append a new Agent Handoff History entry for every session and preserve earlier entries.

## Implementation and verification

- Make minimal, modular changes that follow existing project style and address the active milestone.
- Preserve required synchronization, security controls, licensing, asset-policy rules, and authorized-use boundaries.
- Do not modify proprietary binaries, extract protected content, bypass DRM or platform controls, or add proprietary assets. Follow `docs/asset-policy.md`.
- Add or update independently authored regression tests where the project has applicable tests.
- Run focused tests after meaningful changes. Before declaring Phase 10 complete, run the full verification commands listed in `progress.md` and record their actual outcomes.
- Check formatting only for modified files; do not reformat unrelated code.
- Never automatically launch the game, another application, or a physical GPU workload. Hardware and authorized-workload validation must be left as exact manual instructions unless the user explicitly requests a specific launch.
- Do not claim hardware performance or memory improvements without measurements from an actual authorized hardware run.

## Git checkpoints and handoff

Create focused, descriptive, incremental commits for meaningful verified changes, with related regression tests and without unrelated files, secrets, or proprietary assets. Standing authorization (granted by the user on 2026-11-23 for Phase 10): agents may create incremental local commits without asking for each one, whenever a meaningful implementation has passed its applicable tests. Record every such commit (hash, scope, tests run) in `progress.md`. This authorization covers only ordinary local commits on the current branch. Explicit user authorization is still required immediately before any destructive Git operation (reset, clean, forced checkout, amend or other history rewriting, branch deletion), any remote push, and any major scope change. If work is interrupted before verification, document the exact uncommitted state in `progress.md`.

Before ending a session, leave actionable resume instructions: first action, next implementation step, pending tests, blockers, required commands, known regressions, remaining risks, and the location of any diagnostic executable. Preserve enough information for the next agent to resume using only the repository and its local history.
