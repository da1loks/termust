#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod connection;
mod credentials;
mod sftp;
mod terminal;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    if let Some(status) = credentials::answer_ssh_prompt() {
        std::process::exit(status);
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/termust-icon.png"))
        .expect("the embedded Termust icon is a valid PNG");
    eframe::run_native(
        "termust",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_icon(icon)
                .with_inner_size([1_060.0, 660.0])
                .with_min_inner_size([670.0, 420.0]),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        },
        Box::new(|context| Ok(Box::new(app::App::new(&context.egui_ctx)))),
    )
}
