# UX pass

Implements the 12 findings of the UX review artifact (`.pi/artifacts/cam-viewer-ux-review.html`), all three lots approved.
Branch `feat/ux-pass`; work-unit commits per lot; push/PR/merge remain the user's decision.

## Specs
- S1 (01) "STOP deja un MP4 que no se puede abrir" — decision: "Seguir con MP4 y detener ffmpeg de forma limpia (q/SIGINT)". Stop sends `q` on stdin, waits with a bounded timeout, SIGKILL only as last resort; resulting file passes `ffprobe`.
- S2 (02) "La segunda grabación falla y la UI no lo muestra" — "Nombre con fecha y hora" (never collides), "Carpeta fija (`~/Videos/cam-viewer`…)", "Revisar el proceso hijo en cada frame" (UI returns to RECORD with the error if ffmpeg exits), "Contador `REC mm:ss` en rojo", and a "Saved · <path>" notice with OPEN FOLDER.
- S3 (03) "El nombre de la cámara tapa el chip de MUTE" — "El nombre queda solo a la izquierda. Los controles van juntos arriba a la derecha y se dibujan después del frame." Chip text names the action (UNMUTE when muted).
- S4 (04) "Sin RECORD ni MUTE mientras no haya stream."
- S5 (05) "grabar siempre el audio" — mute no longer drops audio from recordings.
- S6 (06) "Camera list is locked until the file loads. Fix the file and press RELOAD, or start a new list (your file is kept as cameras.toml.bak)."
- S7 (07) "en el centro solo va el estado ("PAUSED", "CONNECTING")" — no repeated camera name.
- S8 (08) raise `ghost_text` / `ASH` to at least ~#9a9aa0 for small text on dark.
- S9 (09) "Regla: claro + sombra = acción · marca roja = seleccionado · borde = disponible" — segmented controls.
- S10 (10) sidebar "0 SIGNALS · LOCAL ONLY" → "NO CAMERAS YET".
- S11 (11) welcome cards use the same 18px right margin as the grid.
- S12 (12) welcome block no longer leaves the lower half empty (centered vertically).

## Tasks
- [x] T1 — Lot A: S1, S2, S3, S4, S5 (recording lifecycle in stream.rs, tile controls in app.rs). Route: inline, then delegated (context backstop) to gentle-ai-worker. Commit: `785339b` (+ `e75b0ce` .gitignore).
  - Evidence: `cargo test` 167 lib passed + 6 + 1 passed, 1 ignored; clippy `-D warnings` clean; rustfmt clean on touched files. Parent added `report_recording_error` so folder-creation errors are shown (worker had left them swallowed).
  - Slice boundary: lot A (~1000 authored lines, ~half tests) is review slice 1; lots B+C form slice 2.
  - Done inline: `src/recordings.rs` (directory/prepare/file_name/unique_path, 6 tests green); RED tests in `src/stream.rs` mod `recording_tests` (compile-fail RED observed).
  - Design for the rest:
    - `RecordingState { Idle, Recording { path: PathBuf, started: Instant }, Saving { path: PathBuf }, Saved { path: PathBuf, at: Instant }, Failed(String) }`.
    - `recording_args(url, path) -> Vec<String>`: `-v error -n -rtsp_transport tcp -timeout <STREAM_IO_TIMEOUT µs> -i url -map 0:v:0 -map 0:a? -c:v copy -c:a aac -f mp4 path`; no `-nostdin`; independent of mute (S5).
    - `spawn_recorder(args, path) -> Result<Recorder, String>`: ffmpeg via `no_window_command`, stdin piped, stdout null, stderr piped and drained by a thread keeping the last non-empty line (never let the pipe fill).
    - `settle_exit(path, success, requested, stderr_tail) -> RecordingState`: unrequested exit → `Failed(tail)` or `Failed("recording stopped unexpectedly")`, even on status 0; requested → `Saved` if file exists and non-empty, else `Failed("nothing was recorded")` (tail appended if non-empty is fine only if tests still pass).
    - `finish_recording(recorder, timeout) -> RecordingState`: write `q`, drop stdin, poll `try_wait` until timeout, SIGKILL only as last resort (→ Failed mentioning the file may be incomplete), join stderr thread, then `settle_exit(.., requested=true, ..)`.
    - `StreamHandle::recording_state()` polls the child each frame and settles unrequested exits. `start_recording(path)` idempotent while running (keep `concurrent_recording_starts_publish_at_most_one_child`), returns Err without changing state while `Saving`. `stop_recording()` takes the recorder, sets `Saving`, finishes on a background thread. `stop()` (shutdown/Drop) finishes synchronously with ~3s timeout.
    - app.rs `video_surface`/`recording_control`: name alone top-left; MUTE + RECORD grouped top-right (left of the badge when the badge is TopRight), drawn AFTER the frame; name elided so it never reaches the controls; controls hidden unless the stream has a frame (S4) — but a recording that is Recording/Saving/Saved/Failed keeps its chip so it can be stopped/read; RECORD calls `recordings::prepare(&cam.name)`; Recording chip shows `REC mm:ss` in STATUS_OFFLINE; Saving shows `SAVING…` (not clickable); Saved shows `SAVED · <file name>` for ~8s and click opens the folder (`open_in_os_default_app` on the parent dir); Failed shows the error elided with full text on hover. Keep `mute_chip_label` (action wording).
- [x] T2 — Lot B: S6, S7, S8. Route: gentle-ai-worker (context backstop). Commit: `3baeebd`. Evidence: 176 lib passed; parent mutation check (settings_locked forced false) made 2 tests fail, incl. the on-disk save test. S7 has no dedicated test (UI text only).
  - S6 design: while `config_error` is set, Settings shows the camera list as locked (no rows, no + ADD CAMERA, SAVE disabled for camera changes) with the copy "Camera list is locked until the file loads." / "Fix the file and press RELOAD, or start a new list (your file is kept as cameras.toml.bak)."; START A NEW LIST unlocks editing for the session (saving then goes through the existing `save_guarded` backup). Badge position / update check stay editable only if saving them cannot write the camera list over the unreadable file — otherwise lock them too.
  - S7: CONNECTING tile center text and PAUSED message show only the state, never `cam.name`.
  - S8: raise `theme::ASH` and `ghost_text()` alpha so small text on SOOT/SOOT_2 reaches >= 4.5:1 (target around #9a9aa0); keep CONCRETE/PAPER unchanged.
- [x] T3 — Lot C: S9, S10, S11, S12. Route: gentle-ai-worker (context backstop). Commit: `ca425bd`. Evidence: 182 lib passed, each RED observed individually (S11 measured 1180 vs 1172 before fix).
  - S9: `segmented_row` selected = dark fill + red marker square + red bottom rule; unselected = transparent with CONCRETE border and ASH text; no paper fill or offset shadow on segments (those mean "action").
  - S10: sidebar summary with zero cameras reads "NO CAMERAS YET"; the separate CAMERAS list empty label must not duplicate it (drop or reword).
  - S11: welcome option cards end at the same right margin (18px) as grid tiles.
  - S12: welcome block centered vertically in the central panel.
- [x] T4 — Runtime visual check (Xvfb) + ffprobe of a stopped recording. Route: inline.
  - Xvfb 1280x720 after-shots: grid tiles show name only, no MUTE/RECORD while offline; config-error Settings locked with START A NEW LIST, file bytes unchanged, no .bak; welcome centred, second card right edge 1261 vs grid 1262; sidebar NO CAMERAS YET.
  - ffprobe: covered by `finishing_a_recording_leaves_a_file_ffprobe_can_read` (real ffmpeg, `q` stop, ffprobe reads video).
  - NOT verified: live video with controls over a frame, REC/SAVING/SAVED chips, unlocked segmented control — no RTSP server available locally.

- [x] T5 — Fix 4 defects from independent gentle-ai-verify (Discover bypassed S6 lock; Drop blocked UI; stalled input ignored `q`; already-exited recorder read SAVED) + parallel exit join. Route: gentle-ai-worker. Commit: see `git log` (fix(recording): never lose a recording...). Evidence: 187 lib passed, RED per defect observed by worker; SIGINT escalation tested against a silent TCP listener (FIFO open() is not interruptible by SIGINT).

- [x] T6 — Fix the advisory findings of native review `review-c4616168871cc301` in one separate commit. Route: gentle-ai-worker. Commit: `d385e8c`. Evidence: 200 lib passed, clippy and rustfmt clean; RED per finding observed by worker (F6 has no isolated test: try_wait I/O errors are not reproducible portably).
  - F1 (risk, stream.rs:457-464): ffmpeg stderr tail shown in the chip/hover may contain `rtsp://user:pass@host`; redact credentials in every recording error text before it reaches RecordingState.
  - F2 (reliability, app.rs:1591-1593): exit only joins finish threads of cameras still configured; finishes started by dropping a removed/edited camera are never awaited at exit.
  - F3 (reliability, app.rs:2343-2344): clicking a Failed chip starts a recording even when the stream has no frame; without a frame a click must dismiss the error (back to Idle), not start.
  - F4 (reliability, stream.rs:216-223): per-frame settle of an unrequested exit in `recording_state()` has no test.
  - F5 (reliability, app.rs:3209-3211): the S11 welcome-margin test duplicates the layout instead of measuring the real welcome view.
  - F6 (reliability, stream.rs:217-218): a `try_wait` error is ignored; it must settle as Failed.
  - F7 (reliability+resilience, stream.rs:458-464): an unrequested exit reports Failed but hides that a partial, possibly playable file exists; mention it (file name) when non-empty.
  - F8 (resilience, stream.rs:465-470): a requested stop reports SAVED from file size alone; also require a clean exit (status success, or the exit caused by our own SIGINT), else Failed mentioning the kept file.
  - F9 (resilience, stream.rs:444): stderr drain stops at the first invalid UTF-8 line; read bytes and decode lossily so the pipe is always drained.

- [x] T7 — Fix the 3 advisory findings of native review `review-78ea4c3a7b74c68d` in one separate commit. Route: gentle-ai-worker. Commit: `46790f8`. Evidence: 207 lib passed, clippy and rustfmt clean; RED per finding observed by worker.
  - G1 (risk, stream.rs:527-533 `redact_url_userinfo`): userinfo containing RFC 3986 sub-delims (`!$&'()*+,;=`), percent-encoded bytes or `:` in the password may not be fully redacted; redact everything between `scheme://` and the last `@` before the host's first `/`, `?`, `#` or whitespace.
  - G2 (reliability, app.rs:4135-4138 and other ffmpeg-dependent tests): tests return early and pass when ffmpeg is missing; make the skip explicit (`#[ignore]` with a reason, or fail unless an opt-out env var is set) so a missing ffmpeg is never reported as a pass.
  - G3 (reliability, stream.rs:221-225): when an exit is settled, the stderr tail can be read before the drain thread has stored ffmpeg's last line; join the drain thread (bounded) before reading the tail on every settle path.

## Release
- `feat/ux-pass` fast-forwarded onto `main`; README documents recording location and states (`851de1c`); `aeaca34 chore(release): bump version to 0.11.0` pushed to `origin/main`.
- Annotated tag `v0.11.0` pushed; GitHub Release https://github.com/edoriban/cam-viewer/releases/tag/v0.11.0 with Linux and Windows x86_64 artifacts. Native review of the release candidate approved (readability, 6 advisory findings).
- v0.11.0 installed locally at `~/.local/bin/cam-viewer` from the published asset.
- Not verified: recording chips and controls over live video (no RTSP source here). Windows cross-build emits one `unused_mut` warning on a non-unix path.

## Log
- L1: "pasale ux pass y taste a la aplicación a ver qué se mejora" → "haz un artifact para poder ver tu propuesta de una manera mas visual" → decisions A, B, C; bug 01 = mp4-graceful; "vamos con esto".
- L2: independent verify (S1 partial, S2 partial, S5 pass, S6 fail) → defects fixed in T5.
- L3: native review approved with 10 advisory findings; user: "corrige los problemas enun commit aparte".
- L4: second native review approved with 3 advisory findings; user: "sí, corrigelas".
- L5: "ok entonces con estos fixes hacemos la nueva versión" → chose v0.11.0.
