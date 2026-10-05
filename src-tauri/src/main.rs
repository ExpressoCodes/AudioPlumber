// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri entry point.
//!
//! All PipeWire logic lives in the Tauri-free [`pipewire`] module; the
//! `#[tauri::command]` functions below are thin shims that forward to it. This
//! separation keeps the backend reusable verbatim by the forthcoming
//! egui/eframe frontend and keeps the eventual webview removal reversible.

use audio_plumber::pipewire::{self, Link, Port};
use std::collections::HashMap;

#[tauri::command]
async fn get_outputs() -> Vec<Port> {
    pipewire::get_outputs()
}

#[tauri::command]
async fn get_inputs() -> Vec<Port> {
    pipewire::get_inputs()
}

#[tauri::command]
async fn get_links() -> Vec<Link> {
    pipewire::get_links()
}

#[tauri::command]
async fn connect(from: String, to: String) -> Result<(), String> {
    pipewire::connect(&from, &to)
}

#[tauri::command]
async fn disconnect(from: String, to: String) -> Result<(), String> {
    pipewire::disconnect(&from, &to)
}

#[tauri::command]
async fn create_virtual_sink(name: String) -> Result<u32, String> {
    pipewire::create_virtual_sink(&name)
}

#[tauri::command]
async fn delete_virtual_sink(module_id: u32) -> Result<(), String> {
    pipewire::delete_virtual_sink(module_id)
}

#[tauri::command]
async fn check_deps() -> Vec<String> {
    pipewire::check_deps()
}

#[tauri::command]
async fn get_node_names() -> HashMap<String, String> {
    pipewire::get_node_names()
}

#[tauri::command]
async fn save_connections(connections: Vec<Link>) -> Result<(), String> {
    pipewire::save_connections(connections)
}

#[tauri::command]
async fn load_saved_connections() -> Vec<Link> {
    pipewire::load_saved_connections()
}

#[tauri::command]
async fn restore_connections() -> Vec<String> {
    pipewire::restore_connections()
}

#[tauri::command]
async fn get_debug_info() -> String {
    pipewire::get_debug_info()
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_outputs,
            get_inputs,
            get_links,
            connect,
            disconnect,
            create_virtual_sink,
            delete_virtual_sink,
            check_deps,
            get_node_names,
            save_connections,
            load_saved_connections,
            restore_connections,
            get_debug_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
