//! AudioPlumber library crate.
//!
//! The Tauri-free [`pipewire`] backend lives here so that any front-end — the
//! legacy Tauri webview binary or the native egui binary — can depend on it as
//! a plain library with no GUI-toolkit coupling. Keeping the backend behind a
//! library boundary is what makes the Tauri -> egui pivot reversible: the two
//! binaries share one audited backend verbatim.

pub mod pipewire;
