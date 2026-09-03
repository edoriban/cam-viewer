// Windows assigns a console to console-subsystem binaries, so launching the app
// from the shell or a shortcut pops a cmd window next to the GUI.
#![windows_subsystem = "windows"]

use cam_viewer::{app::CamViewerApp, config};
use eframe::egui;

fn main() -> eframe::Result<()> {
    // Answered before any window opens. Note that the Windows GUI subsystem
    // gives the process no console, so this print is only visible where one is
    // already attached; attaching one needs FFI, which this crate forbids.
    if std::env::args().skip(1).any(|arg| arg == "--version" || arg == "-V") {
        println!("cam-viewer {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let path = config::config_path();
    let cfg = match config::load(&path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("failed to load cameras.toml: {err:#}");
            config::Config::default()
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("cam-viewer"),
        ..Default::default()
    };

    eframe::run_native(
        "cam-viewer",
        options,
        Box::new(move |cc| {
            cam_viewer::theme::install(&cc.egui_ctx);
            Ok(Box::new(CamViewerApp::new(&cfg)))
        }),
    )
}
