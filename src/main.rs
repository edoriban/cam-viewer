use cam_viewer::{app::CamViewerApp, config};
use eframe::egui;

fn main() -> eframe::Result<()> {
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
