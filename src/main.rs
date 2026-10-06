//! Valoingest tray service.
//!
//! Lives in the Windows notification area, watches
//! `%LOCALAPPDATA%\VALORANT\Saved\Demos` and uploads each replay the game
//! downloads, with the privacy and publication choices set in the tray menu.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod api;
mod engine;
mod login;
mod platform;
mod store;
mod tray;
mod updater;

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    sync::{Arc, Mutex, mpsc},
};

const LOG_LIMIT_BYTES: u64 = 2 * 1024 * 1024;

struct FileLogger(Mutex<File>);

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata())
            && let Ok(mut file) = self.0.lock()
        {
            let _ = writeln!(
                file,
                "{} {:<5} {}",
                store::now_secs(),
                record.level(),
                record.args()
            );
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.0.lock() {
            let _ = file.flush();
        }
    }
}

fn init_logging() {
    let dir = store::app_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("valoingest.log");
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LOG_LIMIT_BYTES) {
        let _ = fs::rename(&path, dir.join("valoingest.old.log"));
    }
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
        let logger = Box::leak(Box::new(FileLogger(Mutex::new(file))));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("valoingest {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    init_logging();
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--apply-update")
    {
        if let Err(error) = updater::apply_update() {
            log::error!("update installation failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    if !platform::acquire_single_instance() {
        log::info!("another instance is already running");
        return;
    }
    log::info!("valoingest {} starting", env!("CARGO_PKG_VERSION"));
    updater::cleanup_completed();

    let (sender, receiver) = mpsc::channel();
    let shared = Arc::new(engine::Shared::new(tray::wake));
    let engine = engine::Engine::new(shared.clone(), sender.clone());
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || engine.run(receiver))
        .expect("could not start the upload engine");

    if !std::env::args_os().any(|arg| arg == "--skip-update") {
        updater::start(sender.clone());
    }

    tray::run(shared, sender);
    log::info!("valoingest exiting");
}
