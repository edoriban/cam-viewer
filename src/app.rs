use crate::config::{self, BadgePosition, CameraConfig, Config};
use crate::discover::net::{self, InterfaceInfo};
use crate::discover::probe::DISCOVER_PROBE_TIMEOUT;
use crate::discover::scan::DEFAULT_PORTS;
use crate::discover::{
    RowStatus, DiscoveryConfig, DiscoveryHandle, DiscoveryResult, DiscoverySnapshot, Phase,
};
use crate::stream::{Status, StreamHandle};
use crate::theme::{self, BtnVariant};
use crate::update;
use eframe::egui;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

const REPAINT_INTERVAL: Duration = Duration::from_millis(50);
const SIDEBAR_WIDTH: f32 = 236.0;
const GRID_MIN_TILE_WIDTH: f32 = 360.0;
const GRID_SPACING: f32 = 12.0;

enum View {
    Grid,
    Solo(usize),
    Discover,
    Settings,
}

struct CameraView {
    name: String,
    url: String,
    stream: StreamHandle,
    texture: Option<(u64, egui::TextureHandle)>,
}

struct SettingsRow {
    name: String,
    url: String,
}

pub struct SettingsEditor {
    rows: Vec<SettingsRow>,
    badge_position: BadgePosition,
    update_check: bool,
    /// Row the user asked to delete, applied at the start of the next frame.
    /// The footer is a bottom panel and therefore draws before the row list,
    /// so the deletion has to already be applied when `is_dirty()` is asked,
    /// or SAVE would stay disabled for a frame after removing a camera.
    pending_delete: Option<usize>,
    error: Option<String>,
    original: Config,
    /// First CANCEL/Escape while dirty arms this instead of discarding
    /// immediately; a second one confirms. Prevents a reflex Escape from
    /// silently losing an edit.
    confirm_discard: bool,
}

impl SettingsEditor {
    fn from_config(config: &Config) -> Self {
        Self {
            rows: config
                .cameras
                .iter()
                .map(|cam| SettingsRow {
                    name: cam.name.clone(),
                    url: cam.url.clone(),
                })
                .collect(),
            badge_position: config.badge_position,
            update_check: config.update_check,
            pending_delete: None,
            error: None,
            original: config.clone(),
            confirm_discard: false,
        }
    }

    /// SAVE is disabled while every editable field still matches what the
    /// editor was opened with, so a no-op save can't restart streams.
    fn is_dirty(&self) -> bool {
        self.badge_position != self.original.badge_position
            || self.update_check != self.original.update_check
            || self.collect() != self.original.cameras
    }

    /// Removes the row queued for deletion by the previous frame.
    ///
    /// Deletion is deferred because the footer is a bottom panel and draws
    /// before the row list: applying it here keeps `is_dirty()` and the rows
    /// consistent, so SAVE lights up in the same frame the row disappears.
    /// A stale index is dropped rather than panicking, since rows can also be
    /// removed by a config reload between frames.
    fn apply_pending_delete(&mut self) {
        if let Some(index) = self.pending_delete.take()
            && index < self.rows.len()
        {
            self.rows.remove(index);
        }
    }

    fn add_row(&mut self) {
        self.rows.push(SettingsRow {
            name: format!("Cam {}", self.rows.len() + 1),
            url: String::new(),
        });
    }

    fn collect(&self) -> Vec<CameraConfig> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let url = row.url.trim();
                if url.is_empty() {
                    return None;
                }
                let name = row.name.trim();
                Some(CameraConfig {
                    name: if name.is_empty() {
                        format!("Cam {}", i + 1)
                    } else {
                        name.to_owned()
                    },
                    url: url.to_owned(),
                })
            })
            .collect()
    }
}

enum SettingsAction {
    None,
    Cancel,
    Save,
}

// ---------------------------------------------------------------------------
// Discover wizard (REQ-12…REQ-15, REQ-10 rows, REQ-6 hint)
// ---------------------------------------------------------------------------

/// One results row: the discovery outcome plus UI bookkeeping. `duplicate`
/// marks any collision with the configured cameras; `exact_duplicate` marks
/// the strict exact-string-URL subset that is excluded from ADD SELECTED.
struct DiscoverRow {
    result: DiscoveryResult,
    checked: bool,
    duplicate: bool,
    exact_duplicate: bool,
    /// Path typed by the user for a `PathUnknown` host. Preserved across
    /// frames by `reconcile_rows`, which only refreshes `result`.
    manual_path: String,
    /// Configured cameras this host could be: same stream, different address,
    /// and their old address answered nothing this scan. Recomputed each
    /// frame from the live results.
    relink_options: Vec<String>,
    /// Chosen camera to re-point at this address instead of adding a new one.
    relink_to: Option<String>,
    /// Camera currently holding this address, which a relink would displace
    /// into the address being vacated. Names the other half of a swap.
    relink_swaps_with: Option<String>,
}

impl DiscoverRow {
    fn new(result: DiscoveryResult) -> Self {
        Self {
            result,
            checked: false,
            duplicate: false,
            exact_duplicate: false,
            manual_path: String::new(),
            relink_options: Vec::new(),
            relink_to: None,
            relink_swaps_with: None,
        }
    }

    /// URL this row would contribute: the probed one, or one assembled from a
    /// hand-typed path when the host speaks RTSP but matched no known path.
    ///
    /// Global credentials are deliberately not injected here: a host that
    /// needed them would have answered BadCredentials and be
    /// `NeedsCredentials`, not `PathUnknown`.
    fn effective_url(&self) -> Option<String> {
        if let Some(url) = &self.result.url {
            return Some(url.clone());
        }
        if self.result.auth != RowStatus::PathUnknown {
            return None;
        }
        let path = normalized_path(&self.manual_path)?;
        Some(format!(
            "rtsp://{}:{}{}",
            self.result.ip, self.result.port, path
        ))
    }

    /// Enabled-checkbox semantics: a row is actionable once it can name a URL
    /// and either is not already configured, or is being relinked. For
    /// `PathUnknown` that means the user has typed a usable path.
    ///
    /// An already-configured address stays actionable when a relink is chosen:
    /// pointing a camera at an address another camera already holds is exactly
    /// how a swapped pair of names gets corrected.
    fn addable(&self) -> bool {
        self.effective_url().is_some() && (!self.exact_duplicate || self.relink_to.is_some())
    }

    fn key(&self) -> String {
        result_key(&self.result)
    }
}

fn result_key(result: &DiscoveryResult) -> String {
    result.url.clone().unwrap_or_else(|| result.ip.to_string())
}

/// Authority host of an rtsp/http URL: scheme stripped, userinfo cut at the
/// LAST '@' (passwords may contain '@'), port cut at ':'.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host_port.starts_with('[') {
        host_port
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// The wizard's single global credential pair: trimmed; present when a
/// username is given (blank passwords stay legal as `user:` in URLs).
fn global_creds(username: &str, password: &str) -> Option<(String, String)> {
    let username = username.trim();
    (!username.is_empty()).then(|| (username.to_owned(), password.trim().to_owned()))
}

/// (host, port, path) for an rtsp/http URL, with a missing port defaulting
/// to RTSP's 554 so `host/path` and `host:554/path` compare as the same
/// camera (a manually-configured URL usually omits the default port while
/// discovery always states it explicitly).
fn url_identity(url: &str) -> Option<(String, u16, String)> {
    let rest = url.split_once("://")?.1;
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = if host_port.starts_with('[') {
        (host_port.to_owned(), None)
    } else {
        match host_port.split_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().ok()),
            None => (host_port.to_owned(), None),
        }
    };
    (!host.is_empty()).then(|| (host, port.unwrap_or(554), path.to_owned()))
}

/// Same camera per `url_identity`, not necessarily the same URL string.
fn same_camera_url(a: &str, b: &str) -> bool {
    url_identity(a).is_some_and(|ia| url_identity(b) == Some(ia))
}

/// Existing list plus extras, dropping any extra that identifies the same
/// camera as an existing one or an earlier extra (belt-and-braces dedup,
/// same equality `apply_cameras` uses for stream reuse).
fn merge_cameras(existing: &[CameraConfig], extra: Vec<CameraConfig>) -> Vec<CameraConfig> {
    let mut merged = existing.to_vec();
    for cam in extra {
        if !merged.iter().any(|m| same_camera_url(&m.url, &cam.url)) {
            merged.push(cam);
        }
    }
    merged
}

/// Configured cameras that this discovery result plausibly *is*: same port
/// and path, a different address, and whose own address answered nothing in
/// this scan.
///
/// Deliberately returns every match rather than picking one. Two identical
/// cameras that both changed address produce two candidates each, and
/// guessing there would silently swap which physical camera is called what.
fn relink_candidates(
    result: &DiscoveryResult,
    existing: &[CameraConfig],
    discovered_ips: &[Ipv4Addr],
) -> Vec<String> {
    let Some(url) = &result.url else {
        return Vec::new();
    };
    let Some((_, port, path)) = url_identity(url) else {
        return Vec::new();
    };
    existing
        .iter()
        .filter(|cam| {
            let Some((host, cam_port, cam_path)) = url_identity(&cam.url) else {
                return false;
            };
            if cam_port != port || cam_path != path {
                return false;
            }
            let Ok(cam_ip) = host.parse::<Ipv4Addr>() else {
                return false; // a hostname-based URL is not an address move
            };
            cam_ip != result.ip && !discovered_ips.contains(&cam_ip)
        })
        .map(|cam| cam.name.clone())
        .collect()
}

/// Re-points chosen cameras at their rediscovered address, keeping name and
/// position. Returns how many were moved.
fn apply_relinks(existing: &mut [CameraConfig], rows: &[DiscoverRow]) -> usize {
    let mut moved = 0;
    for row in rows.iter().filter(|row| row.checked) {
        let (Some(name), Some(url)) = (row.relink_to.as_ref(), row.effective_url()) else {
            continue;
        };
        let Some(target) = existing.iter().position(|cam| &cam.name == name) else {
            continue;
        };
        let vacated = std::mem::replace(&mut existing[target].url, url.clone());
        // If another camera already held this address, the two names were
        // swapped: hand it the address this one just left, so the pair trades
        // places instead of both pointing at the same camera.
        if let Some(other) = existing
            .iter()
            .position(|cam| &cam.name != name && same_camera_url(&cam.url, &url))
        {
            existing[other].url = vacated;
        }
        moved += 1;
    }
    moved
}

/// A hand-typed RTSP path made usable: trimmed and forced to a single leading
/// slash. `None` for input that names no path, including a bare `/`, which the
/// vendor table already probed and which therefore cannot be the answer here.
fn normalized_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return None;
    }
    let without_slashes = trimmed.trim_start_matches('/');
    if without_slashes.is_empty() {
        return None;
    }
    Some(format!("/{without_slashes}"))
}

/// `{vendor} {ip}`, falling back to the sequential `Cam N` convention.
fn discovered_name(vendor: Option<&str>, ip: Ipv4Addr, fallback: usize) -> String {
    match vendor {
        Some(vendor) => format!("{vendor} {ip}"),
        None => format!("Cam {fallback}"),
    }
}

/// CameraConfig values built from checked addable rows; numbering continues
/// after the existing cameras, matching the Settings `Cam N` convention.
fn selected_cameras(rows: &[DiscoverRow], existing_count: usize) -> Vec<CameraConfig> {
    rows.iter()
        .filter(|row| row.checked && row.addable() && row.relink_to.is_none())
        .enumerate()
        .map(|(i, row)| CameraConfig {
            name: discovered_name(
                row.result.vendor.as_deref(),
                row.result.ip,
                existing_count + i + 1,
            ),
            url: row.effective_url().unwrap_or_default(),
        })
        .collect()
}

/// Reconcile wizard rows against the latest snapshot results keyed by URL
/// (IP fallback for not-yet-authenticated hosts): checkbox state preserved
/// across frames, new rows default unchecked, duplicate layers marked, and
/// vanished results dropped. Transport-failure hosts never appear here by
/// upstream contract (`probe_pool` only emits qualifying results).
fn reconcile_rows(
    rows: &mut Vec<DiscoverRow>,
    results: &[DiscoveryResult],
    existing: &[CameraConfig],
) {
    let discovered_ips: Vec<Ipv4Addr> = results.iter().map(|result| result.ip).collect();
    for result in results {
        let key = result_key(result);
        match rows.iter_mut().find(|row| row.key() == key) {
            Some(row) => row.result = result.clone(),
            None => {
                let exact_duplicate = result
                    .url
                    .as_deref()
                    .is_some_and(|url| existing.iter().any(|cam| same_camera_url(&cam.url, url)));
                let ip = result.ip.to_string();
                let duplicate = exact_duplicate
                    || existing
                        .iter()
                        .any(|cam| url_host(&cam.url).as_deref() == Some(ip.as_str()));
                let mut row = DiscoverRow::new(result.clone());
                row.duplicate = duplicate;
                row.exact_duplicate = exact_duplicate;
                rows.push(row);
            }
        }
    }
    rows.retain(|row| results.iter().any(|result| result_key(result) == row.key()));

    for row in rows.iter_mut() {
        let was_empty = row.relink_options.is_empty();
        row.relink_options = relink_candidates(&row.result, existing, &discovered_ips);
        // A choice that no longer matches anything must not silently persist.
        if row
            .relink_to
            .as_ref()
            .is_some_and(|name| !row.relink_options.contains(name))
        {
            row.relink_to = None;
        }
        // One unambiguous candidate is offered pre-selected; more than one is
        // left to the user, because picking would risk swapping identities.
        if was_empty && row.relink_to.is_none() && row.relink_options.len() == 1 {
            row.relink_to = row.relink_options.first().cloned();
        }
        row.relink_swaps_with = match (&row.relink_to, row.effective_url()) {
            (Some(chosen), Some(url)) => existing
                .iter()
                .find(|cam| &cam.name != chosen && same_camera_url(&cam.url, &url))
                .map(|cam| cam.name.clone()),
            _ => None,
        };
    }
}

/// Wizard state machine (REQ-13): `handle.is_some()` is Scanning, non-empty
/// `rows` without a handle is Results, everything else Idle.
struct DiscoverWizard {
    interfaces: Vec<InterfaceInfo>,
    selected_iface: usize,
    username: String,
    password: String,
    handle: Option<DiscoveryHandle>,
    latest: Option<DiscoverySnapshot>,
    rows: Vec<DiscoverRow>,
    error: Option<String>,
    cancelling: bool,
    /// WS-Discovery hint inputs (found, degraded), set once past Configuring.
    ws: Option<(u32, bool)>,
    /// The single live preview, if any. Deliberately one at a time: each one
    /// is an ffmpeg process piping full-resolution frames, so previewing every
    /// row at once would cost more than the whole scan.
    preview: Option<Preview>,
}

/// A live look at one discovered address, so two identical cameras can be
/// told apart by what they actually see rather than by guessing.
struct Preview {
    url: String,
    stream: StreamHandle,
    texture: Option<(u64, egui::TextureHandle)>,
}

impl Preview {
    fn start(url: String) -> Self {
        Self {
            stream: StreamHandle::spawn(url.clone()),
            url,
            texture: None,
        }
    }

    /// Latest frame as a texture, or `None` while connecting.
    fn texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let frame = self.stream.latest_frame()?;
        if let Some((generation, tex)) = &self.texture
            && *generation == frame.generation
        {
            return Some(tex.clone());
        }
        let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
        let tex = ctx.load_texture("discover-preview", image, egui::TextureOptions::LINEAR);
        self.texture = Some((frame.generation, tex.clone()));
        Some(tex)
    }
}

impl DiscoverWizard {
    fn new() -> Self {
        // REQ-1: enumerate once on view entry; default to the first RFC1918.
        let interfaces = net::list_interfaces();
        let selected_iface = net::pick_default_subnet(&interfaces).unwrap_or(0);
        Self {
            interfaces,
            selected_iface,
            username: String::new(),
            password: String::new(),
            handle: None,
            latest: None,
            rows: Vec::new(),
            error: None,
            cancelling: false,
            ws: None,
            preview: None,
        }
    }

    /// Live snapshot pull at the top of `update()` (REQ-4): reconciles rows
    /// and transitions out of Scanning once a terminal phase lands. The
    /// handle is taken (not leaked) only after the pipeline thread finished,
    /// so its Drop-join returns instantly instead of blocking the UI.
    fn poll(&mut self, existing: &[CameraConfig]) {
        let snap = match self.handle.as_ref() {
            Some(handle) => handle.snapshot(),
            None => return,
        };
        if !matches!(snap.phase, Phase::Configuring) {
            self.ws = Some((snap.ws_found, snap.ws_degraded));
        }
        self.latest = Some(snap.clone());
        reconcile_rows(&mut self.rows, &snap.results, existing);

        if !matches!(
            snap.phase,
            Phase::Complete | Phase::Cancelled | Phase::Failed(_)
        ) {
            return;
        }
        self.handle.take(); // Thread already joined; Drop returns instantly.
        self.cancelling = false;
        match snap.phase {
            Phase::Cancelled => self.rows.clear(), // REQ-4: Idle, inputs kept.
            Phase::Failed(reason) => self.error = Some(reason),
            _ => {}
        }
    }

    fn start_scan(&mut self) {
        let Some(interface) = self.interfaces.get(self.selected_iface) else {
            return;
        };
        // REQ-1 clamp scenario: always the containing /24 regardless of the
        // interface's reported prefix.
        let Some(subnet) = net::subnet_of(interface.ip, 24) else {
            self.error = Some(format!("cannot derive /24 subnet from {}", interface.ip));
            return;
        };
        self.rows.clear();
        self.latest = None;
        self.error = None;
        self.cancelling = false;
        self.ws = None;
        self.handle = Some(DiscoveryHandle::start(DiscoveryConfig {
            subnet,
            ports: DEFAULT_PORTS.to_vec(),
            creds: global_creds(&self.username, &self.password),
            probe_timeout: DISCOVER_PROBE_TIMEOUT,
        }));
    }

    /// Signals cancellation; the wizard keeps polling until the pipeline
    /// reports terminal so Drop never blocks the UI on probe wind-down.
    fn request_cancel(&mut self) {
        if let Some(handle) = &self.handle {
            handle.cancel();
        }
        self.cancelling = true;
    }

    /// Ends the wizard from outside the Discover view (task 6.3 / REQ-4
    /// nav-away guarantee). Cancellation is signalled synchronously; the
    /// handle then moves to a detached closer thread so its Drop-join (up to
    /// one in-flight probe attempt plus poll granularity) never blocks UI
    /// repaints. Chosen over deferring the drop to a later `update()` tick:
    /// this runs exactly once at transition time and does not depend on
    /// repaint cadence, focus state, or extra per-frame app state.
    fn end_detached(&mut self) {
        if let Some(handle) = self.handle.take() {
            Self::detach_handle(handle);
        }
    }

    /// RAII hand-off. If spawning fails, std drops the closure inline — the
    /// handle still cancels and joins, just synchronously; nothing leaks.
    fn detach_handle(handle: DiscoveryHandle) {
        handle.cancel();
        let _ = std::thread::Builder::new()
            .name("discover-close".to_owned())
            .spawn(move || drop(handle));
    }

    fn back_to_idle(&mut self) {
        self.rows.clear();
        self.latest = None;
        self.error = None;
    }

    fn show(&mut self, ctx: &egui::Context) -> DiscoverAction {
        let mut action = DiscoverAction::None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::SOOT)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(ctx, |ui| {
                paper_visuals(ui);
                ui.label(
                    egui::RichText::new("DISCOVER")
                        .font(theme::display_font(30.0))
                        .color(theme::PAPER),
                );
                ui.label(theme::micro_label(
                    format!(
                        "LOCAL NETWORK \u{b7} RTSP AUTO-DISCOVERY \u{b7} {} INTERFACES",
                        self.interfaces.len()
                    ),
                    theme::LABEL_ON_PAPER,
                ));
                ui.add_space(8.0);

                if let Some(err) = &self.error {
                    error_line(ui, err);
                    ui.add_space(4.0);
                }

                if self.handle.is_some() {
                    self.show_scanning(ui, &mut action);
                } else if self.rows.is_empty() {
                    self.show_idle(ui, &mut action);
                } else {
                    self.show_results(ui, &mut action);
                }
            });
        action
    }

    fn show_idle(&mut self, ui: &mut egui::Ui, action: &mut DiscoverAction) {
        ui.add_space(4.0);
        if self.interfaces.is_empty() {
            ui.label(theme::micro_label(
                "NO NETWORK INTERFACES FOUND",
                theme::LABEL_ON_PAPER,
            ));
            ui.label(theme::micro_label(
                "ADD A CAMERA MANUALLY IN SETTINGS",
                theme::LABEL_ON_PAPER,
            ));
            return;
        }

        ui.label(theme::micro_label("SUBNET", theme::LABEL_ON_PAPER));
        // Re-applied locally: ComboBox builds its own child Ui for the
        // closed button and it doesn't reliably inherit the ambient
        // paper_visuals() call at the top of show(), leaving it on the
        // dark widget palette (looked disabled against the SOOT page).
        paper_visuals(ui);
        egui::ComboBox::from_id_salt("discover_subnet")
            .selected_text(self.subnet_label(self.selected_iface))
            .width(280.0)
            .show_ui(ui, |ui| {
                for i in 0..self.interfaces.len() {
                    let label = self.subnet_label(i);
                    ui.selectable_value(&mut self.selected_iface, i, label);
                }
            });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(theme::micro_label(
                    "USERNAME (OPTIONAL)",
                    theme::LABEL_ON_PAPER,
                ));
                ui.add_sized(
                    [200.0, 28.0],
                    egui::TextEdit::singleline(&mut self.username),
                );
            });
            ui.vertical(|ui| {
                ui.label(theme::micro_label(
                    "PASSWORD (OPTIONAL)",
                    theme::LABEL_ON_PAPER,
                ));
                ui.add_sized(
                    [200.0, 28.0],
                    egui::TextEdit::singleline(&mut self.password).password(true),
                );
            });
        });
        // REQ-6: one-line plaintext-storage hint above credentials.
        ui.label(theme::micro_label(
            "CREDENTIALS ARE STORED IN PLAINTEXT INSIDE CAMERAS.TOML",
            theme::LABEL_ON_PAPER,
        ));

        ui.add_space(12.0);
        if theme::brutal_button_sized(
            ui,
            egui::vec2(ui.available_width(), 34.0),
            "SCAN",
            BtnVariant::Confirm,
        ) {
            *action = DiscoverAction::Scan;
        }
    }

    fn show_scanning(&mut self, ui: &mut egui::Ui, action: &mut DiscoverAction) {
        footer_panel(ui, "discover_scanning_footer", |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.cancelling {
                    ui.label(theme::micro_label(
                        "CANCELLING\u{2026}",
                        theme::LABEL_ON_PAPER,
                    ));
                } else if theme::brutal_button(ui, "CANCEL", BtnVariant::Danger) {
                    *action = DiscoverAction::Cancel;
                }
            });
        });

        ui.add_space(4.0);
        match &self.latest {
            Some(snap) => ui.label(theme::micro_label(
                format!(
                    "HOSTS {}/{} \u{b7} RESPONDERS {} \u{b7} PROBES {}/{}",
                    snap.hosts_scanned.min(snap.hosts_total),
                    snap.hosts_total,
                    snap.responders_found,
                    snap.probes_done,
                    snap.probes_total
                ),
                theme::LABEL_ON_PAPER,
            )),
            None => ui.label(theme::micro_label(
                "STARTING DISCOVERY\u{2026}",
                theme::LABEL_ON_PAPER,
            )),
        };
        auth_legend(ui);
        self.maybe_ws_hint(ui);
        ui.add_space(6.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                discover_rows_ui(ui, &mut self.rows, &mut self.preview);
            });
    }

    fn show_results(&mut self, ui: &mut egui::Ui, action: &mut DiscoverAction) {
        let selected = self
            .rows
            .iter()
            .filter(|row| row.checked && row.addable())
            .count();
        footer_panel(ui, "discover_results_footer", |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Disabled (unclickable) when nothing addable is ticked (REQ-13).
            let can_add = selected > 0;
            let clicked = ui
                .scope(|ui| {
                    if !can_add {
                        ui.disable();
                    }
                    theme::brutal_button_sized(
                        ui,
                        egui::vec2(170.0, 34.0),
                        "ADD SELECTED",
                        BtnVariant::Confirm,
                    )
                })
                .inner;
            if clicked {
                *action = DiscoverAction::AddSelected;
            }
            if theme::brutal_button(ui, "< BACK", BtnVariant::Paper) {
                *action = DiscoverAction::Back;
            }
            });
        });

        ui.add_space(4.0);
        ui.label(theme::micro_label(
            format!(
                "{} DEVICE(S) FOUND \u{b7} {} SELECTED",
                self.rows.len(),
                selected
            ),
            theme::LABEL_ON_PAPER,
        ));
        auth_legend(ui);
        self.maybe_ws_hint(ui);
        ui.add_space(6.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                discover_rows_ui(ui, &mut self.rows, &mut self.preview);
            });
    }

    fn maybe_ws_hint(&self, ui: &mut egui::Ui) {
        let Some((found, degraded)) = self.ws else {
            return;
        };
        let results_empty = self
            .latest
            .as_ref()
            .is_none_or(|snap| snap.results.is_empty());
        if degraded || (found == 0 && results_empty) {
            ui.label(theme::micro_label(
                "FOUND 0 VIA ONVIF \u{b7} FIREWALL?",
                theme::LABEL_ON_PAPER,
            ));
        }
    }

    /// Containing-/24 label for the picker (REQ-1 clamp scenario).
    fn subnet_label(&self, index: usize) -> String {
        match self.interfaces.get(index) {
            Some(interface) => match net::subnet_of(interface.ip, 24) {
                Some(subnet) => {
                    format!(
                        "{}/24 \u{b7} {}",
                        Ipv4Addr::from(subnet.network),
                        interface.name
                    )
                }
                None => format!("{} \u{b7} {}", interface.ip, interface.name),
            },
            None => String::from("\u{2014}"),
        }
    }
}

/// Bottom-anchored action bar.
///
/// MUST be called before anything else is drawn into `ui`. A panel reserves
/// space out of the Ui's remaining rect, so declaring it after the page
/// content has already advanced the cursor mis-measures what is left and
/// starves the scroll area below it.
///
/// Uses a real panel so egui reserves the footer's *measured* height and the
/// scroll area above receives exactly what is left. The previous approach
/// subtracted a guessed 50pt from the available height, which under-counted
/// the footer (12pt space + 8pt item spacing + a 34pt button) and drifted
/// further wrong as the window height changed, pushing the buttons out of
/// alignment or off-screen on other displays.
fn footer_panel(ui: &mut egui::Ui, id: &'static str, add: impl FnOnce(&mut egui::Ui)) {
    egui::TopBottomPanel::bottom(id)
        .frame(egui::Frame::NONE)
        .show_inside(ui, |ui| {
            ui.add_space(10.0);
            add(ui);
            ui.add_space(2.0);
        });
}

fn discover_rows_ui(
    ui: &mut egui::Ui,
    rows: &mut [DiscoverRow],
    preview: &mut Option<Preview>,
) {
    for row in rows {
        discover_row_card(ui, row, preview);
        ui.add_space(8.0);
    }
}

fn discover_row_card(ui: &mut egui::Ui, row: &mut DiscoverRow, preview: &mut Option<Preview>) {
    let border = if row.checked {
        theme::STATUS_OFFLINE
    } else {
        theme::INK
    };
    egui::Frame::new()
        .fill(theme::PAPER_2)
        .stroke(egui::Stroke::new(2.0_f32, border))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            paper_visuals(ui);
            // REQ-10 binding semantics: warning-dot / already-configured hosts
            // are not addable; fade the whole row so that reads as disabled,
            // not just the checkbox.
            ui.add_enabled_ui(row.addable(), |ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut row.checked, "");
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(dot, 0.0, auth_color(row.result.auth));
                    ui.painter().rect_stroke(
                        dot,
                        0.0,
                        egui::Stroke::new(1.0_f32, theme::PAPER.gamma_multiply(0.25)),
                        egui::StrokeKind::Inside,
                    );

                    ui.vertical(|ui| {
                        let headline = match &row.result.vendor {
                            Some(vendor) => {
                                format!("{} \u{b7} {}", row.result.ip, vendor.to_uppercase())
                            }
                            None => row.result.ip.to_string(),
                        };
                        ui.label(
                            egui::RichText::new(headline)
                                .font(theme::mono_font(12.5))
                                .color(theme::INK),
                        );
                        let detail = match (&row.result.url, row.result.resolution) {
                            (Some(url), Some((w, h))) => format!("{url}  \u{b7}  {w}\u{d7}{h}"),
                            (Some(url), None) => url.clone(),
                            (None, _) => match row.result.auth {
                                RowStatus::PathUnknown => format!(
                                    "RTSP ON PORT {} \u{2014} STREAM PATH NOT RECOGNISED",
                                    row.result.port
                                ),
                                _ => String::from(
                                    "NO WORKING URL \u{2014} AUTHENTICATION REQUIRED",
                                ),
                            },
                        };
                        let shown = theme::elide_to_width(
                            ui.painter(),
                            &detail,
                            theme::mono_font(10.0),
                            ui.available_width(),
                            theme::LABEL_ON_PAPER,
                        );
                        ui.label(theme::micro_label(shown, theme::LABEL_ON_PAPER));
                    });
                });
            });
            // Two identical cameras are indistinguishable on paper: same
            // path, port, resolution and vendor. Seeing the picture is the
            // only thing that actually tells them apart.
            if let Some(url) = row.effective_url() {
                let showing = preview.as_ref().is_some_and(|p| p.url == url);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if showing {
                        if theme::brutal_button(ui, "HIDE PREVIEW", BtnVariant::Paper) {
                            *preview = None; // Drop stops the ffmpeg process.
                        }
                    } else if theme::brutal_button(ui, "PREVIEW", BtnVariant::Paper) {
                        // Replacing the previous preview stops it: only one
                        // stream runs at a time.
                        *preview = Some(Preview::start(url.clone()));
                    }
                });
                if showing {
                    let tex = preview.as_mut().and_then(|p| p.texture(ui.ctx()));
                    ui.add_space(4.0);
                    match tex {
                        Some(tex) => {
                            let width = ui.available_width().min(320.0);
                            let size = tex.size_vec2();
                            let height = if size.x > 0.0 {
                                width * size.y / size.x
                            } else {
                                width * 9.0 / 16.0
                            };
                            ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(
                                width, height,
                            )));
                        }
                        None => {
                            ui.label(theme::micro_label("CONNECTING\u{2026}", theme::LABEL_ON_PAPER));
                        }
                    }
                }
            }
            // Full contrast, outside the fade: this is what turns a
            // "new camera" row into "the camera you already had, moved".
            if !row.relink_options.is_empty() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(theme::micro_label("LOOKS MOVED", theme::INK));
                    let selected = match &row.relink_to {
                        Some(name) => format!("RELINK \u{2192} {name}"),
                        None => String::from("ADD AS NEW"),
                    };
                    egui::ComboBox::from_id_salt(("relink", row.result.ip))
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut row.relink_to, None, "ADD AS NEW");
                            for name in &row.relink_options {
                                ui.selectable_value(
                                    &mut row.relink_to,
                                    Some(name.clone()),
                                    format!("RELINK \u{2192} {name}"),
                                );
                            }
                        });
                });
                if let Some(name) = &row.relink_to {
                    let note = match &row.relink_swaps_with {
                        Some(other) => format!(
                            "{name} moves here and {other} takes the address {name} leaves \u{2014} they swap"
                        ),
                        None => format!("{name} keeps its name and moves to this address"),
                    };
                    ui.label(theme::micro_label(note, theme::LABEL_ON_PAPER));
                }
            }
            // Outside the fade above on purpose: this row is disabled precisely
            // because no path is known, so the control that fixes that has to
            // stay usable.
            if row.result.auth == RowStatus::PathUnknown && !row.exact_duplicate {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(theme::micro_label("PATH", theme::LABEL_ON_PAPER));
                    let hint = format!("rtsp://{}:{}", row.result.ip, row.result.port);
                    ui.add(
                        egui::TextEdit::singleline(&mut row.manual_path)
                            .hint_text("/media/video1")
                            .desired_width(200.0)
                            .font(theme::mono_font(11.0)),
                    );
                    let preview = match normalized_path(&row.manual_path) {
                        Some(path) => format!("{hint}{path}"),
                        None => String::from("type the camera's stream path to add it"),
                    };
                    ui.label(theme::micro_label(preview, theme::LABEL_ON_PAPER));
                });
            }
            // Drawn at full contrast, outside the fade above: this is the
            // reason the row is disabled, so it must stay legible.
            if row.relink_to.is_some() {
                // Saying "already configured" next to a chosen relink reads as
                // "nothing will happen", which is the opposite of the truth.
            } else if row.exact_duplicate {
                ui.label(theme::micro_label("ALREADY CONFIGURED", theme::INK));
            } else if row.duplicate {
                ui.label(theme::micro_label("SAME HOST ALREADY CONFIGURED", theme::INK));
            }
        });
}

/// Auth-status dot color per REQ-10 / design mapping.
fn auth_color(auth: RowStatus) -> egui::Color32 {
    match auth {
        RowStatus::Authenticated | RowStatus::Open => theme::STATUS_ONLINE,
        RowStatus::NeedsCredentials | RowStatus::PathUnknown => theme::STATUS_CONNECTING,
    }
}

/// Explains the row dot colors: nothing else on screen says what they mean.
fn auth_legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (color, label) in [
            (theme::STATUS_ONLINE, "OPEN / AUTHENTICATED \u{2014} ADDABLE"),
            (theme::STATUS_CONNECTING, "NEEDS CREDENTIALS / PATH"),
        ] {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(dot, 0.0, color);
            ui.label(theme::micro_label(label, theme::LABEL_ON_PAPER));
            ui.add_space(10.0);
        }
    });
}

/// Inline error banner shared by Settings and Discover (dot + red mono text).
fn error_line(ui: &mut egui::Ui, err: &str) {
    ui.horizontal(|ui| {
        let (block, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().rect_filled(block, 0.0, theme::STATUS_OFFLINE);
        ui.label(
            egui::RichText::new(err.to_string())
                .font(theme::mono_font(10.0))
                .color(theme::STATUS_OFFLINE),
        );
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarAction {
    None,
    Grid,
    Discover,
    Settings,
    Solo(usize),
}

/// User intent produced by one frame of the discover wizard panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoverAction {
    None,
    Scan,
    Cancel,
    Back,
    AddSelected,
}

pub struct CamViewerApp {
    cameras: Vec<CameraView>,
    view: View,
    settings: SettingsEditor,
    discover: Option<DiscoverWizard>,
    badge_position: BadgePosition,
    /// Persisted opt-out, carried so a settings save cannot silently re-enable
    /// a check the user turned off.
    update_check: bool,
    /// Filled in by the background check; `None` until (and unless) a newer
    /// release is found.
    update_available: update::Shared,
    /// Cleared for the session once the user dismisses the notice.
    update_dismissed: bool,
}

impl CamViewerApp {
    pub fn new(config: &Config) -> Self {
        let cameras = config
            .cameras
            .iter()
            .map(|cam| CameraView {
                name: cam.name.clone(),
                url: cam.url.clone(),
                stream: StreamHandle::spawn(cam.url.clone()),
                texture: None,
            })
            .collect();
        Self {
            cameras,
            view: if config.cameras.is_empty() {
                View::Settings
            } else {
                View::Grid
            },
            settings: SettingsEditor::from_config(config),
            discover: None,
            badge_position: config.badge_position,
            update_check: config.update_check,
            update_available: if config.update_check {
                update::spawn_check()
            } else {
                update::Shared::default()
            },
            update_dismissed: false,
        }
    }

    fn open_settings(&mut self) {
        self.settings = SettingsEditor::from_config(&Config {
            badge_position: self.badge_position,
            update_check: self.update_check,
            cameras: self.camera_configs(),
        });
        self.view = View::Settings;
    }

    /// Strip announcing a newer published release, shown only once the
    /// background check has found one. It links to the release page and never
    /// downloads anything itself; dismissing hides it for this session.
    fn show_update_notice(&mut self, ctx: &egui::Context) {
        if self.update_dismissed {
            return;
        }
        let Some(available) = self
            .update_available
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
        else {
            return;
        };

        let mut dismiss = false;
        egui::TopBottomPanel::top("update_notice")
            .frame(
                egui::Frame::new()
                    .fill(theme::PAPER)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(theme::micro_label("UPDATE AVAILABLE", theme::INK));
                    ui.label(
                        egui::RichText::new(format!(
                            "v{} \u{2192} v{}",
                            env!("CARGO_PKG_VERSION"),
                            available.version
                        ))
                        .font(theme::mono_font(12.0))
                        .color(theme::LABEL_ON_PAPER),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::brutal_button(ui, "DISMISS", BtnVariant::Paper) {
                            dismiss = true;
                        }
                        if theme::brutal_button(ui, "GET IT", BtnVariant::Ink) {
                            ctx.open_url(egui::OpenUrl::new_tab(&available.url));
                        }
                    });
                });
            });
        self.update_dismissed = dismiss;
    }

    fn camera_configs(&self) -> Vec<CameraConfig> {
        self.cameras
            .iter()
            .map(|cam| CameraConfig {
                name: cam.name.clone(),
                url: cam.url.clone(),
            })
            .collect()
    }

    /// Single source of truth for persisting a camera list and (re)spawning
    /// streams (REQ-14): `config::save` + `apply_cameras` + navigate to Grid.
    /// On failure nothing is mutated and the error names the cause; the
    /// caller stays where it was. `cameras` is the COMPLETE desired list —
    /// existing streams are reused by URL equality, so unchanged cameras
    /// never restart.
    ///
    /// Deviation from the task sketch (`extra` param with internal merge):
    /// merging lives in the tested `merge_cameras` helper at the discovery
    /// call site, because a settings save REPLACES the list (renames and
    /// deletions cannot be expressed as "+extra" against the current list).
    fn commit_cameras(
        &mut self,
        badge_position: BadgePosition,
        update_check: bool,
        cameras: Vec<CameraConfig>,
    ) -> Result<(), String> {
        config::save(
            &config::config_path(),
            &Config {
                badge_position,
                update_check,
                cameras: cameras.clone(),
            },
        )
        .map_err(|err| format!("Failed to save config: {err:#}"))?;
        self.badge_position = badge_position;
        self.update_check = update_check;
        self.apply_cameras(cameras);
        self.view = View::Grid;
        Ok(())
    }

    fn save_settings(&mut self) {
        self.settings.error = None;
        let desired = self.settings.collect();
        let badge_position = self.settings.badge_position;
        let update_check = self.settings.update_check;
        if let Err(err) = self.commit_cameras(badge_position, update_check, desired) {
            self.settings.error = Some(err);
        }
    }

    fn cancel_settings(&mut self) {
        self.settings.error = None;
        self.view = View::Grid;
    }

    /// REQ-12: entering Discover starts fresh at Idle. Leaving through any
    /// route ends the wizard via [`CamViewerApp::abandon_discover`].
    fn open_discover(&mut self) {
        self.discover = Some(DiscoverWizard::new());
        self.view = View::Discover;
    }

    /// Task 6.3 / spec REQ-4: leaving Discover through ANY route (GRID VIEW,
    /// SETTINGS, sidebar SOLO rows, tile clicks, Escape) ends the wizard with
    /// Escape's guarantees — workers cancelled, no orphaned threads or late
    /// writes, cameras.toml untouched — while the blocking join runs on a
    /// detached closer thread instead of the UI thread.
    fn abandon_discover(&mut self) {
        if let Some(mut wizard) = self.discover.take() {
            wizard.end_detached();
        }
    }

    fn close_discover(&mut self) {
        self.abandon_discover();
        self.view = View::Grid;
    }

    /// REQ-14 merge: build entries from checked addable rows, dedup against
    /// the current config by exact URL, persist through the shared commit
    /// path. Save failure keeps the wizard in Results with the selection
    /// intact and no navigation.
    fn add_selected(&mut self) {
        let mut existing = self.camera_configs();
        let (extra, relinked) = match self.discover.as_ref() {
            Some(wizard) => {
                // Relinks rewrite entries in place, so they run before the
                // append: a moved camera keeps its name instead of being
                // duplicated under a generated one.
                let relinked = apply_relinks(&mut existing, &wizard.rows);
                let extra = selected_cameras(&wizard.rows, existing.len());
                (extra, relinked)
            }
            None => (Vec::new(), 0),
        };
        if extra.is_empty() && relinked == 0 {
            return; // Verified no-op guard (REQ-13 empty-selection scenario).
        }
        let merged = merge_cameras(&existing, extra);
        match self.commit_cameras(self.badge_position, self.update_check, merged) {
            Ok(()) => self.discover = None, // commit navigated to Grid (REQ-15).
            Err(err) => {
                if let Some(wizard) = self.discover.as_mut() {
                    wizard.error = Some(err);
                }
            }
        }
    }

    fn apply_cameras(&mut self, desired: Vec<CameraConfig>) {
        let mut pool: Vec<Option<CameraView>> = std::mem::take(&mut self.cameras)
            .into_iter()
            .map(Some)
            .collect();
        let mut cameras = Vec::with_capacity(desired.len());

        for cam in &desired {
            let pos = pool
                .iter()
                .position(|slot| slot.as_ref().is_some_and(|view| view.url == cam.url));
            let reused = pos.and_then(|i| pool[i].take());
            cameras.push(match reused {
                Some(mut view) => {
                    view.name.clone_from(&cam.name);
                    view
                }
                None => CameraView {
                    name: cam.name.clone(),
                    url: cam.url.clone(),
                    stream: StreamHandle::spawn(cam.url.clone()),
                    texture: None,
                },
            });
        }

        drop(pool.into_iter().flatten());
        self.cameras = cameras;
    }
}

impl eframe::App for CamViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Discovery snapshot polling at the very top of update (REQ-4): live
        // progress rides the global 50ms repaint cadence — no new timers.
        if matches!(self.view, View::Discover) {
            let existing = self.camera_configs();
            if let Some(wizard) = self.discover.as_mut() {
                wizard.poll(&existing);
            }
        }

        if matches!(self.view, View::Settings) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.settings.is_dirty() && !self.settings.confirm_discard {
                self.settings.confirm_discard = true;
            } else {
                self.cancel_settings();
            }
        }
        // Escape leaves Discover; dropping the wizard cancels any scan.
        if matches!(self.view, View::Discover) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_discover();
        }
        if matches!(self.view, View::Solo(_)) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.view = View::Grid;
        }

        self.show_update_notice(ctx);

        let sidebar = show_sidebar(ctx, &self.view, &self.cameras);
        match sidebar {
            SidebarAction::Grid => self.view = View::Grid,
            SidebarAction::Discover => self.open_discover(),
            SidebarAction::Settings => self.open_settings(),
            SidebarAction::Solo(index) => self.view = View::Solo(index),
            SidebarAction::None => {}
        }
        // Task 6.3 / REQ-4: any route out of Discover ends the wizard with
        // the Escape guarantees; keyed on the view rather than enumerated
        // routes so no transition can bypass it.
        if self.discover.is_some() && !matches!(self.view, View::Discover) {
            self.abandon_discover();
        }

        match self.view {
            View::Grid => match show_grid(ctx, &mut self.cameras, self.badge_position) {
                GridAction::OpenSolo(index) => self.view = View::Solo(index),
                GridAction::GoToDiscover => self.open_discover(),
                GridAction::GoToSettings => self.open_settings(),
                GridAction::None => {}
            },
            View::Solo(index) => {
                if index >= self.cameras.len()
                    || show_solo(ctx, &mut self.cameras[index], self.badge_position)
                {
                    self.view = View::Grid;
                }
            }
            View::Discover => {
                let action = self
                    .discover
                    .as_mut()
                    .map(|wizard| wizard.show(ctx))
                    .unwrap_or(DiscoverAction::None);
                match action {
                    DiscoverAction::Scan => {
                        if let Some(wizard) = self.discover.as_mut() {
                            wizard.start_scan();
                        }
                    }
                    DiscoverAction::Cancel => {
                        if let Some(wizard) = self.discover.as_mut() {
                            wizard.request_cancel();
                        }
                    }
                    DiscoverAction::Back => {
                        if let Some(wizard) = self.discover.as_mut() {
                            wizard.back_to_idle();
                        }
                    }
                    DiscoverAction::AddSelected => self.add_selected(),
                    DiscoverAction::None => {}
                }
            }
            View::Settings => match show_settings(ctx, &mut self.settings) {
                SettingsAction::Save => self.save_settings(),
                SettingsAction::Cancel => self.cancel_settings(),
                SettingsAction::None => {}
            },
        }

        ctx.request_repaint_after(REPAINT_INTERVAL);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for cam in &self.cameras {
            cam.stream.stop();
        }
    }
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

fn show_sidebar(ctx: &egui::Context, view: &View, cameras: &[CameraView]) -> SidebarAction {
    let mut action = SidebarAction::None;
    egui::SidePanel::left("sidebar")
        .exact_width(SIDEBAR_WIDTH)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin {
                    left: 14,
                    right: 14,
                    top: 16,
                    bottom: 16,
                }),
        )
        .show(ctx, |ui| {
            // Wordmark
            ui.horizontal(|ui| {
                let (block, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(block, 0.0, theme::STATUS_OFFLINE);
                ui.painter().rect_stroke(
                    block,
                    0.0,
                    egui::Stroke::new(1.0_f32, theme::PAPER),
                    egui::StrokeKind::Inside,
                );
                ui.label(
                    egui::RichText::new("C A M - V I E W E R")
                        .font(theme::mono_font(11.5))
                        .color(theme::PAPER),
                );
            });
            ui.add_space(10.0);
            hairline(ui, theme::SOOT_2, 2.0);
            ui.add_space(8.0);
            ui.label(theme::micro_label(
                format!("{} SIGNALS \u{b7} LOCAL ONLY", cameras.len()),
                theme::ASH,
            ));
            ui.add_space(14.0);

            let grid_active = matches!(view, View::Grid | View::Solo(_));
            if theme::nav_item(
                ui,
                "GRID VIEW",
                Some(cameras.len().to_string()),
                grid_active,
            ) && !grid_active
            {
                action = SidebarAction::Grid;
            }
            ui.add_space(6.0);
            if theme::nav_item(ui, "DISCOVER", None, matches!(view, View::Discover))
                && !matches!(view, View::Discover)
            {
                action = SidebarAction::Discover;
            }
            ui.add_space(6.0);
            if theme::nav_item(ui, "SETTINGS", None, matches!(view, View::Settings))
                && !matches!(view, View::Settings)
            {
                action = SidebarAction::Settings;
            }
            ui.add_space(18.0);

            ui.label(theme::micro_label("CAMERAS", theme::ASH));
            ui.add_space(4.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if cameras.is_empty() {
                        ui.label(theme::micro_label("NO CAMERAS CONFIGURED", theme::ASH));
                        return;
                    }
                    for (i, cam) in cameras.iter().enumerate() {
                        let selected = matches!(view, View::Solo(s) if *s == i);
                        if camera_row(ui, cam, cam.stream.status(), selected)
                            && action == SidebarAction::None
                        {
                            action = SidebarAction::Solo(i);
                        }
                        ui.add_space(2.0);
                    }
                });
        });
    action
}

fn hairline(ui: &mut egui::Ui, color: egui::Color32, thickness: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, thickness), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

fn camera_row(ui: &mut egui::Ui, cam: &CameraView, status: Status, selected: bool) -> bool {
    let width = ui.available_width();
    let height = if selected { 46.0 } else { 30.0 };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

    if selected {
        ui.painter()
            .rect_filled(rect.translate(egui::vec2(4.0, 4.0)), 0.0, theme::CONCRETE);
    }
    let painter = ui.painter_at(rect);
    if selected || response.hovered() {
        painter.rect_filled(rect, 0.0, theme::SOOT_2);
    }

    let dot_y = if selected {
        rect.top() + 12.0
    } else {
        rect.center().y
    };
    let dot_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 6.0, dot_y - 4.0),
        egui::vec2(8.0, 8.0),
    );
    painter.rect_filled(dot_rect, 0.0, theme::status_color(status));
    painter.rect_stroke(
        dot_rect,
        0.0,
        egui::Stroke::new(1.0_f32, theme::PAPER.gamma_multiply(0.25)),
        egui::StrokeKind::Inside,
    );

    let name_x = rect.left() + 24.0;
    let name_max = rect.right() - name_x - 6.0;
    let shown_name = theme::elide_to_width(
        &painter,
        &cam.name,
        egui::FontId::new(13.5, egui::FontFamily::Proportional),
        name_max,
        theme::PAPER,
    );
    if selected {
        painter.text(
            egui::pos2(name_x, rect.top() + 7.0),
            egui::Align2::LEFT_TOP,
            shown_name,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
            theme::PAPER,
        );
        painter.text(
            egui::pos2(name_x, rect.bottom() - 7.0),
            egui::Align2::LEFT_BOTTOM,
            status.label().to_uppercase(),
            theme::mono_font(9.0),
            theme::status_color(status),
        );
    } else {
        painter.text(
            egui::pos2(name_x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            shown_name,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
            theme::PAPER,
        );
    }

    response.clicked()
}

// ---------------------------------------------------------------------------
// Grid view
// ---------------------------------------------------------------------------

/// Grid-view outcome: open a tile solo, or jump elsewhere from the empty state.
enum GridAction {
    None,
    OpenSolo(usize),
    GoToDiscover,
    GoToSettings,
}

fn show_grid(
    ctx: &egui::Context,
    cameras: &mut [CameraView],
    badge_position: BadgePosition,
) -> GridAction {
    let mut opened = None;
    let mut empty_state_action = GridAction::None;
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin::same(18)),
        )
        .show(ctx, |ui| {
            let avail = ui.available_size();
            if cameras.is_empty() {
                ui.label(
                    egui::RichText::new("No cameras configured.")
                        .font(theme::mono_font(12.0))
                        .color(theme::ASH),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if theme::brutal_button(ui, "DISCOVER", BtnVariant::Confirm) {
                        empty_state_action = GridAction::GoToDiscover;
                    }
                    if theme::brutal_button(ui, "+ ADD CAMERA", BtnVariant::Paper) {
                        empty_state_action = GridAction::GoToSettings;
                    }
                });
                return;
            }
            let cols = pick_columns(avail, cameras.len());
            let rows = cameras.len().div_ceil(cols);
            let aspect = 16.0_f32 / 9.0;
            let tile_w_from_cols =
                ((avail.x - GRID_SPACING * (cols - 1) as f32) / cols as f32).max(160.0);
            // Shrink tiles (keeping 16:9) so all rows fit the viewport when possible.
            let tile_h_fit = ((avail.y - GRID_SPACING * (rows - 1) as f32) / rows as f32).max(90.0);
            let tile_w = tile_w_from_cols.min(tile_h_fit * aspect);
            let tile_h = tile_w / aspect;
            let needed_h = rows as f32 * tile_h + (rows - 1) as f32 * GRID_SPACING;

            ui.spacing_mut().item_spacing = egui::vec2(GRID_SPACING, GRID_SPACING);
            let layout = GridTileLayout {
                cols,
                tile_w,
                tile_h,
            };
            if needed_h <= avail.y {
                draw_grid_rows(ui, cameras, layout, badge_position, &mut opened);
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        draw_grid_rows(ui, cameras, layout, badge_position, &mut opened);
                    });
            }
        });
    match opened {
        Some(index) => GridAction::OpenSolo(index),
        None => empty_state_action,
    }
}

/// Near-square column count: 1x1, 1x2, 2x2, 2x3, 3x3, 3x4, 4x4… (smallest
/// `cols` that keeps rows within `cols + 1`), capped by ~360px minimum tile
/// width. Vertical overflow is handled by the caller's scroll fallback.
fn pick_columns(avail: egui::Vec2, count: usize) -> usize {
    let cols_by_width = (((avail.x + GRID_SPACING) / (GRID_MIN_TILE_WIDTH + GRID_SPACING))
        .floor()
        .max(1.0)) as usize;
    let mut cols = 1;
    while cols * (cols + 1) < count {
        cols += 1;
    }
    cols.min(cols_by_width).min(count).max(1)
}

struct GridTileLayout {
    cols: usize,
    tile_w: f32,
    tile_h: f32,
}

fn draw_grid_rows(
    ui: &mut egui::Ui,
    cameras: &mut [CameraView],
    layout: GridTileLayout,
    badge_position: BadgePosition,
    opened: &mut Option<usize>,
) {
    let total = cameras.len();
    for start in (0..total).step_by(layout.cols) {
        ui.horizontal(|ui| {
            for (i, cam) in cameras
                .iter_mut()
                .enumerate()
                .take((start + layout.cols).min(total))
                .skip(start)
            {
                let clicked = video_surface(
                    ui,
                    cam,
                    egui::vec2(layout.tile_w, layout.tile_h),
                    true,
                    badge_position,
                );
                if clicked && opened.is_none() {
                    *opened = Some(i);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Video surface (shared by grid tiles and solo stage)
// ---------------------------------------------------------------------------

fn video_surface(
    ui: &mut egui::Ui,
    cam: &mut CameraView,
    size: egui::Vec2,
    overlays: bool,
    badge_position: BadgePosition,
) -> bool {
    let size = size.max(egui::vec2(80.0, 60.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::SOOT_2);

    match cam.stream.latest_frame() {
        Some(frame) => {
            let tex = ensure_texture(ui.ctx(), cam, &frame);
            let draw = fit_rect(rect, frame.width as f32, frame.height as f32);
            painter.image(
                tex.id(),
                draw,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            let msg = match cam.stream.status() {
                Status::Online => format!("{}: waiting for frames...", cam.name),
                Status::Connecting => format!("{}: connecting...", cam.name),
                Status::Offline => format!("{}: offline", cam.name),
                Status::Paused => format!("{}: paused", cam.name),
            };
            center_message(&painter, rect, msg);
        }
    }

    let hovered = response.hovered();
    let border = if overlays && hovered {
        theme::CONCRETE
    } else {
        theme::BORDER_DIM
    };
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0_f32, border),
        egui::StrokeKind::Inside,
    );

    if overlays {
        let name_font = theme::mono_font(12.5);
        let name_pos = rect.left_top() + egui::vec2(12.0, 10.0);
        let name_max_w = (rect.width() * 0.6).max(40.0);
        let shown_name = theme::elide_to_width(
            &painter,
            &cam.name,
            name_font.clone(),
            name_max_w,
            egui::Color32::from_rgba_unmultiplied(11, 11, 13, 220),
        );
        let shadow = painter.layout_no_wrap(
            shown_name.clone(),
            name_font.clone(),
            egui::Color32::from_rgba_unmultiplied(11, 11, 13, 220),
        );
        painter.galley(name_pos + egui::vec2(1.0, 1.0), shadow, theme::SOOT);
        let name = painter.layout_no_wrap(shown_name, name_font, theme::PAPER);
        painter.galley(name_pos, name, theme::PAPER);

        // Name stays pinned top-left; the badge renders at the configured
        // corner so it can never collide with the name overlay.
        theme::status_badge(&painter, rect, cam.stream.status(), badge_position);
    }

    response.clicked()
}

fn center_message(painter: &egui::Painter, rect: egui::Rect, text: impl Into<String>) {
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text.into(),
        theme::mono_font(12.5),
        theme::ghost_text(),
    );
}

// ---------------------------------------------------------------------------
// Solo view
// ---------------------------------------------------------------------------

fn show_solo(ctx: &egui::Context, cam: &mut CameraView, badge_position: BadgePosition) -> bool {
    let mut back = false;

    egui::TopBottomPanel::top("solo_top")
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin::symmetric(18, 12)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                back |= theme::brutal_button(ui, "< BACK", BtnVariant::Paper);
                ui.add_space(8.0);
                let galley = ui.painter().layout_no_wrap(
                    cam.name.to_uppercase(),
                    theme::display_font(26.0),
                    theme::PAPER,
                );
                let (name_rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                ui.painter()
                    .galley(name_rect.left_top(), galley, theme::PAPER);

                let status = cam.stream.status();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(status.label().to_uppercase())
                            .font(theme::mono_font(11.0))
                            .color(theme::status_color(status)),
                    );
                });
            });
            let full = ui.max_rect();
            let y = full.bottom() - 1.0;
            ui.painter().line_segment(
                [egui::pos2(full.left(), y), egui::pos2(full.right(), y)],
                egui::Stroke::new(2.0_f32, theme::BORDER_DIM),
            );
        });

    egui::TopBottomPanel::bottom("solo_instruments")
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin {
                    left: 18,
                    right: 18,
                    top: 0,
                    bottom: 18,
                }),
        )
        .show(ctx, |ui| instrument_row(ui, cam));

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin {
                    left: 18,
                    right: 18,
                    top: 0,
                    bottom: 18,
                }),
        )
        .show(ctx, |ui| {
            video_surface(ui, cam, ui.available_size(), false, badge_position);
        });

    back
}

fn instrument_row(ui: &mut egui::Ui, cam: &CameraView) {
    let width = ui.available_width();
    let height = 56.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::PAPER);
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0_f32, theme::INK),
        egui::StrokeKind::Inside,
    );

    let status = cam.stream.status();
    let resolution = cam
        .stream
        .latest_frame()
        .map(|f| format!("{}\u{d7}{}", f.width, f.height));

    let col_status = rect.left() + 150.0;
    let col_source = col_status + 190.0;
    for x in [col_status, col_source] {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(2.0_f32, theme::INK),
        );
    }

    instrument_cell(
        &painter,
        rect,
        rect.left() + 16.0,
        "STATUS",
        status.label().to_uppercase(),
        theme::status_color(status),
    );
    instrument_cell(
        &painter,
        rect,
        col_status + 16.0,
        "RESOLUTION",
        resolution.unwrap_or_else(|| "\u{2014}".to_owned()),
        theme::INK,
    );

    let source_x = col_source + 16.0;
    let max_w = (rect.right() - source_x - 16.0).max(40.0);
    let shown_url = theme::elide_to_width(
        &painter,
        &cam.url,
        theme::mono_font(12.0),
        max_w,
        theme::INK,
    );
    instrument_cell(&painter, rect, source_x, "SOURCE", shown_url, theme::INK);
}

fn instrument_cell(
    painter: &egui::Painter,
    strip: egui::Rect,
    x: f32,
    label: &str,
    value: String,
    color: egui::Color32,
) {
    painter.text(
        egui::pos2(x, strip.top() + 9.0),
        egui::Align2::LEFT_TOP,
        label,
        theme::mono_font(9.0),
        theme::LABEL_ON_PAPER,
    );
    painter.text(
        egui::pos2(x, strip.bottom() - 9.0),
        egui::Align2::LEFT_BOTTOM,
        value,
        theme::mono_font(13.5),
        color,
    );
}

// ---------------------------------------------------------------------------
// Settings view
// ---------------------------------------------------------------------------

thread_local! {
    /// (content height, viewport height) from the last Settings frame.
    ///
    /// Exists so a test can assert that an ordinary camera list fits without
    /// scrolling, instead of that being something only a human notices after
    /// a release. Written every frame; read only by tests.
    static LAST_SETTINGS_FIT: std::cell::Cell<Option<(f32, f32)>> =
        const { std::cell::Cell::new(None) };
}

fn show_settings(ctx: &egui::Context, editor: &mut SettingsEditor) -> SettingsAction {
    let mut action = SettingsAction::None;
    // Applied before anything draws, so is_dirty() and the row list agree for
    // the whole frame.
    editor.apply_pending_delete();
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            paper_visuals(ui);

            let dirty = editor.is_dirty();
            footer_panel(ui, "settings_footer", |ui| {
                if dirty && editor.confirm_discard {
                    ui.label(theme::micro_label(
                        "UNSAVED CHANGES \u{2014} PRESS CANCEL AGAIN TO DISCARD THEM",
                        theme::STATUS_OFFLINE,
                    ));
                    ui.add_space(6.0);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let saved = ui
                        .scope(|ui| {
                            if !dirty {
                                ui.disable();
                            }
                            theme::brutal_button(ui, "SAVE", BtnVariant::Confirm)
                        })
                        .inner;
                    if saved {
                        action = SettingsAction::Save;
                    }

                    // A reflex CANCEL/Escape shouldn't silently drop an edit:
                    // the first press while dirty only arms the discard, a
                    // second press (here or via Escape) confirms it.
                    let armed = dirty && editor.confirm_discard;
                    let (label, variant) = if armed {
                        ("CONFIRM DISCARD", BtnVariant::Danger)
                    } else {
                        ("CANCEL", BtnVariant::Ink)
                    };
                    if theme::brutal_button(ui, label, variant) {
                        if dirty && !editor.confirm_discard {
                            editor.confirm_discard = true;
                        } else {
                            action = SettingsAction::Cancel;
                        }
                    }
                });
            });


            ui.label(
                egui::RichText::new("SETTINGS")
                    .font(theme::display_font(30.0))
                    .color(theme::PAPER),
            );
            ui.label(theme::micro_label(
                format!(
                    "CAMERAS.TOML \u{b7} EDITED IN APP \u{b7} {} ENTRIES",
                    editor.rows.len()
                ),
                theme::LABEL_ON_PAPER,
            ));
            ui.add_space(8.0);

            if let Some(err) = &editor.error {
                error_line(ui, err);
                ui.add_space(4.0);
            }

            // Every setting scrolls together. Keeping these two controls
            // outside the scroll area cost it ~130pt of fixed height, which
            // was enough to make a two-camera list scroll on a 720p window
            // even though it comfortably fits the page.
            let mut delete = None;
            let scrolled = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    badge_position_row(ui, editor);
                    ui.add_space(14.0);
                    update_check_row(ui, editor);
                    ui.add_space(14.0);

                    for (i, row) in editor.rows.iter_mut().enumerate() {
                        settings_row(ui, i, row, &mut delete);
                        ui.add_space(10.0);
                    }

                    ui.collapsing("How do I find the camera URL?", |ui| {
                        ui.label("Most IP cameras expose an RTSP stream. The URL looks like:");
                        ui.monospace("rtsp://USER:PASSWORD@CAMERA_IP:554/PATH");
                        ui.add_space(4.0);
                        ui.label(
                            "To find the IP, check your router's DHCP client list or scan the subnet:",
                        );
                        ui.monospace("nmap -sn 192.168.1.0/24  # adjust to your subnet");
                        ui.add_space(4.0);
                        ui.label("To discover the exact path and credentials, try:");
                        ui.add_space(2.0);
                        for hint in [
                            "The camera's web interface (http://CAMERA_IP) usually documents \
                             its RTSP path under video/stream settings.",
                            "Common paths by brand: Dahua/Hikvision /live/ch0 or \
                             /Streaming/Channels/101, TP-Link /stream1, generic ONVIF /onvif1.",
                            "Test a candidate URL before saving it here with ffplay:\n\
                             ffplay -rtsp_transport tcp \"rtsp://CAMERA_IP/live/ch0\"",
                        ] {
                            ui.horizontal(|ui| {
                                ui.label("\u{2022}");
                                ui.label(hint);
                            });
                        }
                    });
                    ui.add_space(12.0);

                    let add_width = ui.available_width();
                    if theme::brutal_button_sized(
                        ui,
                        egui::vec2(add_width, 34.0),
                        "+ ADD CAMERA",
                        BtnVariant::Confirm,
                    ) {
                        editor.add_row();
                    }
                });

            editor.pending_delete = delete;
            LAST_SETTINGS_FIT.with(|cell| {
                cell.set(Some((scrolled.content_size.y, scrolled.inner_rect.height())));
            });
        });
    action
}

/// Captioned segmented control: the selected option reads as Paper, the rest
/// as Ink. Shared by every two-option setting so they stay visually identical.
fn segmented_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    caption: &str,
    current: &mut T,
    options: &[(T, &str)],
) {
    ui.label(theme::micro_label(caption, theme::LABEL_ON_PAPER));
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for (option, label) in options {
            let selected = *current == *option;
            if theme::brutal_button(
                ui,
                label,
                if selected {
                    BtnVariant::Paper
                } else {
                    BtnVariant::Ink
                },
            ) {
                *current = *option;
            }
            ui.add_space(6.0);
        }
    });
}

/// Segmented two-option control for the grid-tile badge corner.
fn badge_position_row(ui: &mut egui::Ui, editor: &mut SettingsEditor) {
    segmented_row(
        ui,
        "BADGE POSITION",
        &mut editor.badge_position,
        &[
            (BadgePosition::TopRight, "TOP RIGHT"),
            (BadgePosition::BottomRight, "BOTTOM RIGHT"),
        ],
    );
}

/// Opt-out for the once-per-start release check. Worth surfacing because this
/// app runs on surveillance networks where an outbound request is a decision
/// the operator should own, not something buried in a TOML file.
fn update_check_row(ui: &mut egui::Ui, editor: &mut SettingsEditor) {
    segmented_row(
        ui,
        "CHECK FOR UPDATES ON START",
        &mut editor.update_check,
        &[(true, "ON"), (false, "OFF")],
    );
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new("Asks GitHub once whether a newer release exists. Never downloads.")
            .font(theme::mono_font(11.0))
            .color(theme::ASH),
    );
}

fn settings_row(
    ui: &mut egui::Ui,
    index: usize,
    row: &mut SettingsRow,
    delete: &mut Option<usize>,
) {
    // An empty URL isn't an error — it just means the row is incomplete and
    // `collect()` will silently skip it on save. A brand-new row starts this
    // way, so it gets a calm note rather than the loud red "invalid" look.
    let empty = row.url.trim().is_empty();
    let card = egui::Frame::new()
        .fill(theme::PAPER_2)
        .stroke(egui::Stroke::new(2.0_f32, theme::INK))
        .inner_margin(egui::Margin::same(12));
    card.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(theme::micro_label("NAME", theme::LABEL_ON_PAPER));
                ui.add_sized([170.0, 28.0], egui::TextEdit::singleline(&mut row.name));
            });
            ui.vertical(|ui| {
                ui.label(theme::micro_label("RTSP URL", theme::LABEL_ON_PAPER));
                let url_edit = egui::TextEdit::singleline(&mut row.url)
                    .hint_text("rtsp://user:pass@ip:554/path")
                    .font(theme::mono_font(11.5))
                    .text_color(theme::INK);
                let width = (ui.available_width() - 92.0).max(160.0);
                ui.add_sized([width, 28.0], url_edit);
                if empty {
                    ui.label(theme::micro_label(
                        "EMPTY \u{2014} WON'T BE SAVED",
                        theme::LABEL_ON_PAPER,
                    ));
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::brutal_button(ui, "DELETE", BtnVariant::Danger) && delete.is_none() {
                    *delete = Some(index);
                }
            });
        });
    });
}

fn paper_visuals(ui: &mut egui::Ui) {
    let v = ui.visuals_mut();
    v.override_text_color = Some(theme::INK);
    v.extreme_bg_color = theme::PAPER;
    v.faint_bg_color = theme::PAPER_2;
    v.selection.bg_fill = theme::INK;
    v.selection.stroke = egui::Stroke::new(1.0_f32, theme::STATUS_OFFLINE);
    v.error_fg_color = theme::STATUS_OFFLINE;
    v.warn_fg_color = theme::STATUS_CONNECTING;
    v.weak_text_color = Some(theme::LABEL_ON_PAPER);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, theme::INK);
    for state in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        state.bg_stroke = egui::Stroke::new(2.0_f32, theme::INK);
        state.fg_stroke = egui::Stroke::new(1.0_f32, theme::INK);
        // Buttons/ComboBox paint their background with `weak_bg_fill`, not
        // `bg_fill` (that one's for checkboxes/sliders) — without this they
        // kept the dark global theme's fill and looked disabled on paper.
        state.weak_bg_fill = theme::PAPER_2;
    }
    v.widgets.inactive.bg_fill = theme::PAPER_2;
    v.widgets.hovered.bg_fill = theme::PAPER_2;
    v.widgets.active.bg_fill = theme::PAPER_2;
}

// ---------------------------------------------------------------------------
// Texture helpers
// ---------------------------------------------------------------------------

fn ensure_texture(
    ctx: &egui::Context,
    cam: &mut CameraView,
    frame: &Arc<crate::stream::Frame>,
) -> egui::TextureHandle {
    if let Some((generation, tex)) = &cam.texture
        && *generation == frame.generation
    {
        return tex.clone();
    }
    let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
    let tex = ctx.load_texture(
        format!("cam-{}", cam.name),
        image,
        egui::TextureOptions::LINEAR,
    );
    cam.texture = Some((frame.generation, tex.clone()));
    tex
}

fn fit_rect(rect: egui::Rect, w: f32, h: f32) -> egui::Rect {
    if w <= 0.0 || h <= 0.0 {
        return rect;
    }
    let scale = (rect.width() / w).min(rect.height() / h);
    egui::Rect::from_center_size(rect.center(), egui::vec2(w * scale, h * scale))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::RowStatus;
    use std::net::Ipv4Addr;

    fn result(ip: [u8; 4], url: Option<&str>, auth: RowStatus) -> DiscoveryResult {
        DiscoveryResult {
            ip: Ipv4Addr::from(ip),
            port: 554,
            url: url.map(str::to_owned),
            vendor: None,
            resolution: None,
            auth,
        }
    }

    #[test]
    fn preview_remembers_the_exact_url_it_was_started_with() {
        // The row decides whether to show PREVIEW or HIDE PREVIEW by comparing
        // this string with effective_url(). Any rewriting here and the button
        // would never flip to HIDE, leaving a stream the user cannot stop.
        let url = "rtsp://192.168.100.22:554/live/ch0";
        let preview = Preview::start(url.to_owned());
        assert_eq!(preview.url, url);
        preview.stream.stop();
    }

    #[test]
    fn a_relinked_row_still_offers_a_preview_url() {
        // Preview is only useful on exactly the rows that are ambiguous, so
        // choosing a relink target must not remove the URL it previews.
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 22],
            "rtsp://192.168.100.22:554/live/ch0",
        ));
        row.relink_to = Some("Lateral".to_owned());
        assert_eq!(
            row.effective_url().as_deref(),
            Some("rtsp://192.168.100.22:554/live/ch0")
        );
    }

    fn found(ip: [u8; 4], url: &str) -> DiscoveryResult {
        DiscoveryResult {
            ip: Ipv4Addr::from(ip),
            port: 554,
            url: Some(url.to_owned()),
            vendor: Some("Generic".to_owned()),
            resolution: Some((1920, 1080)),
            auth: RowStatus::Open,
        }
    }

    #[test]
    fn a_camera_rediscovered_at_a_new_address_is_offered_as_a_relink() {
        let existing = vec![camera("Lateral", "rtsp://192.168.100.5:554/live/ch0")];
        let result = found([192, 168, 100, 22], "rtsp://192.168.100.22:554/live/ch0");
        let candidates = relink_candidates(&result, &existing, &[result.ip]);
        assert_eq!(candidates, vec!["Lateral".to_owned()]);
    }

    #[test]
    fn a_camera_still_answering_at_its_own_address_is_not_a_move() {
        // The old address responded in this very scan, so nothing moved and
        // offering to relink would corrupt a working configuration.
        let existing = vec![camera("Lateral", "rtsp://192.168.100.5:554/live/ch0")];
        let result = found([192, 168, 100, 22], "rtsp://192.168.100.22:554/live/ch0");
        let both_alive = [Ipv4Addr::new(192, 168, 100, 5), result.ip];
        assert!(relink_candidates(&result, &existing, &both_alive).is_empty());
    }

    #[test]
    fn a_different_stream_path_is_never_a_move() {
        let existing = vec![camera("Lateral", "rtsp://192.168.100.5:554/live/ch0")];
        let result = found([192, 168, 100, 22], "rtsp://192.168.100.22:554/other/path");
        assert!(relink_candidates(&result, &existing, &[result.ip]).is_empty());
    }

    #[test]
    fn two_identical_cameras_that_both_moved_stay_ambiguous() {
        // The real case: both cameras use /live/ch0 and both changed address.
        // Each result matches both configured cameras, and picking one would
        // silently swap which physical camera is called what.
        let existing = vec![
            camera("Lateral", "rtsp://192.168.100.5:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let result = found([192, 168, 100, 22], "rtsp://192.168.100.22:554/live/ch0");
        let candidates = relink_candidates(&result, &existing, &[result.ip]);
        assert_eq!(
            candidates,
            vec!["Lateral".to_owned(), "Cochera".to_owned()],
            "both must be offered so the user decides"
        );
    }

    #[test]
    fn relink_moves_the_url_and_keeps_the_name() {
        let mut existing = vec![
            camera("Lateral", "rtsp://192.168.100.5:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 22],
            "rtsp://192.168.100.22:554/live/ch0",
        ));
        row.checked = true;
        row.relink_to = Some("Lateral".to_owned());

        assert_eq!(apply_relinks(&mut existing, &[row]), 1);
        assert_eq!(existing[0].name, "Lateral", "name survives the move");
        assert_eq!(existing[0].url, "rtsp://192.168.100.22:554/live/ch0");
        assert_eq!(
            existing[1].url, "rtsp://192.168.100.6:554/live/ch0",
            "the other camera is untouched"
        );
        assert_eq!(existing.len(), 2, "relink replaces, never appends");
    }

    #[test]
    fn relinking_onto_an_address_another_camera_holds_swaps_them() {
        // The correction path for a mis-assigned pair: Lateral was pointed at
        // the camera that is really the Cochera. Relinking Cochera here must
        // trade addresses, not leave both cameras on the same one.
        let mut existing = vec![
            camera("Lateral", "rtsp://192.168.100.11:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 11],
            "rtsp://192.168.100.11:554/live/ch0",
        ));
        row.checked = true;
        row.exact_duplicate = true; // the address is already configured
        row.relink_to = Some("Cochera".to_owned());

        assert_eq!(apply_relinks(&mut existing, &[row]), 1);
        assert_eq!(existing[1].url, "rtsp://192.168.100.11:554/live/ch0");
        assert_eq!(
            existing[0].url, "rtsp://192.168.100.6:554/live/ch0",
            "Lateral takes the address Cochera vacated"
        );
        assert_eq!(existing[0].name, "Lateral");
        assert_eq!(existing[1].name, "Cochera");
    }

    #[test]
    fn an_already_configured_address_stays_actionable_when_relinked() {
        // The bug this fixes: the checkbox was dead on exactly the row needed
        // to correct a swap, because "already configured" was treated as if
        // the only possible action were adding a new camera.
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 11],
            "rtsp://192.168.100.11:554/live/ch0",
        ));
        row.exact_duplicate = true;
        assert!(!row.addable(), "adding a configured address is still a no-op");

        row.relink_to = Some("Cochera".to_owned());
        assert!(row.addable(), "relinking it is a real action");
    }

    #[test]
    fn a_relink_onto_a_free_address_displaces_nobody() {
        let mut existing = vec![
            camera("Lateral", "rtsp://192.168.100.5:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 22],
            "rtsp://192.168.100.22:554/live/ch0",
        ));
        row.checked = true;
        row.relink_to = Some("Lateral".to_owned());

        apply_relinks(&mut existing, &[row]);
        assert_eq!(existing[0].url, "rtsp://192.168.100.22:554/live/ch0");
        assert_eq!(
            existing[1].url, "rtsp://192.168.100.6:554/live/ch0",
            "the untouched camera keeps its address"
        );
    }

    #[test]
    fn the_swap_partner_is_reported_for_the_row_note() {
        let existing = vec![
            camera("Lateral", "rtsp://192.168.100.11:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let results = vec![found(
            [192, 168, 100, 11],
            "rtsp://192.168.100.11:554/live/ch0",
        )];
        let mut rows = Vec::new();
        reconcile_rows(&mut rows, &results, &existing);
        rows[0].relink_to = Some("Cochera".to_owned());
        reconcile_rows(&mut rows, &results, &existing);
        assert_eq!(rows[0].relink_swaps_with.as_deref(), Some("Lateral"));
    }

    #[test]
    fn an_unchecked_relink_changes_nothing() {
        let mut existing = vec![camera("Lateral", "rtsp://192.168.100.5:554/live/ch0")];
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 22],
            "rtsp://192.168.100.22:554/live/ch0",
        ));
        row.relink_to = Some("Lateral".to_owned());
        row.checked = false;
        assert_eq!(apply_relinks(&mut existing, &[row]), 0);
        assert_eq!(existing[0].url, "rtsp://192.168.100.5:554/live/ch0");
    }

    #[test]
    fn a_relinked_row_is_not_also_appended_as_a_new_camera() {
        // Otherwise the move would leave both a moved camera and a duplicate.
        let mut row = DiscoverRow::new(found(
            [192, 168, 100, 22],
            "rtsp://192.168.100.22:554/live/ch0",
        ));
        row.checked = true;
        row.relink_to = Some("Lateral".to_owned());
        assert!(selected_cameras(&[row], 0).is_empty());
    }

    #[test]
    fn one_unambiguous_candidate_is_preselected_but_two_are_not() {
        let one = vec![camera("Lateral", "rtsp://192.168.100.5:554/live/ch0")];
        let results = vec![found([192, 168, 100, 22], "rtsp://192.168.100.22:554/live/ch0")];
        let mut rows = Vec::new();
        reconcile_rows(&mut rows, &results, &one);
        assert_eq!(rows[0].relink_to.as_deref(), Some("Lateral"));

        let two = vec![
            camera("Lateral", "rtsp://192.168.100.5:554/live/ch0"),
            camera("Cochera", "rtsp://192.168.100.6:554/live/ch0"),
        ];
        let mut rows = Vec::new();
        reconcile_rows(&mut rows, &results, &two);
        assert_eq!(rows[0].relink_to, None, "ambiguous stays unselected");
        assert_eq!(rows[0].relink_options.len(), 2);
    }

    fn unknown_path_row(ip: [u8; 4], port: u16) -> DiscoverRow {
        DiscoverRow::new(DiscoveryResult {
            ip: Ipv4Addr::from(ip),
            port,
            url: None,
            vendor: None,
            resolution: None,
            auth: RowStatus::PathUnknown,
        })
    }

    fn editor_with(urls: &[(&str, &str)]) -> SettingsEditor {
        SettingsEditor::from_config(&Config {
            badge_position: BadgePosition::BottomRight,
            update_check: true,
            cameras: urls
                .iter()
                .map(|(name, url)| camera(name, url))
                .collect(),
        })
    }

    #[test]
    fn a_queued_delete_removes_the_row_and_marks_the_editor_dirty() {
        // The reported bug: a camera could not be deleted. SAVE must become
        // enabled in the same frame the row disappears, or the deletion looks
        // like it did nothing.
        let mut editor = editor_with(&[
            ("Lateral", "rtsp://192.168.100.10:554/live/ch0"),
            ("Cochera", "rtsp://192.168.100.11:554/live/ch0"),
        ]);
        assert!(!editor.is_dirty(), "untouched editor is clean");

        editor.pending_delete = Some(0);
        editor.apply_pending_delete();

        assert_eq!(editor.rows.len(), 1);
        assert_eq!(editor.rows[0].name, "Cochera");
        assert!(editor.is_dirty(), "SAVE must be enabled right away");
        assert_eq!(editor.pending_delete, None, "the request is consumed");
    }

    #[test]
    fn a_stale_delete_index_is_ignored_rather_than_panicking() {
        let mut editor = editor_with(&[("Lateral", "rtsp://192.168.100.10:554/live/ch0")]);
        editor.pending_delete = Some(7);
        editor.apply_pending_delete();
        assert_eq!(editor.rows.len(), 1, "nothing removed");
        assert_eq!(editor.pending_delete, None);
    }

    #[test]
    fn deleting_every_row_leaves_a_saveable_empty_list() {
        // Removing the last camera must still be committable, otherwise a
        // user cannot clear a bad configuration.
        let mut editor = editor_with(&[("Lateral", "rtsp://192.168.100.10:554/live/ch0")]);
        editor.pending_delete = Some(0);
        editor.apply_pending_delete();
        assert!(editor.rows.is_empty());
        assert!(editor.is_dirty());
        assert!(editor.collect().is_empty());
    }

    #[test]
    fn typed_path_is_normalised_to_one_leading_slash() {
        assert_eq!(normalized_path("/media/video1").as_deref(), Some("/media/video1"));
        assert_eq!(normalized_path("media/video1").as_deref(), Some("/media/video1"));
        assert_eq!(normalized_path("  /live.sdp  ").as_deref(), Some("/live.sdp"));
        assert_eq!(normalized_path("//live").as_deref(), Some("/live"));
    }

    #[test]
    fn input_that_names_no_path_is_rejected() {
        // A bare "/" is already in the vendor table and already failed, so it
        // cannot be the answer for a PathUnknown host.
        for raw in ["", "   ", "/", "//", "\t"] {
            assert_eq!(normalized_path(raw), None, "must reject {raw:?}");
        }
    }

    #[test]
    fn unknown_path_row_is_not_addable_until_a_path_is_typed() {
        let mut row = unknown_path_row([192, 168, 1, 90], 554);
        assert!(!row.addable(), "nothing typed yet");
        assert_eq!(row.effective_url(), None);

        row.manual_path = "media/video1".to_owned();
        assert!(row.addable(), "a typed path makes the row addable");
        assert_eq!(
            row.effective_url().as_deref(),
            Some("rtsp://192.168.1.90:554/media/video1")
        );
    }

    #[test]
    fn typed_path_url_uses_the_port_that_actually_responded() {
        // Hardcoding 554 would silently produce a dead URL for this camera.
        let mut row = unknown_path_row([10, 0, 0, 7], 8554);
        row.manual_path = "/live.sdp".to_owned();
        assert_eq!(
            row.effective_url().as_deref(),
            Some("rtsp://10.0.0.7:8554/live.sdp")
        );
    }

    #[test]
    fn credentials_row_stays_unaddable_even_with_a_typed_path() {
        // Only PathUnknown accepts a hand-typed path; a host that answered
        // BadCredentials still needs credentials and a rescan.
        let mut row = DiscoverRow::new(result([192, 168, 1, 91], None, RowStatus::NeedsCredentials));
        row.manual_path = "/live".to_owned();
        assert_eq!(row.effective_url(), None);
        assert!(!row.addable());
    }

    #[test]
    fn typed_path_reaches_the_saved_camera_list() {
        let mut row = unknown_path_row([192, 168, 1, 92], 554);
        row.manual_path = "/cam/stream".to_owned();
        row.checked = true;
        let cameras = selected_cameras(&[row], 0);
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].url, "rtsp://192.168.1.92:554/cam/stream");
    }

    #[test]
    fn a_typed_path_survives_a_results_refresh() {
        // reconcile_rows runs every frame while scanning; losing the field
        // mid-typing would make it unusable.
        let mut rows = vec![unknown_path_row([192, 168, 1, 93], 554)];
        rows[0].manual_path = "/half-typ".to_owned();
        let refreshed = vec![rows[0].result.clone()];
        reconcile_rows(&mut rows, &refreshed, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].manual_path, "/half-typ");
    }

    fn camera(name: &str, url: &str) -> CameraConfig {
        CameraConfig {
            name: name.to_owned(),
            url: url.to_owned(),
        }
    }

    #[test]
    fn url_host_strips_scheme_userinfo_port_and_path() {
        assert_eq!(
            url_host("rtsp://admin:s3cret@192.168.1.64:554/Streaming/Channels/101").as_deref(),
            Some("192.168.1.64")
        );
        assert_eq!(
            url_host("rtsp://192.168.1.10/live").as_deref(),
            Some("192.168.1.10")
        );
        assert_eq!(
            url_host("rtsp://user:p@ss@10.0.0.5:8554/onvif1").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(url_host("not-a-url"), None);
    }

    #[test]
    fn global_creds_require_username_and_trim_both_fields() {
        assert_eq!(global_creds("", ""), None);
        assert_eq!(global_creds("   ", "s3cret"), None);
        assert_eq!(global_creds("", "s3cret"), None);
        assert_eq!(
            global_creds(" admin ", " s3cret "),
            Some(("admin".to_owned(), "s3cret".to_owned()))
        );
        // Blank passwords are legal in rtsp://user:@host URLs.
        assert_eq!(
            global_creds("admin", ""),
            Some(("admin".to_owned(), String::new()))
        );
    }

    #[test]
    fn merge_cameras_appends_new_and_skips_exact_url_duplicates() {
        let existing = vec![camera("Front", "rtsp://192.168.1.10/live")];
        let extra = vec![
            camera("Back", "rtsp://192.168.1.11/live"),
            // Exact duplicate of an existing URL: dropped.
            camera("Front copy", "rtsp://192.168.1.10/live"),
            // Exact duplicate within extra itself: first wins.
            camera("Back 2", "rtsp://192.168.1.11/live"),
        ];
        let merged = merge_cameras(&existing, extra);
        assert_eq!(
            merged,
            vec![
                camera("Front", "rtsp://192.168.1.10/live"),
                camera("Back", "rtsp://192.168.1.11/live"),
            ]
        );
    }

    #[test]
    fn merge_cameras_treats_default_and_explicit_rtsp_port_as_the_same_camera() {
        // Manually-configured URLs usually omit the default RTSP port (554);
        // discovery always states it explicitly. A raw string comparison
        // missed this and re-added the same physical camera as a duplicate.
        let existing = vec![camera("Cochera", "rtsp://192.168.100.6/live/ch0")];
        let extra = vec![camera("Generic 192.168.100.6", "rtsp://192.168.100.6:554/live/ch0")];
        assert_eq!(merge_cameras(&existing, extra), existing);
    }

    #[test]
    fn discovered_name_uses_vendor_guess_or_sequential_fallback() {
        let ip = Ipv4Addr::new(192, 168, 1, 64);
        assert_eq!(
            discovered_name(Some("Hikvision".to_owned()).as_deref(), ip, 3),
            "Hikvision 192.168.1.64"
        );
        assert_eq!(discovered_name(None, ip, 3), "Cam 3");
    }

    #[test]
    fn selected_cameras_collects_only_checked_addable_rows_with_names() {
        let mut hit = DiscoverRow::new(result(
            [192, 168, 1, 64],
            Some("rtsp://192.168.1.64:554/x"),
            RowStatus::Authenticated,
        ));
        hit.result.vendor = Some("Hikvision".to_owned());
        hit.checked = true;
        let unchecked = DiscoverRow::new(result(
            [192, 168, 1, 65],
            Some("rtsp://192.168.1.65/y"),
            RowStatus::Open,
        ));
        let mut already = DiscoverRow::new(result(
            [192, 168, 1, 66],
            Some("rtsp://192.168.1.66/z"),
            RowStatus::Authenticated,
        ));
        already.checked = true;
        already.duplicate = true;
        already.exact_duplicate = true;
        let mut needs_creds = DiscoverRow::new(result(
            [192, 168, 1, 67],
            None,
            RowStatus::NeedsCredentials,
        ));
        needs_creds.checked = true;

        let rows = vec![hit, unchecked, already, needs_creds];
        assert_eq!(
            selected_cameras(&rows, 2),
            vec![camera(
                "Hikvision 192.168.1.64",
                "rtsp://192.168.1.64:554/x"
            )]
        );
    }

    #[test]
    fn reconcile_rows_creates_unchecked_rows_and_flags_duplicates() {
        let existing = vec![
            camera("Front", "rtsp://192.168.1.10:554/live"),
            camera("Garage", "rtsp://admin:pw@192.168.1.20:8554/open"),
        ];
        let results = vec![
            result(
                [192, 168, 1, 10],
                Some("rtsp://192.168.1.10:554/live"),
                RowStatus::Open,
            ),
            result(
                [192, 168, 1, 20],
                Some("rtsp://192.168.1.20:554/onvif1"),
                RowStatus::Authenticated,
            ),
            result([192, 168, 1, 40], None, RowStatus::NeedsCredentials),
        ];
        let mut rows = Vec::new();
        reconcile_rows(&mut rows, &results, &existing);

        assert_eq!(rows.len(), 3);
        // Exact-string URL match against a configured camera.
        assert!(rows[0].duplicate && rows[0].exact_duplicate && !rows[0].checked);
        // Same host as a configured camera but a different URL: only
        // informational.
        assert!(rows[1].duplicate && !rows[1].exact_duplicate && !rows[1].checked);
        assert!(!rows[2].duplicate && !rows[2].checked);
    }

    #[test]
    fn reconcile_rows_preserves_checked_state_across_frames_and_drops_gone_results() {
        let existing = vec![];
        let first = vec![
            result(
                [192, 168, 1, 30],
                Some("rtsp://192.168.1.30/a"),
                RowStatus::Open,
            ),
            result(
                [192, 168, 1, 31],
                Some("rtsp://192.168.1.31/b"),
                RowStatus::Open,
            ),
        ];
        let mut rows = Vec::new();
        reconcile_rows(&mut rows, &first, &existing);
        rows[0].checked = true;

        // Same frame-by-frame refresh with identical results: checkbox kept.
        reconcile_rows(&mut rows, &first, &existing);
        assert!(rows[0].checked);

        // A later snapshot carrying fewer results drops vanished rows.
        let later = vec![first[1].clone()];
        reconcile_rows(&mut rows, &later, &existing);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result.ip, Ipv4Addr::new(192, 168, 1, 31));
    }

    /// Live pipeline against TEST-NET-1 (RFC 5737, silently unroutable):
    /// real worker threads, zero network side effects, CI-safe.
    fn live_scan_wizard() -> DiscoverWizard {
        let mut wizard = DiscoverWizard {
            interfaces: Vec::new(),
            selected_iface: 0,
            username: String::new(),
            password: String::new(),
            handle: None,
            latest: None,
            rows: Vec::new(),
            error: None,
            cancelling: false,
            ws: None,
            preview: None,
        };
        wizard.handle = Some(DiscoveryHandle::start(DiscoveryConfig {
            subnet: net::Subnet {
                network: Ipv4Addr::new(192, 0, 2, 0).to_bits(),
                prefix: 24,
            },
            ports: vec![9],
            creds: None,
            probe_timeout: Duration::from_millis(250),
        }));
        wizard
    }

    fn headless_app() -> CamViewerApp {
        CamViewerApp::new(&Config::default())
    }

    #[test]
    fn end_detached_releases_a_live_pipeline_without_blocking_the_caller() {
        let mut wizard = live_scan_wizard();

        let started = Instant::now();
        wizard.end_detached();
        let elapsed = started.elapsed();
        assert!(wizard.handle.is_none());
        // A synchronous Drop-join of this pipeline costs at least one 250 ms
        // wsdiscovery poll tick plus scan wind-down; the closer thread must
        // absorb that instead of the caller.
        assert!(
            elapsed < Duration::from_millis(500),
            "end_detached blocked its caller for {elapsed:?}"
        );

        // Idempotent: no second handle, instant no-op.
        let again = Instant::now();
        wizard.end_detached();
        assert!(again.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn close_discover_ends_the_wizard_and_navigates_to_grid_without_blocking() {
        let mut app = headless_app();
        app.view = View::Discover;
        app.discover = Some(live_scan_wizard());

        let started = Instant::now();
        app.close_discover();

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "close_discover blocked its caller"
        );
        assert!(app.discover.is_none());
        assert!(matches!(app.view, View::Grid));
    }

    #[test]
    fn abandon_discover_cancels_an_in_flight_scan_from_any_non_discover_view() {
        for view in [View::Grid, View::Solo(0), View::Settings] {
            let mut app = headless_app();
            app.view = view;
            app.discover = Some(live_scan_wizard());

            let started = Instant::now();
            app.abandon_discover();

            assert!(
                started.elapsed() < Duration::from_millis(500),
                "abandon_discover blocked its caller"
            );
            assert!(app.discover.is_none());
            // Navigation itself stays untouched; only the wizard ends.
            assert!(!matches!(app.view, View::Discover));
        }
    }
}

#[cfg(test)]
mod settings_layout_tests {
    use super::*;

    /// Renders Settings headlessly and returns (content height, viewport
    /// height) for the scroll area.
    fn render(cameras: usize, window_height: f32) -> (f32, f32) {
        let cfg = Config {
            badge_position: BadgePosition::BottomRight,
            update_check: true,
            cameras: (0..cameras)
                .map(|i| CameraConfig {
                    name: format!("Cam {i}"),
                    url: format!("rtsp://192.168.100.{}:554/live/ch0", 10 + i),
                })
                .collect(),
        };
        let mut editor = SettingsEditor::from_config(&cfg);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1366.0, window_height),
            )),
            ..Default::default()
        };
        // Three frames: egui settles panel sizes over the first two.
        for _ in 0..3 {
            let _ = ctx.run(input.clone(), |ctx| {
                let _ = show_settings(ctx, &mut editor);
            });
        }
        LAST_SETTINGS_FIT
            .with(|cell| cell.get())
            .expect("Settings rendered at least once")
    }

    #[test]
    fn a_short_camera_list_needs_no_scrolling() {
        // The regression this guards: BADGE POSITION and CHECK FOR UPDATES
        // used to sit outside the scroll area, costing it ~160pt of fixed
        // height and making a two-camera list scroll on an ordinary window.
        for height in [687.0, 720.0, 800.0] {
            let (content, viewport) = render(2, height);
            assert!(
                content <= viewport,
                "2 cameras must fit at window height {height}: \
                 content {content}pt vs viewport {viewport}pt"
            );
        }
    }

    #[test]
    fn a_long_camera_list_still_scrolls() {
        // The complement: the scroll area must still do its job, or a long
        // list would simply be unreachable.
        let (content, viewport) = render(8, 687.0);
        assert!(
            content > viewport,
            "8 cameras should overflow: content {content}pt vs viewport {viewport}pt"
        );
    }

    #[test]
    fn every_setting_scrolls_with_the_camera_list() {
        // If the controls were moved back out of the scroll area, the content
        // measured here would shrink by their height.
        let (with_controls, _) = render(0, 687.0);
        assert!(
            with_controls > 150.0,
            "the settings controls belong inside the scrolled content, \
             measured only {with_controls}pt"
        );
    }
}
