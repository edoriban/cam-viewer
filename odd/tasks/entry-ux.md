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
- [ ] T1 — Config load error is surfaced in-app (banner: path, message, OPEN FILE, RELOAD) and SAVE cannot overwrite an unreadable file (backup to `cameras.toml.bak` before writing). Route: delegated direct (writer trigger: main.rs, app.rs, config.rs).
- [ ] T2 — First-run welcome view: primary DISCOVER, secondary ADD CAMERA; reused as Grid empty state. Route: delegated direct (app.rs + tests).
- [ ] T3 — Stream exposes attempt count, next-retry instant and a retry-now wake; tiles show phase band (reach · stream · live) with elapsed time, offline attempt/countdown and RETRY NOW; sidebar summary `N/M ONLINE · x UP · y OFF`. Route: delegated direct (stream.rs, app.rs, theme.rs).

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

## Progress
- Branch `feat/entry-ux` created from `main` @ 8c9cb8b.

## Next step
Resolve delivery chain strategy, then T1.
