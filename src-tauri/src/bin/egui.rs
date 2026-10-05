// Prevents an extra console window on Windows in release (harmless on Linux).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Native egui/eframe entry point for AudioPlumber.
//!
//! This binary renders the patchbay with a pure-Rust GUI and talks to the
//! shared Tauri-free [`audio_plumber::pipewire`] backend directly — no webview,
//! no IPC. It is the target of the Tauri -> egui pivot.

fn main() -> eframe::Result<()> {
    audio_plumber::app::run()
}
