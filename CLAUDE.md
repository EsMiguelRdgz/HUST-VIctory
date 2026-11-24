# Claude Instructions for VIctory

Claude agents working in this repository must follow `AGENTS.md` in full. It is the
universal engineering protocol and takes precedence over Claude defaults.

In particular:

- Read `AGENTS.md` and all of `progress.md` before changing source code, and record
  recovery findings and the resume point in `progress.md` first.
- Keep `progress.md` (repository root) current after every significant finding, source
  change, test run, commit, or blocker; append a handoff entry for every session.
- Ask for explicit user confirmation immediately before every Git mutation, including
  each commit. Never use destructive Git operations, and never fetch, merge, rebase, or
  import upstream development.
- Never launch the game, another application, or a GPU workload automatically; leave
  exact manual validation instructions instead. Do not claim hardware performance or
  memory improvements without measurements from an authorized hardware run.
- Follow `docs/asset-policy.md`; do not modify proprietary binaries or bypass access
  controls.
