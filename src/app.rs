use crate::config::{self, BadgePosition, CameraConfig, Config};
use crate::stream::{Status, StreamHandle};
use crate::theme::{self, BtnVariant};
use eframe::egui;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPAINT_INTERVAL: Duration = Duration::from_millis(50);
const PAUSED_REPAINT_INTERVAL: Duration = Duration::from_millis(1000);
const PAUSE_AFTER_UNFOCUSED: Duration = Duration::from_secs(10);
const SIDEBAR_WIDTH: f32 = 236.0;
const GRID_MIN_TILE_WIDTH: f32 = 360.0;
const GRID_SPACING: f32 = 12.0;

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
    badge_position: BadgePosition,
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
            badge_position: config.badge_position,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarAction {
    None,
    Grid,
    Settings,
    Solo(usize),
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
            Some(since) if !self.paused && now.duration_since(since) >= PAUSE_AFTER_UNFOCUSED => {
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
    badge_position: BadgePosition,
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
            badge_position: config.badge_position,
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
            badge_position: self.badge_position,
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
        let badge_position = self.settings.badge_position;
        if let Err(err) = config::save(
            &config::config_path(),
            &Config {
                badge_position,
                cameras: desired.clone(),
            },
        ) {
            self.settings.error = Some(format!("Failed to save config: {err:#}"));
            return;
        }
        self.badge_position = badge_position;
        self.apply_cameras(desired);
        self.view = View::Grid;
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

        if matches!(self.view, View::Settings) && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_settings();
        }

        let sidebar = show_sidebar(ctx, &self.view, &self.cameras);
        match sidebar {
            SidebarAction::Grid => self.view = View::Grid,
            SidebarAction::Settings => self.open_settings(),
            SidebarAction::Solo(index) => self.view = View::Solo(index),
            SidebarAction::None => {}
        }

        match self.view {
            View::Grid => {
                if let Some(index) = show_grid(ctx, &mut self.cameras, paused, self.badge_position)
                {
                    self.view = View::Solo(index);
                }
            }
            View::Solo(index) => {
                if index >= self.cameras.len()
                    || show_solo(ctx, &mut self.cameras[index], paused, self.badge_position)
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

            let grid_active = !matches!(view, View::Settings);
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
    let painter = ui.painter_at(rect);

    if selected || response.hovered() {
        painter.rect_filled(rect, 0.0, theme::SOOT_2);
    }
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, height)),
            0.0,
            theme::PAPER,
        );
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

fn show_grid(
    ctx: &egui::Context,
    cameras: &mut [CameraView],
    paused: bool,
    badge_position: BadgePosition,
) -> Option<usize> {
    let mut opened = None;
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
                draw_grid_rows(ui, cameras, layout, paused, badge_position, &mut opened);
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        draw_grid_rows(ui, cameras, layout, paused, badge_position, &mut opened);
                    });
            }
        });
    opened
}

/// Columns chosen from width (~360px minimum tiles), capped so rows fit the height.
fn pick_columns(avail: egui::Vec2, count: usize) -> usize {
    let cols_by_width = (((avail.x + GRID_SPACING) / (GRID_MIN_TILE_WIDTH + GRID_SPACING))
        .floor()
        .max(1.0)) as usize;
    let cap = cols_by_width.min(count);
    let min_tile_height = GRID_MIN_TILE_WIDTH / (16.0 / 9.0);
    let rows_by_height = (((avail.y + GRID_SPACING) / (min_tile_height + GRID_SPACING))
        .floor()
        .max(1.0)) as usize;
    let mut cols = 1;
    while cols < cap && count.div_ceil(cols) > rows_by_height {
        cols += 1;
    }
    cols
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
    paused: bool,
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
                    paused,
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
    paused: bool,
    badge_position: BadgePosition,
) -> bool {
    let size = size.max(egui::vec2(80.0, 60.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::SOOT_2);

    if paused {
        center_message(&painter, rect, format!("{}: paused", cam.name));
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
                center_message(&painter, rect, msg);
            }
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

fn show_solo(
    ctx: &egui::Context,
    cam: &mut CameraView,
    paused: bool,
    badge_position: BadgePosition,
) -> bool {
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
            video_surface(ui, cam, ui.available_size(), false, paused, badge_position);
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

fn show_settings(ctx: &egui::Context, editor: &mut SettingsEditor) -> SettingsAction {
    let mut action = SettingsAction::None;
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::SOOT)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            paper_visuals(ui);

            ui.label(
                egui::RichText::new("SETTINGS")
                    .font(theme::display_font(30.0))
                    .color(theme::INK),
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
                ui.horizontal(|ui| {
                    let (block, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(block, 0.0, theme::STATUS_OFFLINE);
                    ui.label(
                        egui::RichText::new(err.to_string())
                            .font(theme::mono_font(10.0))
                            .color(theme::STATUS_OFFLINE),
                    );
                });
                ui.add_space(4.0);
            }

            badge_position_row(ui, editor);

            let mut delete = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
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
                        BtnVariant::Paper,
                    ) {
                        editor.add_row();
                    }
                });

            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::brutal_button(ui, "SAVE", BtnVariant::Ink) {
                    action = SettingsAction::Save;
                }
                if theme::brutal_button(ui, "CANCEL", BtnVariant::Paper) {
                    action = SettingsAction::Cancel;
                }
            });
        });
    action
}

/// Segmented two-option control for the grid-tile badge corner.
fn badge_position_row(ui: &mut egui::Ui, editor: &mut SettingsEditor) {
    ui.label(theme::micro_label("BADGE POSITION", theme::LABEL_ON_PAPER));
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for (option, label) in [
            (BadgePosition::TopRight, "TOP RIGHT"),
            (BadgePosition::BottomRight, "BOTTOM RIGHT"),
        ] {
            let selected = editor.badge_position == option;
            if theme::brutal_button(
                ui,
                label,
                if selected {
                    BtnVariant::Ink
                } else {
                    BtnVariant::Paper
                },
            ) {
                editor.badge_position = option;
            }
            ui.add_space(6.0);
        }
    });
}

fn settings_row(
    ui: &mut egui::Ui,
    index: usize,
    row: &mut SettingsRow,
    delete: &mut Option<usize>,
) {
    let invalid = row.url.trim().is_empty();
    let mut card = egui::Frame::new()
        .fill(theme::PAPER_2)
        .stroke(egui::Stroke::new(
            2.0_f32,
            if invalid {
                theme::STATUS_OFFLINE
            } else {
                theme::INK
            },
        ))
        .inner_margin(egui::Margin::same(12));
    if invalid {
        card = card.shadow(theme::hard_shadow(theme::STATUS_OFFLINE));
    }
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
                    .text_color(if invalid {
                        theme::STATUS_OFFLINE
                    } else {
                        theme::INK
                    });
                let width = (ui.available_width() - 92.0).max(160.0);
                if invalid {
                    ui.scope(|ui| {
                        let visuals = ui.visuals_mut();
                        visuals.widgets.inactive.bg_stroke =
                            egui::Stroke::new(2.0_f32, theme::STATUS_OFFLINE);
                        visuals.widgets.hovered.bg_stroke =
                            egui::Stroke::new(2.0_f32, theme::STATUS_OFFLINE);
                        visuals.widgets.active.bg_stroke =
                            egui::Stroke::new(2.0_f32, theme::STATUS_OFFLINE);
                        ui.add_sized([width, 28.0], url_edit);
                    });
                } else {
                    ui.add_sized([width, 28.0], url_edit);
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
    v.selection.stroke = egui::Stroke::new(1.0_f32, theme::PAPER);
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
    }
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
        assert_eq!(
            tracker.observe(true, t0 + Duration::from_secs(10)),
            FocusChange::None
        );
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
