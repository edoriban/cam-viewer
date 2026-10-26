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
- [ ] T2 — Lot B: S6, S7, S8. Route: gentle-ai-worker (context backstop). Commit: —
  - S6 design: while `config_error` is set, Settings shows the camera list as locked (no rows, no + ADD CAMERA, SAVE disabled for camera changes) with the copy "Camera list is locked until the file loads." / "Fix the file and press RELOAD, or start a new list (your file is kept as cameras.toml.bak)."; START A NEW LIST unlocks editing for the session (saving then goes through the existing `save_guarded` backup). Badge position / update check stay editable only if saving them cannot write the camera list over the unreadable file — otherwise lock them too.
  - S7: CONNECTING tile center text and PAUSED message show only the state, never `cam.name`.
  - S8: raise `theme::ASH` and `ghost_text()` alpha so small text on SOOT/SOOT_2 reaches >= 4.5:1 (target around #9a9aa0); keep CONCRETE/PAPER unchanged.
- [ ] T3 — Lot C: S9, S10, S11, S12. Route: gentle-ai-worker (context backstop). Commit: —
  - S9: `segmented_row` selected = dark fill + red marker square + red bottom rule; unselected = transparent with CONCRETE border and ASH text; no paper fill or offset shadow on segments (those mean "action").
  - S10: sidebar summary with zero cameras reads "NO CAMERAS YET"; the separate CAMERAS list empty label must not duplicate it (drop or reword).
  - S11: welcome option cards end at the same right margin (18px) as grid tiles.
  - S12: welcome block centered vertically in the central panel.
- [ ] T4 — Runtime visual check (Xvfb) + ffprobe of a stopped recording. Route: inline.

## Log
- L1: "pasale ux pass y taste a la aplicación a ver qué se mejora" → "haz un artifact para poder ver tu propuesta de una manera mas visual" → decisions A, B, C; bug 01 = mp4-graceful; "vamos con esto".
