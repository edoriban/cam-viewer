use crate::config::BadgePosition;
use crate::stream::Status;
use eframe::egui;
use std::sync::Arc;

// Surfaces
pub const SOOT: egui::Color32 = egui::Color32::from_rgb(0x0B, 0x0B, 0x0D);
pub const SOOT_2: egui::Color32 = egui::Color32::from_rgb(0x17, 0x16, 0x1C);
pub const INK: egui::Color32 = egui::Color32::from_rgb(0x14, 0x13, 0x0F);
pub const ASH: egui::Color32 = egui::Color32::from_rgb(0x6B, 0x6A, 0x62);
pub const CONCRETE: egui::Color32 = egui::Color32::from_rgb(0xB9, 0xB5, 0xAA);
pub const PAPER: egui::Color32 = egui::Color32::from_rgb(0xEC, 0xE9, 0xE0);
pub const PAPER_2: egui::Color32 = egui::Color32::from_rgb(0xE2, 0xDE, 0xD2);

// Status mapping
pub const STATUS_ONLINE: egui::Color32 = egui::Color32::from_rgb(0x00, 0xB0, 0x50);
pub const STATUS_CONNECTING: egui::Color32 = egui::Color32::from_rgb(0xFF, 0xD2, 0x00);
pub const STATUS_OFFLINE: egui::Color32 = egui::Color32::from_rgb(0xE3, 0x06, 0x13);

// Derived neutrals
pub const BORDER_DIM: egui::Color32 = egui::Color32::from_rgb(0x48, 0x47, 0x44);
pub const LABEL_ON_PAPER: egui::Color32 = egui::Color32::from_rgb(0x4A, 0x49, 0x41);

/// Ghost text over dark surfaces (paper white at low alpha).
pub fn ghost_text() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(236, 233, 224, 90)
}

const DISPLAY_FAMILY: &str = "space-grotesk-bold";
const FONT_SPACE_GROTESK_MEDIUM: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-Medium.ttf");
const FONT_SPACE_GROTESK_BOLD: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-Bold.ttf");
const FONT_JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");

pub fn status_color(status: Status) -> egui::Color32 {
    match status {
        Status::Online => STATUS_ONLINE,
        Status::Connecting => STATUS_CONNECTING,
        Status::Offline => STATUS_OFFLINE,
        Status::Paused => ASH,
    }
}

pub const fn hard_shadow(color: egui::Color32) -> egui::Shadow {
    egui::Shadow {
        offset: [4, 4],
        blur: 0,
        spread: 0,
        color,
    }
}

pub fn display_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(Arc::from(DISPLAY_FAMILY)))
}

pub fn mono_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Monospace)
}

/// Micro-label convention: small monospace, uppercase.
pub fn micro_label(text: impl Into<String>, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(text.into())
        .font(mono_font(9.5))
        .color(color)
}

/// Install fonts and global styling once at startup.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(apply_style);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "cam-space-grotesk-medium".into(),
        embedded_font(FONT_SPACE_GROTESK_MEDIUM),
    );
    fonts.font_data.insert(
        "cam-space-grotesk-bold".into(),
        embedded_font(FONT_SPACE_GROTESK_BOLD),
    );
    fonts.font_data.insert(
        "cam-jetbrains-mono".into(),
        embedded_font(FONT_JETBRAINS_MONO),
    );

    // Custom display family (bold Space Grotesk), with medium as fallback glyph source.
    fonts.families.insert(
        egui::FontFamily::Name(Arc::from(DISPLAY_FAMILY)),
        vec![
            "cam-space-grotesk-bold".into(),
            "cam-space-grotesk-medium".into(),
        ],
    );
    // Proportional: Space Grotesk Medium primary, egui defaults kept as glyph fallbacks.
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cam-space-grotesk-medium".into());
    // Monospace: JetBrains Mono primary, defaults kept as glyph fallbacks.
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "cam-jetbrains-mono".into());

    ctx.set_fonts(fonts);
}

fn embedded_font(data: &'static [u8]) -> std::sync::Arc<egui::FontData> {
    std::sync::Arc::new(egui::FontData::from_static(data))
}

fn apply_style(style: &mut egui::Style) {
    let v = &mut style.visuals;
    v.dark_mode = true;
    v.override_text_color = Some(PAPER);
    v.panel_fill = SOOT;
    v.window_fill = SOOT;
    v.window_stroke = egui::Stroke::new(2.0_f32, CONCRETE);
    v.window_shadow = hard_shadow(INK);
    v.extreme_bg_color = SOOT_2;
    v.faint_bg_color = SOOT_2;
    v.selection.bg_fill = STATUS_OFFLINE;
    v.selection.stroke = egui::Stroke::new(1.0_f32, PAPER);
    v.error_fg_color = STATUS_OFFLINE;
    v.warn_fg_color = STATUS_CONNECTING;
    v.hyperlink_color = CONCRETE;

    for widget in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::ZERO;
    }
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, SOOT_2);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, PAPER);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.5_f32, PAPER);
    v.widgets.active.fg_stroke = egui::Stroke::new(2.0_f32, PAPER);

    style.text_styles = [
        (egui::TextStyle::Small, mono_font(11.0)),
        (
            egui::TextStyle::Body,
            egui::FontId::new(13.5, egui::FontFamily::Proportional),
        ),
        (
            egui::TextStyle::Button,
            egui::FontId::new(11.5, egui::FontFamily::Monospace),
        ),
        (
            egui::TextStyle::Monospace,
            egui::FontId::new(12.0, egui::FontFamily::Monospace),
        ),
        (egui::TextStyle::Heading, display_font(20.0)),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BtnVariant {
    /// Paper background, ink text; hover inverts.
    Paper,
    /// Ink background, paper text; hover inverts.
    Ink,
    /// Ghost danger: paper background, signal-red text; hover fills red.
    Danger,
    /// Confirm action (save/add): paper background, ink text; hover fills
    /// signal-green with paper text.
    Confirm,
}

fn btn_palette(
    variant: BtnVariant,
    hovered: bool,
) -> (egui::Color32, egui::Color32, egui::Color32) {
    match variant {
        BtnVariant::Paper => {
            if hovered {
                (INK, PAPER, PAPER)
            } else {
                (PAPER, INK, INK)
            }
        }
        BtnVariant::Ink => {
            if hovered {
                (PAPER, INK, INK)
            } else {
                (INK, PAPER, INK)
            }
        }
        BtnVariant::Danger => {
            if hovered {
                (STATUS_OFFLINE, PAPER, STATUS_OFFLINE)
            } else {
                (PAPER, STATUS_OFFLINE, STATUS_OFFLINE)
            }
        }
        BtnVariant::Confirm => {
            if hovered {
                (STATUS_ONLINE, PAPER, STATUS_ONLINE)
            } else {
                (PAPER, INK, INK)
            }
        }
    }
}

fn draw_brutal_button(
    ui: &mut egui::Ui,
    size: Option<egui::Vec2>,
    text: &str,
    variant: BtnVariant,
) -> bool {
    let font = mono_font(11.0);
    let padding = ui.spacing().button_padding;
    let desired = match size {
        Some(s) => s,
        None => {
            let measured =
                ui.painter()
                    .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE);
            measured.size() + padding * 2.0
        }
    };
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());
    let hovered = response.hovered() || response.is_pointer_button_down_on();
    let (bg, fg, border) = btn_palette(variant, hovered);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    if variant != BtnVariant::Danger {
        // Hard offset shadow, no blur. Drawn unclipped: painter_at(rect)
        // below would cut off anything outside `rect`, including this offset.
        ui.painter()
            .rect_filled(rect.translate(egui::vec2(4.0, 4.0)), 0.0, CONCRETE);
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0_f32, border),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, fg);
    response.clicked()
}

pub fn brutal_button(ui: &mut egui::Ui, text: &str, variant: BtnVariant) -> bool {
    draw_brutal_button(ui, None, text, variant)
}

pub fn brutal_button_sized(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    text: &str,
    variant: BtnVariant,
) -> bool {
    draw_brutal_button(ui, Some(size), text, variant)
}

/// Bordered sidebar navigation entry; active inverts to paper with a red offset shadow.
pub fn nav_item(ui: &mut egui::Ui, label: &str, count: Option<String>, active: bool) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::click());
    let hovered = response.hovered() || response.is_pointer_button_down_on();
    let (bg, fg, border) = if active {
        (PAPER, INK, PAPER)
    } else if hovered {
        (SOOT_2, PAPER, CONCRETE)
    } else {
        (SOOT, CONCRETE, CONCRETE)
    };

    let font = mono_font(10.5);
    if active {
        // Drawn unclipped: painter_at(rect) below would cut off this offset.
        ui.painter()
            .rect_filled(rect.translate(egui::vec2(3.0, 3.0)), 0.0, STATUS_OFFLINE);
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, bg);
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0_f32, border),
        egui::StrokeKind::Inside,
    );

    let label_galley = painter.layout_no_wrap(label.to_owned(), font.clone(), fg);
    painter.galley(
        egui::pos2(
            rect.left() + 12.0,
            rect.center().y - label_galley.size().y / 2.0,
        ),
        label_galley,
        fg,
    );
    if let Some(count) = count {
        let count_color = fg.gamma_multiply(0.7);
        let count_galley = painter.layout_no_wrap(count, font, count_color);
        painter.galley(
            egui::pos2(
                rect.right() - 12.0 - count_galley.size().x,
                rect.center().y - count_galley.size().y / 2.0,
            ),
            count_galley,
            count_color,
        );
    }
    response.clicked()
}

/// Status chip mounted on a video tile's configured corner.
pub fn status_badge(
    painter: &egui::Painter,
    tile: egui::Rect,
    status: Status,
    position: BadgePosition,
) {
    let text = status.label().to_uppercase();
    let text_color = match status {
        Status::Online | Status::Connecting => SOOT,
        Status::Offline | Status::Paused => PAPER,
    };
    let galley = painter.layout_no_wrap(text, mono_font(9.0), text_color);
    let size = galley.size() + egui::vec2(14.0, 8.0);
    let inset = 10.0;
    let min = match position {
        BadgePosition::TopRight => egui::pos2(tile.right() - inset - size.x, tile.top() + inset),
        BadgePosition::BottomRight => egui::pos2(
            tile.right() - inset - size.x,
            tile.bottom() - inset - size.y,
        ),
    };
    let rect = egui::Rect::from_min_size(min, size);
    painter.rect_filled(rect, 0.0, status_color(status));
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(2.0_f32, INK),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, text_color);
}

/// Truncate a string with an ellipsis so it fits `max_w` points.
pub fn elide_to_width(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    max_w: f32,
    color: egui::Color32,
) -> String {
    if painter
        .layout_no_wrap(text.to_owned(), font.clone(), color)
        .size()
        .x
        <= max_w
    {
        return text.to_owned();
    }
    let mut candidate = text.to_owned();
    while candidate.chars().count() > 1 {
        candidate.pop();
        let truncated = format!("{candidate}\u{2026}");
        if painter
            .layout_no_wrap(truncated.clone(), font.clone(), color)
            .size()
            .x
            <= max_w
        {
            return truncated;
        }
    }
    candidate
}
