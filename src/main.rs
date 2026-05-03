#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod auth;
mod config;
mod events;
mod launcher;
mod minecraft;

use app::GlideLauncherApp;
use eframe::egui;
use std::sync::mpsc;

const ICON_BYTES: &[u8] = include_bytes!("../glideicon.png");

fn main() -> eframe::Result<()> {
    let launch_test = std::env::args().any(|arg| arg == "--launch-test");
    if launch_test || std::env::args().any(|arg| arg == "--prepare-only") {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = if launch_test {
                launcher::prepare_and_launch(config::LauncherConfig::load(), tx.clone()).map(|_| ())
            } else {
                launcher::prepare_only(config::LauncherConfig::load(), tx.clone())
            };
            if let Err(error) = result {
                let _ = tx.send(events::WorkerEvent::Failed(format!("{error:#}")));
            }
        });

        let mut pid = None;
        while let Ok(event) = rx.recv() {
            if let events::WorkerEvent::LaunchStarted(started_pid) = &event {
                pid = Some(*started_pid);
            }
            println!("{event:?}");
            if matches!(
                event,
                events::WorkerEvent::Finished(_) | events::WorkerEvent::Failed(_)
            ) {
                break;
            }
        }
        if launch_test {
            if let Some(pid) = pid {
                std::thread::sleep(std::time::Duration::from_secs(5));
                #[cfg(windows)]
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F", "/T"])
                    .status();
            }
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 640.0])
            .with_min_inner_size([420.0, 640.0])
            .with_max_inner_size([420.0, 640.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_maximize_button(false)
            .with_transparent(true)
            .with_icon(load_window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "GlideClient Launcher",
        options,
        Box::new(|cc| Ok(Box::new(GlideLauncherApp::new(cc, ICON_BYTES)))),
    )
}

fn load_window_icon() -> egui::IconData {
    let image = image::load_from_memory(ICON_BYTES)
        .expect("glideicon.png must be a valid image")
        .into_rgba8();
    let (width, height) = image.dimensions();

    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}
