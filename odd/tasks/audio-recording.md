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
- User explicitly authorized commit, push, and release.
- Use ffmpeg process ownership and no-console-window handling consistently.

## Tasks
- [x] T1 — Route generic subagents to GPT-5.6 Luna.
- [x] T2 — Repair mute persistence and owned recording lifecycle, including independent-review corrections.
- [x] T3 — Independently verify the corrected candidate. Evidence: no actionable findings; `cargo test` → 120 passed, 1 ignored; `git diff --check` passed.
- [x] T4 — Create and push the audio/recording work-unit commit. Evidence: `ce4fead feat(recording): add controlled camera capture`, pushed as `origin/fix/audio-recording`, then fast-forwarded locally onto `main`.
- [ ] T5 — Bump to v0.8.0, build release artifacts, push version/tag, and publish GitHub Release. Route: delegated version metadata repair plus authorized delivery steps. Checks: release builds, remote tag, GitHub Release assets.

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
- Work-unit commit `ce4fead` created and pushed.
- v0.8.0 metadata update is in progress.

## Next step
Complete the Cargo metadata bump, run release builds, and publish the annotated tag and GitHub Release.
