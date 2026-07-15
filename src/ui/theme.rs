use eframe::egui::{self, Color32, CornerRadius, Stroke, Vec2};

/// OpenAI-reference tokens for the complete native surface.
pub struct OpenAiTheme;

impl OpenAiTheme {
    pub const CANVAS: [u8; 4] = [255, 255, 255, 255];
    pub const INK: [u8; 4] = [0, 0, 0, 255];
    pub const SECONDARY_INK: [u8; 4] = [102, 102, 102, 255];
    pub const DISABLED_INK: [u8; 4] = [145, 145, 145, 255];
    pub const BOUNDARY: [u8; 4] = [145, 145, 145, 255];
    pub const CORNER_RADIUS: f32 = 0.0;
    pub const FOCUS_STROKE_WIDTH: f32 = 2.0;
    pub const MIN_CONTROL_HEIGHT: f32 = 44.0;
    pub const USES_GRADIENTS: bool = false;
    pub const USES_SHADOWS: bool = false;

    pub fn apply(context: &egui::Context) {
        context.set_theme(egui::Theme::Light);
        let mut style = (*context.style_of(egui::Theme::Light)).clone();
        style.spacing.interact_size = Vec2::new(
            style.spacing.interact_size.x.max(44.0),
            Self::MIN_CONTROL_HEIGHT,
        );
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(16.0, 10.0);
        style.visuals = egui::Visuals::light();
        style.visuals.override_text_color = Some(Self::color(Self::INK));
        style.visuals.weak_text_color = Some(Self::color(Self::SECONDARY_INK));
        style.visuals.panel_fill = Self::color(Self::CANVAS);
        style.visuals.window_fill = Self::color(Self::CANVAS);
        style.visuals.extreme_bg_color = Self::color(Self::CANVAS);
        style.visuals.faint_bg_color = Color32::from_gray(245);
        style.visuals.selection.bg_fill = Self::color(Self::INK);
        style.visuals.selection.stroke = Stroke::new(1.0, Self::color(Self::CANVAS));
        style.visuals.window_corner_radius = CornerRadius::ZERO;
        style.visuals.menu_corner_radius = CornerRadius::ZERO;
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
        style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
        style.visuals.window_stroke = Stroke::new(1.0, Self::color(Self::BOUNDARY));
        style.visuals.widgets.noninteractive.bg_fill = Self::color(Self::CANVAS);
        style.visuals.widgets.noninteractive.bg_stroke =
            Stroke::new(1.0, Self::color(Self::BOUNDARY));
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Self::color(Self::INK));
        style.visuals.widgets.inactive.bg_fill = Self::color(Self::CANVAS);
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Self::color(Self::INK));
        style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Self::color(Self::INK));
        style.visuals.widgets.hovered.bg_fill = Color32::from_gray(245);
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Self::color(Self::INK));
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Self::color(Self::INK));
        style.visuals.widgets.active.bg_fill = Self::color(Self::INK);
        style.visuals.widgets.active.bg_stroke =
            Stroke::new(Self::FOCUS_STROKE_WIDTH, Self::color(Self::INK));
        style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, Self::color(Self::CANVAS));
        style.visuals.widgets.open = style.visuals.widgets.active;
        for widget in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = CornerRadius::ZERO;
        }
        context.set_style_of(egui::Theme::Light, style);
    }

    pub fn contrast(foreground: [u8; 4], background: [u8; 4]) -> f64 {
        let foreground = relative_luminance(foreground);
        let background = relative_luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    pub fn color(value: [u8; 4]) -> Color32 {
        Color32::from_rgba_unmultiplied(value[0], value[1], value[2], value[3])
    }
}

fn relative_luminance(color: [u8; 4]) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color[0]) + 0.7152 * channel(color[1]) + 0.0722 * channel(color[2])
}
