// Prevents an extra console window on Windows in release (harmless on Linux).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! AudioPlumber — native egui/eframe entry point.
//!
//! The application is a pure-Rust egui GUI talking directly to the Tauri-free
//! [`audio_plumber::pipewire`] backend. The former Tauri webview front-end has
//! been removed; this is the single binary.

fn main() -> eframe::Result<()> {
    audio_plumber::app::run()
}
