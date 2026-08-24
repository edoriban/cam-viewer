use crate::config::{self, CameraConfig, Config};
use crate::stream::{Status, StreamHandle};
use eframe::egui;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPAINT_INTERVAL: Duration = Duration::from_millis(50);
const PAUSED_REPAINT_INTERVAL: Duration = Duration::from_millis(1000);
const PAUSE_AFTER_UNFOCUSED: Duration = Duration::from_secs(10);
const NAME_FIELD_WIDTH: f32 = 140.0;

enum View {
    Grid,
    Solo(usize),
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
    error: Option<String>,
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
            error: None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusChange {
    None,
    EnteredPause,
    ExitedPause,
}

#[derive(Debug)]
struct FocusTracker {
    unfocused_since: Option<Instant>,
    paused: bool,
}

impl FocusTracker {
    fn new() -> Self {
        Self {
            unfocused_since: None,
            paused: false,
        }
    }

    fn observe(&mut self, active: bool, now: Instant) -> FocusChange {
        if active {
            self.unfocused_since = None;
            return if self.paused {
                self.paused = false;
                FocusChange::ExitedPause
            } else {
                FocusChange::None
            };
        }
        match self.unfocused_since {
            None => self.unfocused_since = Some(now),
            Some(since)
                if !self.paused && now.duration_since(since) >= PAUSE_AFTER_UNFOCUSED =>
            {
                self.paused = true;
                return FocusChange::EnteredPause;
            }
            Some(_) => {}
        }
        FocusChange::None
    }

    fn paused(&self) -> bool {
        self.paused
    }
}

pub struct CamViewerApp {
    cameras: Vec<CameraView>,
    view: View,
    focus: FocusTracker,
    settings: SettingsEditor,
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
            focus: FocusTracker::new(),
            settings: SettingsEditor::from_config(config),
        }
    }

    fn status_color(status: Status) -> egui::Color32 {
        match status {
            Status::Online => egui::Color32::from_rgb(76, 175, 80),
            Status::Connecting => egui::Color32::from_rgb(255, 193, 7),
            Status::Offline => egui::Color32::from_rgb(244, 67, 54),
            Status::Paused => egui::Color32::from_rgb(158, 158, 158),
        }
    }

    fn set_paused(&mut self, paused: bool) {
        for cam in &mut self.cameras {
            cam.stream.set_paused(paused);
            if paused {
                cam.texture = None;
            }
        }
    }

    fn open_settings(&mut self) {
        self.settings = SettingsEditor::from_config(&Config {
            cameras: self
                .cameras
                .iter()
                .map(|cam| CameraConfig {
                    name: cam.name.clone(),
                    url: cam.url.clone(),
                })
                .collect(),
        });
        self.view = View::Settings;
    }

    fn cancel_settings(&mut self) {
        self.settings.error = None;
        self.view = View::Grid;
    }

    fn save_settings(&mut self) {
        self.settings.error = None;
        let desired = self.settings.collect();
        if let Err(err) = config::save(&config::config_path(), &Config { cameras: desired.clone() })
        {
            self.settings.error = Some(format!("Failed to save config: {err:#}"));
            return;
        }
        self.apply_cameras(desired);
        self.view = View::Grid;
    }

    fn apply_cameras(&mut self, desired: Vec<CameraConfig>) {
        let mut pool: Vec<Option<CameraView>> =
            std::mem::take(&mut self.cameras).into_iter().map(Some).collect();
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
        self.set_paused(self.focus.paused());
    }
}

impl eframe::App for CamViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let active = ctx.input(|i| {
            let viewport = i.viewport();
            viewport.focused.unwrap_or(true) && !viewport.minimized.unwrap_or(false)
        });
        match self.focus.observe(active, Instant::now()) {
            FocusChange::EnteredPause => self.set_paused(true),
            FocusChange::ExitedPause => self.set_paused(false),
            FocusChange::None => {}
        }

        let paused = self.focus.paused();

        if matches!(self.view, View::Settings)
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.cancel_settings();
        }

        egui::SidePanel::left("devices").show(ctx, |ui| {
            ui.heading("Cameras");
            ui.add_space(8.0);
            if ui
                .selectable_label(matches!(self.view, View::Grid), "Grid view")
                .clicked()
            {
                self.view = View::Grid;
            }
            if ui
                .selectable_label(matches!(self.view, View::Settings), "Settings")
                .clicked()
            {
                self.open_settings();
            }

            for (i, cam) in self.cameras.iter().enumerate() {
                let status = cam.stream.status();
                let selected = matches!(&self.view, View::Solo(s) if *s == i);
                let response =
                    ui.selectable_label(selected, format!("{}   {}", " ".repeat(4), cam.name));
                let center = egui::pos2(response.rect.left() + 10.0, response.rect.center().y);
                ui.painter()
                    .circle_filled(center, 4.0, Self::status_color(status));

                if response.clicked() {
                    self.view = View::Solo(i);
                }
                if selected {
                    ui.label(
                        egui::RichText::new(format!("     {}", status.label()))
                            .small()
                            .weak(),
                    );
                }
            }
        });

        match self.view {
            View::Grid => {
                if let Some(index) = show_grid(ctx, &mut self.cameras, paused) {
                    self.view = View::Solo(index);
                }
            }
            View::Solo(index) => {
                if index >= self.cameras.len()
                    || show_solo(ctx, &mut self.cameras[index], paused)
                {
                    self.view = View::Grid;
                }
            }
            View::Settings => match show_settings(ctx, &mut self.settings) {
                SettingsAction::Save => self.save_settings(),
                SettingsAction::Cancel => self.cancel_settings(),
                SettingsAction::None => {}
            },
        }

        ctx.request_repaint_after(if paused {
            PAUSED_REPAINT_INTERVAL
        } else {
            REPAINT_INTERVAL
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for cam in &self.cameras {
            cam.stream.stop();
        }
    }
}

fn show_grid(ctx: &egui::Context, cameras: &mut [CameraView], paused: bool) -> Option<usize> {
    let mut opened = None;
    egui::CentralPanel::default().show(ctx, |ui| {
        let avail = ui.available_size();
        let spacing = 12.0;
        let cell = egui::vec2(avail.x, (avail.y / cameras.len() as f32 - spacing).max(40.0));

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, cam) in cameras.iter_mut().enumerate() {
                if i > 0 {
                    ui.add_space(spacing);
                }
                if tile(ui, cam, cell, paused) && opened.is_none() {
                    opened = Some(i);
                }
            }
        });
    });
    opened
}

fn show_solo(ctx: &egui::Context, cam: &mut CameraView, paused: bool) -> bool {
    let mut back = false;
    egui::TopBottomPanel::top("solo_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            back |= ui.button("< Back").clicked();
            ui.heading(&cam.name);
            let status = cam.stream.status();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(
                    CamViewerApp::status_color(status),
                    status.label(),
                );
            });
        });
    });
    egui::CentralPanel::default().show(ctx, |ui| {
        tile(ui, cam, ui.available_size(), paused);
    });
    back
}

fn show_settings(ctx: &egui::Context, editor: &mut SettingsEditor) -> SettingsAction {
    let mut action = SettingsAction::None;
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Settings");
        ui.add_space(8.0);

        if let Some(err) = &editor.error {
            ui.colored_label(egui::Color32::from_rgb(244, 67, 54), err.to_string());
            ui.add_space(4.0);
        }

        let mut delete = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, row) in editor.rows.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [NAME_FIELD_WIDTH, 22.0],
                        egui::TextEdit::singleline(&mut row.name),
                    );
                    let invalid = row.url.trim().is_empty();
                    let mut url_edit = egui::TextEdit::singleline(&mut row.url)
                        .hint_text("rtsp://...");
                    if invalid {
                        url_edit = url_edit.text_color(egui::Color32::from_rgb(244, 67, 54));
                    }
                    let width = (ui.available_width() - 90.0).max(120.0);
                    ui.add_sized([width, 22.0], url_edit);
                    if ui.button("Delete").clicked() && delete.is_none() {
                        delete = Some(i);
                    }
                });
            }
        });
        if let Some(i) = delete {
            editor.rows.remove(i);
        }
        ui.add_space(8.0);

        ui.collapsing("How do I find the camera URL?", |ui| {
            ui.label("Most IP cameras expose an RTSP stream. The URL looks like:");
            ui.monospace("rtsp://USER:PASSWORD@CAMERA_IP:554/PATH");
            ui.add_space(4.0);
            ui.label("To find the IP, check your router's DHCP client list or scan the subnet:");
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
                    ui.label("•");
                    ui.label(hint);
                });
            }
        });

        if ui.button("Add camera").clicked() {
            editor.add_row();
        }
        ui.add_space(4.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Save").clicked() {
                action = SettingsAction::Save;
            }
            if ui.button("Cancel").clicked() {
                action = SettingsAction::Cancel;
            }
        });
    });
    action
}

fn tile(ui: &mut egui::Ui, cam: &mut CameraView, size: egui::Vec2, paused: bool) -> bool {
    let size = size.max(egui::vec2(60.0, 40.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(22));

    if paused {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{}: paused", cam.name),
            egui::FontId::proportional(16.0),
            egui::Color32::from_gray(140),
        );
    } else {
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
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    msg,
                    egui::FontId::proportional(16.0),
                    egui::Color32::from_gray(140),
                );
            }
        }
    }

    painter.text(
        rect.left_top() + egui::vec2(8.0, 14.0),
        egui::Align2::LEFT_TOP,
        &cam.name,
        egui::FontId::proportional(13.0),
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 210),
    );

    response.clicked()
}

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
    let tex = ctx.load_texture(format!("cam-{}", cam.name), image, egui::TextureOptions::LINEAR);
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

    #[test]
    fn pauses_only_after_grace_period_then_resumes_on_focus() {
        let mut tracker = FocusTracker::new();
        let t0 = Instant::now();

        assert_eq!(tracker.observe(false, t0), FocusChange::None);
        assert!(!tracker.paused());

        assert_eq!(
            tracker.observe(false, t0 + PAUSE_AFTER_UNFOCUSED - Duration::from_secs(1)),
            FocusChange::None
        );
        assert!(!tracker.paused());

        assert_eq!(
            tracker.observe(false, t0 + PAUSE_AFTER_UNFOCUSED),
            FocusChange::EnteredPause
        );
        assert!(tracker.paused());

        assert_eq!(
            tracker.observe(false, t0 + PAUSE_AFTER_UNFOCUSED + Duration::from_secs(30)),
            FocusChange::None
        );
        assert!(tracker.paused());

        assert_eq!(
            tracker.observe(true, t0 + PAUSE_AFTER_UNFOCUSED + Duration::from_secs(31)),
            FocusChange::ExitedPause
        );
        assert!(!tracker.paused());

        assert_eq!(
            tracker.observe(true, t0 + PAUSE_AFTER_UNFOCUSED + Duration::from_secs(32)),
            FocusChange::None
        );
        assert!(!tracker.paused());
    }

    #[test]
    fn refocus_resets_grace_period_without_flapping_pauses() {
        let mut tracker = FocusTracker::new();
        let t0 = Instant::now();

        assert_eq!(tracker.observe(false, t0), FocusChange::None);
        assert_eq!(
            tracker.observe(false, t0 + Duration::from_secs(9)),
            FocusChange::None
        );
        assert_eq!(tracker.observe(true, t0 + Duration::from_secs(10)), FocusChange::None);
        assert_eq!(
            tracker.observe(false, t0 + Duration::from_secs(11)),
            FocusChange::None
        );
        assert_eq!(
            tracker.observe(false, t0 + Duration::from_secs(20)),
            FocusChange::None
        );
        assert_eq!(
            tracker.observe(false, t0 + Duration::from_secs(21)),
            FocusChange::EnteredPause
        );
        assert!(tracker.paused());
    }
}
