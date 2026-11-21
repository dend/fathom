// Keeps a console window from opening alongside the app on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> eframe::Result {
    let icon =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/128x128@2x.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Fathom")
        .with_app_id("dev.den.fathom")
        .with_inner_size([1320.0, 840.0])
        .with_min_inner_size([900.0, 560.0])
        .with_drag_and_drop(true);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let config_dir = fathom::system::config_dir();
    if let Some(old) = fathom::system::old_config_dir() {
        fathom::system::move_old_settings(&old, &config_dir);
    }
    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        persist_window: true,
        // Despite its docs, eframe takes this as the path of its state file.
        persistence_path: Some(fathom::system::state_file(&config_dir)),
        ..Default::default()
    };
    eframe::run_native(
        "Fathom",
        options,
        Box::new(|cc| Ok(Box::new(fathom::app::FathomApp::new(cc)))),
    )
}
