# Entry UX

## Objective
Improve what a person sees from launch until live video: never lose a config the app could not read, guide first-run users to Discover, and make camera connection state legible and actionable.

## Problem
- A malformed `cameras.toml` is reported only on stderr; the app opens an empty Settings view and SAVE overwrites the user's file with an empty list.
- First run opens an empty Settings form; Discover is only offered on the Grid empty state.
- Offline tiles show no attempt count, no next-retry countdown and no manual retry.
- "connecting…" and "waiting for frames…" render identically.
- The sidebar header counts configured cameras, not working ones.

## Why
UX pass published at https://claude.ai/artifact/4RHTdmE23w8tzbB9FyZ5GS; user asked to implement all five findings.

## Scope
- In: findings 01–05 from the UX pass.
- Out: README "auto-pause on focus loss" (`set_paused` never called) — noted, not authorized.

## Constraints
- Stay inside the existing theme tokens (`src/theme.rs`); no new palette or type scale.
- TDD: strict (source: global CLAUDE.md "Strict TDD Mode: enabled"); runner `cargo test`.
- Work-unit commits on `feat/entry-ux`; push/PR/merge remain the user's decision.

## Tasks
- [x] T1 — Config load error is surfaced in-app (banner: path, message, OPEN FILE, RELOAD) and SAVE cannot overwrite an unreadable file (backup to `cameras.toml.bak` before writing). Route: delegated direct (writer trigger: main.rs, app.rs, config.rs).
  - Commit `c7bfb27`. `cargo test` 119 lib + 6 discovery_scan + 1 ffmpeg_pipe passed, 1 live_probe ignored (baseline: 120 passed/1 ignored). `cargo clippy --all-targets -- -D warnings` clean. `cargo fmt --check` clean on touched files (app.rs, config.rs, main.rs); repo-wide fmt has pre-existing diffs in untouched files (stream.rs, theme.rs, update.rs, discover/probe.rs), left alone.
- [x] T2 — First-run welcome view: primary DISCOVER, secondary ADD CAMERA; reused as Grid empty state. Route: delegated direct (app.rs + tests).
  - Commit `a1858c0`. `cargo test` 119 lib passed (same as T1 baseline; no new testable branches beyond T1's `initial_view`). `cargo clippy --all-targets -- -D warnings` clean. `cargo fmt --check` clean for app.rs.
- [x] T3 — Stream exposes attempt count, next-retry instant and a retry-now wake; tiles show phase band (reach · stream · live) with elapsed time, offline attempt/countdown and RETRY NOW; sidebar summary `N/M ONLINE · x UP · y OFF`. Route: delegated direct (stream.rs, app.rs, theme.rs).
  - Commit `7d2b6ea`. `cargo test` 132 lib passed (119 + 13 new: 7 in stream.rs, 6 in app.rs), 6 discovery_scan + 1 ffmpeg_pipe passed, 1 live_probe ignored. `cargo clippy --all-targets -- -D warnings` clean. `cargo fmt --check` clean on touched files (app.rs, stream.rs, theme.rs).
  - Correction `ca21665` (review R3-online-on-spawn): status stays Connecting until the first frame of each attempt; `has_frame` resets per attempt. The "stream open" phase is not observable (streaming ffmpeg stderr is discarded), so the band no longer claims it from a spawn. `cargo test` 135 lib + 6 + 1 passed, 1 ignored.
  - Superseded deviation: the "reach" phase ends as soon as the ffmpeg child process spawns (not once RTSP negotiation succeeds) since ffmpeg's stderr is discarded and not parsed for progress; the "reach" segment is near-instant and "stream" covers most of the real wait. Documented in the commit body.

## Acceptance criteria
- [x] Parse error → file bytes unchanged until user saves; saving keeps the original as `.bak` (unit-tested). Banner visibility: not verified at runtime.
- [ ] RELOAD re-reads the config without restarting (implemented; no test, not verified at runtime).
- [x] Zero cameras at launch → Grid with the shared welcome view (initial_view unit-tested). Visual: not verified at runtime.
- [x] Attempt count, retry deadline and retry_now wake unit-tested. Tile rendering: not verified at runtime.
- [x] Connecting vs. no-frame fallback distinct in `tile_band` (unit-tested); "stream open" intentionally not claimed without evidence.
- [x] Sidebar header reports online/connecting/offline counts (`sidebar_summary` unit-tested).
- [x] `cargo test` passes (135 lib + 6 + 1, 1 ignored); `cargo clippy --all-targets -- -D warnings` clean.

## Delivery
- Forecast: ~500–650 authored changed lines (> 400 budget). Strategy: ask-on-risk → chain strategy `stacked-to-main` (user choice).
- Slices: one PR per task (T1, T2, T3), each targeting main, merged in order.

## Review record (RDD on, global)
- T1 `c7bfb27` — assessed high (process_boundary: OPEN FILE spawns a process). Consent granted. 4-lens review (lineage `review-d475b192aaf5b51d`) **approved**; acknowledged, authority burned. Reviewed boundary advanced to `c7bfb27`.
- T2 `a1858c0` — assessed medium, `under_budget` (117 lines); pending in slice with T3.
- T2+T3 range `c7bfb27..7d2b6ea` — medium, `slice_budget_reached` (728 lines). Consent granted. 1-lens review (lineage `review-5f9f3e363664ae47`) → `correction_required`, one blocking finding. Correction budget submitted: 120 lines. Last status: `stop corrected_candidate_unavailable` → make the correction commit, then re-query the exact status below.

- Correction `ca21665` committed; resume required maintainer-authorized recovery (`scope_changed`, actor Edoriban, user-approved 2026-09-23) → successor lineage `review-entryux-t23-recovery`, 1-lens review **approved**; acknowledged, authority burned. Reviewed boundary advanced to `ca21665`.

### Non-blocking advisory findings (follow-ups, not part of this review)
- T1: `.bak` is overwritten by a later backup (config.rs:99-104) — use unique/timestamped names or refuse overwrite.
- T1: OPEN FILE failures are silent (app.rs:1042-1061); Windows path goes through `cmd /C start` — prefer `explorer.exe`.
- T1: backup-abort test doesn't discriminate (config.rs:298-314); app-level save guard and RELOAD lack tests (app.rs:1265-1285).
- T3 fix: regression test calls a no-op helper, so it would not catch an Online assignment re-added inside `run_loop` (stream.rs:972-986).
- T3: `retry_wake` is only cleared at the end of `sleep_interruptible`; a RETRY NOW click after the sleep returns zeroes the next backoff (stream.rs:315).
- T1: dead `initial_view` param (app.rs:1115-1124), duplicated CREATE_NO_WINDOW const, a successful backup isn't reported to the user.

## Environment notes
- `.codegraph/` added to `.git/info/exclude` (local only) so review inventory has no untracked files.
- Engram mirror `odd/entry-ux/tasks`: saved via CLI as #2560 (MCP save failed: multiple active sessions).

## Progress
- Branch `feat/entry-ux` from `main` @ 8c9cb8b. Commits: `c7bfb27` (T1), `a1858c0` (T2), `7d2b6ea` (T3), `ca21665` (T3 review correction), plus docs commits.
- All native reviews closed with approved, acknowledged receipts.
- `cargo test` at `ca21665`: 135 lib + 6 + 1 passed, 1 ignored. Clippy clean.

## Next step
1. Run the app once to visually verify the banner, welcome view, phase band and offline tile (never verified at runtime).
2. Slice PRs per `stacked-to-main` (T1 = `c7bfb27`; T2 = `a1858c0`; T3 = `7d2b6ea` + `ca21665`) — push/PR is the user's decision.
3. Optional follow-ups: the advisory findings above.
