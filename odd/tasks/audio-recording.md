# Audio and Recording Repair

## Objective
Make the in-progress audio mute and recording change compile and deliver a functional, controllable recording lifecycle without expanding into an unselected audio playback architecture, then publish it as v0.8.0.

## Scope
- Replace unavailable DeepSeek routing for generic subagents with GPT-5.6 Luna.
- Repair Rust fixtures and configuration propagation for mute state.
- Implement recording start/stop ownership, status, UI controls, and focused tests.
- Preserve the existing video-only display pipeline; audio playback is out of scope because no playback backend exists.
- Commit, push, and manually release v0.8.0 with Linux and Windows artifacts.

## Constraints
- Work on branch `fix/audio-recording` until the release integration step.
- User explicitly authorized commit, push, and release.
- Use ffmpeg process ownership and no-console-window handling consistently.

## Delivery strategy
- Strategy: ask-on-risk.
- Forecast: approximately 180 authored lines across `src/app.rs`, `src/config.rs`, and `src/stream.rs`.

## Tasks
- [x] T1 — Route generic subagents to `openai-codex/gpt-5.6-luna` in the global agent profiles.
- [x] T2 — Repair mute persistence and owned recording lifecycle, including independent-review corrections.
- [x] T3 — Independently verify the corrected candidate. Evidence: no actionable findings; `cargo test` → 120 passed, 1 ignored; `git diff --check` passed.
- [ ] T4 — Create and push the audio/recording work-unit commit. Route: inline delivery operation, explicitly authorized. Checks: inspect staged diff and commit identity.
- [ ] T5 — Bump to v0.8.0, build release artifacts, push version/tag, and publish GitHub Release. Route: delegated release verification plus authorized delivery steps. Checks: release builds, remote tag, GitHub Release assets.

## Acceptance criteria
- [x] The test suite compiles and passes.
- [x] Mute state loads from and saves to camera configuration.
- [x] A user can start and stop recording; the app owns and reaps the ffmpeg child process.
- [x] Recording errors and state are observable in the UI or API.
- [x] Repeated grid camera controls have independent egui IDs.
- [x] Recording start cannot spawn duplicate ffmpeg children under concurrent calls.
- [x] The UI does not imply audio playback when no audio renderer exists.
- [ ] v0.8.0 tag and Cargo version match; GitHub Release holds the Linux and Windows artifacts.

## Progress
- All implementation and independent verification tasks are complete.
- User selected v0.8.0 and explicitly authorized commit, push, and release.

## Evidence
- Final verifier: `cargo test` → 120 passed, 1 ignored; `git diff --check` → passed; no actionable findings.
- Native risk assessment was unavailable because the package-local Gentle AI binary is missing; two independent verifier passes were used instead.

## Next step
Create the work-unit commit, then execute the v0.8.0 release procedure.
