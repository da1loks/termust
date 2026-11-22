use eframe::egui::{self, Color32, FontId, Margin, Stroke, TextStyle};

pub const CANVAS: Color32 = Color32::from_gray(6);
pub const SIDEBAR: Color32 = Color32::from_gray(15);
pub const TOOLBAR: Color32 = Color32::from_gray(11);
pub const SURFACE: Color32 = Color32::from_gray(22);
pub const BORDER: Color32 = Color32::from_gray(39);
pub const SELECTED: Color32 = Color32::from_gray(32);
pub const TEXT: Color32 = Color32::from_gray(232);
pub const MUTED: Color32 = Color32::from_gray(158);
pub const ACCENT: Color32 = Color32::from_rgb(145, 204, 179);
pub const ERROR: Color32 = Color32::from_rgb(247, 149, 151);

pub fn apply(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);
    context.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = CANVAS;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = 10.into();
    visuals.menu_corner_radius = 8.into();
    visuals.extreme_bg_color = TOOLBAR;
    visuals.text_edit_bg_color = Some(TOOLBAR);
    visuals.faint_bg_color = SIDEBAR;
    visuals.weak_text_color = Some(MUTED);
    visuals.selection.bg_fill = SELECTED;
    visuals.selection.stroke = Stroke::new(1.0, TEXT);
    visuals.error_fg_color = ERROR;

    let widgets = &mut visuals.widgets;
    for (widget, fill, foreground, border) in [
        (&mut widgets.noninteractive, SIDEBAR, TEXT, BORDER),
        (&mut widgets.inactive, SURFACE, TEXT, BORDER),
        (&mut widgets.hovered, SURFACE, Color32::WHITE, BORDER),
        (&mut widgets.active, SURFACE, Color32::WHITE, MUTED),
        (&mut widgets.open, SURFACE, TEXT, BORDER),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, border);
        widget.fg_stroke = Stroke::new(1.0, foreground);
        widget.corner_radius = 6.into();
        widget.expansion = 0.0;
    }
    context.set_visuals(visuals);
    context.style_mut_of(egui::Theme::Dark, |style| {
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(18.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(13.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(13.0));
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.window_margin = Margin::same(16);
    });
}
