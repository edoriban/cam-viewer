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
- [ ] T3 — (reopened: review correction pending) Stream exposes attempt count, next-retry instant and a retry-now wake; tiles show phase band (reach · stream · live) with elapsed time, offline attempt/countdown and RETRY NOW; sidebar summary `N/M ONLINE · x UP · y OFF`. Route: delegated direct (stream.rs, app.rs, theme.rs).
  - Commit `7d2b6ea`. `cargo test` 132 lib passed (119 + 13 new: 7 in stream.rs, 6 in app.rs), 6 discovery_scan + 1 ffmpeg_pipe passed, 1 live_probe ignored. `cargo clippy --all-targets -- -D warnings` clean. `cargo fmt --check` clean on touched files (app.rs, stream.rs, theme.rs).
  - Deviation: the "reach" phase ends as soon as the ffmpeg child process spawns (not once RTSP negotiation succeeds) since ffmpeg's stderr is discarded and not parsed for progress; the "reach" segment is near-instant and "stream" covers most of the real wait. Documented in the commit body.

## Acceptance criteria
- [ ] Parse error → banner visible; file bytes unchanged until user saves; saving keeps the original as `.bak`.
- [ ] RELOAD re-reads the config without restarting.
- [ ] Zero cameras at launch → welcome view with DISCOVER as primary action; Grid empty state uses the same view.
- [ ] Offline tile shows attempt number and seconds to next try; RETRY NOW triggers an immediate reconnect.
- [ ] Connecting vs. stream-open-no-frames are visually distinct.
- [ ] Sidebar header reports online/connecting/offline counts.
- [ ] `cargo test` passes; `cargo clippy` clean for touched code.

## Delivery
- Forecast: ~500–650 authored changed lines (> 400 budget). Strategy: ask-on-risk → chain strategy `stacked-to-main` (user choice).
- Slices: one PR per task (T1, T2, T3), each targeting main, merged in order.

## Review record (RDD on, global)
- T1 `c7bfb27` — assessed high (process_boundary: OPEN FILE spawns a process). Consent granted. 4-lens review (lineage `review-d475b192aaf5b51d`) **approved**; acknowledged, authority burned. Reviewed boundary advanced to `c7bfb27`.
- T2 `a1858c0` — assessed medium, `under_budget` (117 lines); pending in slice with T3.
- T2+T3 range `c7bfb27..7d2b6ea` — medium, `slice_budget_reached` (728 lines). Consent granted. 1-lens review (lineage `review-5f9f3e363664ae47`) → `correction_required`, one blocking finding. Correction budget submitted: 120 lines. Last status: `stop corrected_candidate_unavailable` → make the correction commit, then re-query the exact status below.

### Blocking finding to fix (R3-online-on-spawn, CRITICAL, introduced by 7d2b6ea)
`run_loop` (src/stream.rs ~514-519) sets `Status::Online` as soon as ffmpeg spawns; spawn does not prove the camera is reachable, so an unreachable camera reads Online for the whole 10 s I/O timeout of every attempt. `has_frame` is never cleared, so after one successful frame `badge_display` (src/app.rs ~2244-2250) and the sidebar tally (~1579-1580) report ONLINE during every reconnect.
Fix plan: status stays `Connecting` from `begin_attempt` until the first frame of that attempt; clear `has_frame` in `begin_attempt`/backoff; "STREAM OPEN · NO FRAMES YET" only if backed by real evidence (streaming ffmpeg stderr is `Stdio::null()` ~503), otherwise reduce the band so spawn never claims "stream open"; fix the misleading comment ~516-517. TDD RED first. Commit: `fix(stream): keep cameras connecting until a frame arrives`. ≤120 lines.

### Resume the review after the correction commit
Run exactly (from repo root, on `feat/entry-ux`):
`gentle-ai review status --cwd=/home/edoriban/projects/eo/cam-viewer --contract=gentle-ai.review-integration/v2 --next-transition=true --lineage=review-5f9f3e363664ae47 --agent=claude-code --base-ref=e7d05a6b8d6d4f414f2e4ef744a2385a1277dc91 --committed-only=true`
then follow only its `next_transition` (targeted validator → approval → exact acknowledgement).

### Non-blocking advisory findings (follow-ups, not part of this review)
- T1: `.bak` is overwritten by a later backup (config.rs:99-104) — use unique/timestamped names or refuse overwrite.
- T1: OPEN FILE failures are silent (app.rs:1042-1061); Windows path goes through `cmd /C start` — prefer `explorer.exe`.
- T1: backup-abort test doesn't discriminate (config.rs:298-314); app-level save guard and RELOAD lack tests (app.rs:1265-1285).
- T1: dead `initial_view` param (app.rs:1115-1124), duplicated CREATE_NO_WINDOW const, a successful backup isn't reported to the user.

## Environment notes
- `.codegraph/` added to `.git/info/exclude` (local only) so review inventory has no untracked files.
- Engram mirror `odd/entry-ux/tasks`: saved via CLI as #2560 (MCP save failed: multiple active sessions).

## Progress
- Branch `feat/entry-ux` from `main` @ 8c9cb8b. Commits: `c7bfb27` (T1), `a1858c0` (T2), `7d2b6ea` (T3), `59f295c` (docs).
- `cargo test` at `59f295c`: 132 lib + 6 + 1 passed, 1 ignored (baseline 120/1). Clippy clean.
- Paused by the user on 2026-09-22 before the correction was written; working tree clean.

## Next step
1. Implement the R3-online-on-spawn correction (above) with TDD and commit it.
2. Re-run the exact review status above and finish the review (validator → acknowledge).
3. Run the app once to visually verify the banner, welcome view, phase band and offline tile (never verified at runtime).
4. Tick acceptance criteria; then slice PRs per `stacked-to-main` (T1, T2, T3) — push/PR is the user's decision.
5. Keep the Engram mirror (#2560, topic `odd/entry-ux/tasks`) in sync.
