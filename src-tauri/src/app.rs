//! Native egui/eframe front-end for AudioPlumber.
//!
//! This module reproduces the behaviour of the legacy Tauri webview UI
//! (`ui/index.html` + `ui/main.js` + `ui/style.css`) in pure Rust against the
//! shared [`crate::pipewire`] backend. The two-column click-to-connect patchbay,
//! the hand-drawn glowing bezier cables, the cursor-tracking rubber-band, the
//! simple/advanced modes, virtual-sink create/delete/persist/recreate, the 3s
//! auto-refresh + auto-reconnect, the dep-check / error banners, the virtual-sink
//! modal and the debug panel (D key) are all ported here.
//!
//! Error-visibility parity is preserved exactly: the old `setStatus` was a no-op
//! and certain failures (sink-delete failure, periodic-refresh errors) were
//! invisible to the user — those remain invisible here. Only the banners that
//! existed before (missing deps, connect/disconnect/create failures, surfaced
//! startup restore errors) are reproduced.

use eframe::egui;
use egui::accesskit::{Live, Role};
use egui::{
    Align, Align2, Color32, Id, Key, Layout, Margin, Order, Pos2, Rect, Rounding, Sense, Shape,
    Stroke, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::pipewire::{self, Link, Port, VirtualSink};

// ─── Theme tokens (mirror ui/style.css :root) ──────────────────────────────
const BG: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x2e);
const SURFACE: Color32 = Color32::from_rgb(0x16, 0x21, 0x3e);
const SURFACE2: Color32 = Color32::from_rgb(0x0f, 0x34, 0x60);
const ACCENT: Color32 = Color32::from_rgb(0xe9, 0x45, 0x60);
const TEXT: Color32 = Color32::from_rgb(0xe0, 0xe0, 0xe0);
const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x8a, 0xaa);
const PORT_OUT: Color32 = Color32::from_rgb(0x53, 0xd8, 0xfb);
const PORT_IN: Color32 = Color32::from_rgb(0xff, 0x6b, 0x9d);
const TEAL: Color32 = Color32::from_rgb(0x4e, 0xcd, 0xc4);
const TEAL_BORDER: Color32 = Color32::from_rgb(0x1a, 0x4a, 0x4a);
const CARD_BORDER: Color32 = Color32::from_rgb(0x1e, 0x2a, 0x4a);
const BANNER_BG: Color32 = Color32::from_rgb(0x8b, 0x1a, 0x1a);
const BANNER_BORDER: Color32 = Color32::from_rgb(0xe0, 0x55, 0x55);
const BANNER_TEXT: Color32 = Color32::from_rgb(0xff, 0xd5, 0xd5);
const DEBUG_BG: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x1a);
const DEBUG_TEXT: Color32 = Color32::from_rgb(0xa0, 0xe8, 0xc8);

/// Cable colour cycling palette (mirrors CABLE_COLORS in main.js).
const CABLE_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x53, 0xd8, 0xfb),
    Color32::from_rgb(0xff, 0x6b, 0x9d),
    Color32::from_rgb(0xa8, 0xff, 0x78),
    Color32::from_rgb(0xff, 0xde, 0x59),
    Color32::from_rgb(0xc7, 0x7d, 0xff),
    Color32::from_rgb(0xff, 0x9f, 0x43),
    Color32::from_rgb(0x48, 0xdb, 0xfb),
    Color32::from_rgb(0xff, 0xea, 0xa7),
];
fn cable_color(index: usize) -> Color32 {
    CABLE_COLORS[index % CABLE_COLORS.len()]
}

const STEREO_PORT_NAMES: [&str; 4] = ["playback_FL", "playback_FR", "capture_FL", "capture_FR"];
const DOT_SIZE: f32 = 12.0;
const COL_WIDTH: f32 = 320.0;
const REFRESH_SECS: u64 = 3;

// ─── Backend worker protocol ───────────────────────────────────────────────

#[derive(Default, Clone)]
struct Snapshot {
    outputs: Vec<Port>,
    inputs: Vec<Port>,
    links: Vec<Link>,
    node_names: HashMap<String, String>,
}

/// Commands sent from the UI thread to the backend worker thread. All blocking
/// PipeWire/`pactl` work happens on the worker so the UI thread never stalls.
enum Cmd {
    /// Periodic/explicit refresh: poll a snapshot, then fire auto-reconnect whose
    /// errors are discarded (parity: periodic restore errors are invisible).
    Refresh,
    /// Startup restore: errors ARE surfaced to the banner.
    InitRestore,
    CheckDeps,
    /// Recreate persisted sinks whose ports are absent (startup J6 flow).
    EnsureSinks(Vec<VirtualSink>),
    Connect(Vec<(String, String)>),
    Disconnect(Vec<(String, String)>),
    CreateSink {
        name: String,
        sinks: Vec<VirtualSink>,
    },
    DeleteSink {
        module_id: u32,
        sinks: Vec<VirtualSink>,
    },
    FetchDebug,
}

/// Events sent from the worker back to the UI thread.
enum Evt {
    Snapshot(Snapshot),
    SinksChanged(Vec<VirtualSink>),
    DepsMissing(Vec<String>),
    RestoreErrors(Vec<String>),
    ConnectError(String),
    DisconnectError(String),
    CreateSinkError(String),
    DebugInfo(String),
}

fn spawn_worker(ctx: egui::Context) -> (Sender<Cmd>, Receiver<Evt>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Evt>();

    std::thread::spawn(move || {
        while let Ok(cmd) = cmd_rx.recv() {
            handle_cmd(cmd, &evt_tx);
            ctx.request_repaint();
        }
    });

    (cmd_tx, evt_rx)
}

fn poll_snapshot() -> Snapshot {
    Snapshot {
        outputs: pipewire::get_outputs(),
        inputs: pipewire::get_inputs(),
        links: pipewire::get_links(),
        node_names: pipewire::get_node_names(),
    }
}

fn handle_cmd(cmd: Cmd, evt: &Sender<Evt>) {
    match cmd {
        Cmd::Refresh => {
            let snap = poll_snapshot();
            let _ = evt.send(Evt::Snapshot(snap));
            // Fire-and-forget auto-reconnect; errors are intentionally discarded
            // (parity with the periodic reconnectSavedConnections()).
            let _ = pipewire::restore_connections();
        }
        Cmd::InitRestore => {
            let errors = pipewire::restore_connections();
            let _ = evt.send(Evt::RestoreErrors(errors));
        }
        Cmd::CheckDeps => {
            let _ = evt.send(Evt::DepsMissing(pipewire::check_deps()));
        }
        Cmd::EnsureSinks(mut sinks) => {
            let outputs = pipewire::get_outputs();
            let mut changed = false;
            for vs in &mut sinks {
                if virtual_sink_ports_present(&vs.name, &outputs) {
                    continue;
                }
                match pipewire::create_virtual_sink(&vs.name) {
                    Ok(id) => {
                        vs.module_id = id;
                        changed = true;
                    }
                    Err(e) => {
                        eprintln!("Failed to recreate virtual sink \"{}\": {}", vs.name, e);
                    }
                }
            }
            if changed {
                let _ = pipewire::save_virtual_sinks(&sinks);
                std::thread::sleep(Duration::from_millis(500));
            }
            let _ = evt.send(Evt::SinksChanged(sinks));
            let _ = evt.send(Evt::Snapshot(poll_snapshot()));
        }
        Cmd::Connect(pairs) => {
            let mut err: Option<String> = None;
            for (from, to) in &pairs {
                if let Err(e) = pipewire::connect(from, to) {
                    err = Some(e);
                    break;
                }
            }
            let snap = poll_snapshot();
            let _ = pipewire::save_connections(snap.links.clone());
            let _ = evt.send(Evt::Snapshot(snap));
            if let Some(e) = err {
                let _ = evt.send(Evt::ConnectError(e));
            }
        }
        Cmd::Disconnect(pairs) => {
            let mut errors = Vec::new();
            for (from, to) in &pairs {
                if let Err(e) = pipewire::disconnect(from, to) {
                    errors.push(e);
                }
            }
            let snap = poll_snapshot();
            let _ = pipewire::save_connections(snap.links.clone());
            let _ = evt.send(Evt::Snapshot(snap));
            if !errors.is_empty() {
                let _ = evt.send(Evt::DisconnectError(errors.join("; ")));
            }
        }
        Cmd::CreateSink { name, mut sinks } => match pipewire::create_virtual_sink(&name) {
            Ok(module_id) => {
                sinks.push(VirtualSink { name, module_id });
                let _ = pipewire::save_virtual_sinks(&sinks);
                std::thread::sleep(Duration::from_millis(500));
                let _ = evt.send(Evt::SinksChanged(sinks));
                let _ = evt.send(Evt::Snapshot(poll_snapshot()));
            }
            Err(e) => {
                let _ = evt.send(Evt::CreateSinkError(format!(
                    "Failed to create virtual sink \"{}\": {}",
                    name, e
                )));
            }
        },
        Cmd::DeleteSink { module_id, mut sinks } => {
            match pipewire::delete_virtual_sink(module_id) {
                Ok(()) => {
                    sinks.retain(|v| v.module_id != module_id);
                    let _ = pipewire::save_virtual_sinks(&sinks);
                    let snap = poll_snapshot();
                    let _ = pipewire::save_connections(snap.links.clone());
                    let _ = evt.send(Evt::SinksChanged(sinks));
                    let _ = evt.send(Evt::Snapshot(snap));
                }
                // Parity: delete failure is invisible (old code only setStatus()'d).
                Err(_) => {}
            }
        }
        Cmd::FetchDebug => {
            let _ = evt.send(Evt::DebugInfo(pipewire::get_debug_info()));
        }
    }
}

/// Fuzzy check: does a virtual sink's null-sink currently expose ports? (PipeWire
/// may append a `.2` / `_1` suffix to the requested sink_name.)
fn virtual_sink_ports_present(name: &str, outputs: &[Port]) -> bool {
    let node = format!("AudioPlumber_{}", name);
    outputs.iter().any(|p| {
        p.node == node
            || p.node.starts_with(&format!("{}.", node))
            || p.node.starts_with(&format!("{}_", node))
    })
}

fn node_name_from_port_id(port_id: &str) -> String {
    match port_id.find(':') {
        Some(i) => port_id[..i].to_string(),
        None => port_id.to_string(),
    }
}

// ─── Selection / dot identity ──────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Output,
    Input,
}

/// Identity of a clickable port dot, used both as the anchor-map key and the
/// pending-selection identity (mirrors the DOM data-attributes).
#[derive(Clone, PartialEq, Eq, Hash)]
enum DotKey {
    OutNode(String), // simple-mode left dot, by actual PipeWire node name
    InNode(String),  // simple-mode right dot
    OutPort(String), // advanced-mode left dot, by port id
    InPort(String),  // advanced-mode right dot
}

#[derive(Clone)]
struct Endpoint {
    side: Side,
    key: DotKey,
    ports: Vec<String>,
}

// ─── Application state ─────────────────────────────────────────────────────

pub struct AudioPlumberApp {
    cmd_tx: Sender<Cmd>,
    evt_rx: Receiver<Evt>,

    // Backend data
    outputs: Vec<Port>,
    inputs: Vec<Port>,
    links: Vec<Link>,
    node_names: HashMap<String, String>,
    virtual_sinks: Vec<VirtualSink>,

    // UI state
    simple_mode: bool,
    pending: Option<Endpoint>,
    error_banner: Option<String>,
    show_vsink_modal: bool,
    vsink_input: String,
    vsink_focus_pending: bool,
    show_debug: bool,
    debug_text: String,

    // Per-frame hover memory for card-hover-reveal (1-frame lag, smooth).
    sink_card_rects: HashMap<String, Rect>,
    // Cable the cursor is hovering this frame (for click-to-disconnect).
    hovered_cable: Option<CableId>,
}

impl AudioPlumberApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);

        let (cmd_tx, evt_rx) = spawn_worker(cc.egui_ctx.clone());

        // Startup sequence mirrors ui/main.js init():
        //   load sinks → check_deps → ensure sinks → surfaced restore → refresh.
        let virtual_sinks = pipewire::load_virtual_sinks();
        let _ = cmd_tx.send(Cmd::CheckDeps);
        let _ = cmd_tx.send(Cmd::EnsureSinks(virtual_sinks.clone()));
        let _ = cmd_tx.send(Cmd::InitRestore);
        let _ = cmd_tx.send(Cmd::Refresh);

        // 3s auto-refresh timer thread.
        {
            let tx = cmd_tx.clone();
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(REFRESH_SECS));
                if tx.send(Cmd::Refresh).is_err() {
                    break;
                }
                ctx.request_repaint();
            });
        }

        Self {
            cmd_tx,
            evt_rx,
            outputs: Vec::new(),
            inputs: Vec::new(),
            links: Vec::new(),
            node_names: HashMap::new(),
            virtual_sinks,
            simple_mode: true,
            pending: None,
            error_banner: None,
            show_vsink_modal: false,
            vsink_input: String::new(),
            vsink_focus_pending: false,
            show_debug: false,
            debug_text: String::new(),
            sink_card_rects: HashMap::new(),
            hovered_cable: None,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(evt) = self.evt_rx.try_recv() {
            match evt {
                Evt::Snapshot(s) => {
                    self.outputs = s.outputs;
                    self.inputs = s.inputs;
                    self.links = s.links;
                    self.node_names = s.node_names;
                }
                Evt::SinksChanged(list) => self.virtual_sinks = list,
                Evt::DepsMissing(missing) => {
                    if !missing.is_empty() {
                        self.error_banner = Some(format!(
                            "Missing tools: {}. Make sure PipeWire and pipewire-pulse are installed.",
                            missing.join(", ")
                        ));
                    }
                }
                Evt::RestoreErrors(errors) => {
                    if !errors.is_empty() {
                        self.error_banner = Some(format!(
                            "Some saved connections failed to restore: {}",
                            errors.join("; ")
                        ));
                    }
                }
                Evt::ConnectError(e) => self.error_banner = Some(format!("Connect failed: {}", e)),
                Evt::DisconnectError(e) => {
                    self.error_banner = Some(format!("Disconnect failed: {}", e))
                }
                Evt::CreateSinkError(e) => self.error_banner = Some(e),
                Evt::DebugInfo(info) => self.debug_text = info,
            }
        }
    }

    fn clear_selection(&mut self) {
        self.pending = None;
    }

    fn refresh(&self) {
        let _ = self.cmd_tx.send(Cmd::Refresh);
    }

    /// Port-click state machine (mirrors onPortClick/selectEndpoint/completeConnection).
    /// Returns connect pairs when a connection is completed.
    fn on_port_click(&mut self, ep: Endpoint) -> Option<Vec<(String, String)>> {
        match &self.pending {
            None => {
                self.pending = Some(ep);
                None
            }
            Some(p) => {
                if p.key == ep.key {
                    // Clicking the selected endpoint again cancels.
                    self.pending = None;
                    None
                } else if p.side == ep.side {
                    // Same side moves the selection.
                    self.pending = Some(ep);
                    None
                } else {
                    // Complementary side completes the connection.
                    let (out, inp) = if ep.side == Side::Output {
                        (&ep, p)
                    } else {
                        (p, &ep)
                    };
                    let mut from = out.ports.clone();
                    let mut to = inp.ports.clone();
                    from.sort();
                    to.sort();
                    let pairs: Vec<(String, String)> = from
                        .into_iter()
                        .zip(to)
                        .filter(|(f, t)| !f.is_empty() && !t.is_empty())
                        .collect();
                    self.pending = None;
                    if pairs.is_empty() {
                        None
                    } else {
                        Some(pairs)
                    }
                }
            }
        }
    }

    fn connected_sets(&self) -> (HashSet<String>, HashSet<String>) {
        let mut outs = HashSet::new();
        let mut ins = HashSet::new();
        for l in &self.links {
            outs.insert(node_name_from_port_id(&l.from));
            ins.insert(node_name_from_port_id(&l.to));
        }
        (outs, ins)
    }
}

// ─── Accessibility ─────────────────────────────────────────────────────────
/// Augment a widget's AccessKit node (ARIA-equivalent): optional accessible
/// name, role and live-region politeness. No-op when AccessKit is disabled.
fn a11y(
    ctx: &egui::Context,
    id: Id,
    name: Option<&str>,
    role: Option<Role>,
    live: Option<Live>,
) {
    ctx.accesskit_node_builder(id, |b| {
        if let Some(n) = name {
            b.set_name(n.to_string());
        }
        if let Some(r) = role {
            b.set_role(r);
        }
        if let Some(l) = live {
            b.set_live(l);
        }
    });
}

// ─── Theme ─────────────────────────────────────────────────────────────────
fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = BG;
    visuals.override_text_color = Some(TEXT);
    visuals.window_stroke = Stroke::new(1.0_f32, SURFACE2);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    ctx.set_visuals(visuals);
}

// ─── Bezier helpers ────────────────────────────────────────────────────────
/// Sample the same cubic the webview drew (control points pulled horizontally by
/// half the x-distance), producing a poly-line for painting and hit-testing.
fn sample_cable(from: Pos2, to: Pos2) -> Vec<Pos2> {
    let dx = (to.x - from.x).abs() * 0.5;
    let p0 = from;
    let p1 = Pos2::new(from.x + dx, from.y);
    let p2 = Pos2::new(to.x - dx, to.y);
    let p3 = to;
    let n = 28;
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let a = mt * mt * mt;
            let b = 3.0 * mt * mt * t;
            let c = 3.0 * mt * t * t;
            let d = t * t * t;
            Pos2::new(
                a * p0.x + b * p1.x + c * p2.x + d * p3.x,
                a * p0.y + b * p1.y + c * p2.y + d * p3.y,
            )
        })
        .collect()
}

fn dist_to_polyline(pts: &[Pos2], p: Pos2) -> f32 {
    let mut best = f32::INFINITY;
    for seg in pts.windows(2) {
        best = best.min(dist_point_segment(p, seg[0], seg[1]));
    }
    best
}

fn dist_point_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.x * ab.x + ab.y * ab.y;
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = (((p - a).x * ab.x + (p - a).y * ab.y) / len2).clamp(0.0, 1.0);
    let proj = Pos2::new(a.x + ab.x * t, a.y + ab.y * t);
    (p - proj).length()
}

/// Draw a cable with a multi-pass translucent "bloom" underlay plus a bright
/// core, approximating the SVG feGaussianBlur glow.
fn paint_cable(painter: &egui::Painter, pts: &[Pos2], color: Color32, hovered: bool) {
    let a = |alpha: u8| Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
    let passes: &[(f32, u8)] = if hovered {
        &[(13.0, 28), (9.0, 55), (6.0, 120), (5.0, 255)]
    } else {
        &[(11.0, 20), (7.0, 40), (4.5, 90), (3.5, 217)]
    };
    for &(w, alpha) in passes {
        painter.add(Shape::line(pts.to_vec(), Stroke::new(w, a(alpha))));
    }
}

// ─── Rendering ─────────────────────────────────────────────────────────────

/// A port dot. Records its centre into `anchors`, and registers a click/
/// right-click into the per-frame event buffers.
#[allow(clippy::too_many_arguments)]
fn port_dot(
    ui: &mut egui::Ui,
    fill: Color32,
    key: DotKey,
    side: Side,
    ports: Vec<String>,
    selected: bool,
    connected: bool,
    anchors: &mut HashMap<DotKey, Pos2>,
    clicks: &mut Vec<Endpoint>,
    right_clicks: &mut Vec<Vec<String>>,
) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(DOT_SIZE), Sense::click());
    let hovered = resp.hovered();
    let scale = if selected {
        1.4
    } else if hovered {
        1.3
    } else {
        1.0
    };
    let center = rect.center();
    let r = DOT_SIZE / 2.0 * scale;
    let painter = ui.painter();

    if selected {
        painter.circle_filled(center, r + 4.0, Color32::from_rgba_unmultiplied(255, 255, 255, 150));
    } else if connected {
        painter.circle_filled(center, r + 3.0, Color32::from_rgba_unmultiplied(255, 255, 255, 120));
    }
    painter.circle_filled(center, r, fill);
    if selected {
        painter.circle_stroke(center, r, Stroke::new(2.0_f32, Color32::WHITE));
    }

    anchors.insert(key.clone(), center);

    if resp.clicked() {
        clicks.push(Endpoint { side, key, ports: ports.clone() });
    }
    if resp.secondary_clicked() {
        right_clicks.push(ports);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// Outcome of the (self-immutable) rendering pass, applied to `self` afterwards.
#[derive(Default)]
struct FrameOut {
    anchors: HashMap<DotKey, Pos2>,
    dot_clicks: Vec<Endpoint>,
    dot_right_clicks: Vec<Vec<String>>,
    refresh_clicked: bool,
    mode_clicked: bool,
    new_sink_clicked: bool,
    delete_sink: Option<u32>,
    banner_dismissed: bool,
    modal_cancel: bool,
    modal_confirm: bool,
    patchbay_clip: Option<Rect>,
    card_rects: HashMap<String, Rect>,
}

impl eframe::App for AudioPlumberApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        let mut out = FrameOut::default();

        // Immutable borrow of self for rendering; mutations applied after.
        {
            let this: &AudioPlumberApp = self;
            let (connected_out, connected_in) = this.connected_sets();

            // ── Header ──
            egui::TopBottomPanel::top("header")
                .frame(egui::Frame::none().fill(SURFACE).inner_margin(Margin::symmetric(20.0, 10.0)))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("AudioPlumber")
                                .color(ACCENT)
                                .size(20.0)
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new("Visual PipeWire Patchbay")
                                .color(TEXT_DIM)
                                .size(12.0),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let r = ui.button(egui::RichText::new("Refresh").size(13.0));
                            a11y(ui.ctx(), r.id, Some("Refresh port list"), None, None);
                            if r.clicked() {
                                out.refresh_clicked = true;
                            }
                            let (mode_label, mode_name) = if this.simple_mode {
                                ("\u{2699} Advanced", "Switch to advanced mode (individual ports)")
                            } else {
                                ("\u{25ce} Simple", "Switch to simple mode (bundled stereo)")
                            };
                            let r = ui.button(egui::RichText::new(mode_label).color(TEAL).size(13.0));
                            a11y(ui.ctx(), r.id, Some(mode_name), None, None);
                            if r.clicked() {
                                out.mode_clicked = true;
                            }
                        });
                    });
                });

            // ── Error banner ──
            if let Some(msg) = &this.error_banner {
                egui::TopBottomPanel::top("banner")
                    .frame(
                        egui::Frame::none()
                            .fill(BANNER_BG)
                            .inner_margin(Margin::symmetric(20.0, 10.0)),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            // ARIA role="alert" aria-live="assertive" equivalent.
                            let lbl = ui.label(egui::RichText::new(msg).color(BANNER_TEXT).size(13.0));
                            a11y(ui.ctx(), lbl.id, Some(msg), Some(Role::Alert), Some(Live::Assertive));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let r = ui.button(egui::RichText::new("\u{00d7}").color(BANNER_TEXT));
                                a11y(ui.ctx(), r.id, Some("Dismiss error"), None, None);
                                if r.clicked() {
                                    out.banner_dismissed = true;
                                }
                            });
                        });
                    });
            }

            // ── Hint bar ──
            egui::TopBottomPanel::top("hint")
                .frame(egui::Frame::none().fill(SURFACE).inner_margin(Margin::symmetric(20.0, 5.0)))
                .show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Click a port on either side, then a port on the other side to connect \u{2014} a tentative cable follows your cursor. Press Esc to cancel. Click a cable to disconnect.",
                        )
                        .color(TEXT_DIM)
                        .size(12.0),
                    );
                });

            // ── Debug panel (bottom) ──
            if this.show_debug {
                egui::TopBottomPanel::bottom("debug")
                    .frame(egui::Frame::none().fill(DEBUG_BG).inner_margin(Margin::symmetric(16.0, 10.0)))
                    .resizable(false)
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                            let text = if this.debug_text.is_empty() {
                                "Loading debug info\u{2026}"
                            } else {
                                &this.debug_text
                            };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(text)
                                        .monospace()
                                        .size(11.0)
                                        .color(DEBUG_TEXT),
                                ),
                            );
                        });
                    });
            }

            // ── Left column: virtual sinks ──
            let left = egui::SidePanel::left("vsinks")
                .resizable(false)
                .exact_width(COL_WIDTH)
                .frame(egui::Frame::none().fill(BG).inner_margin(Margin::symmetric(0.0, 12.0)))
                .show(ctx, |ui| {
                    column_header(ui, "Virtual Sinks", Some("\u{ff0b} New Virtual Sink"), &mut out.new_sink_clicked);
                    egui::ScrollArea::vertical().id_source("left_scroll").show(ui, |ui| {
                        render_left_column(ui, this, &connected_out, &mut out);
                    });
                });

            // ── Right column: real outputs ──
            egui::SidePanel::right("outputs")
                .resizable(false)
                .exact_width(COL_WIDTH)
                .frame(egui::Frame::none().fill(BG).inner_margin(Margin::symmetric(0.0, 12.0)))
                .show(ctx, |ui| {
                    column_header(ui, "Outputs (auto)", None, &mut out.new_sink_clicked);
                    egui::ScrollArea::vertical().id_source("right_scroll").show(ui, |ui| {
                        render_right_column(ui, this, &connected_in, &mut out);
                    });
                });

            // ── Centre cable zone ──
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(BG))
                .show(ctx, |_ui| {});

            out.patchbay_clip = Some(Rect::from_min_max(
                Pos2::new(0.0, left.response.rect.top()),
                ctx.screen_rect().max,
            ));
        }

        // ── Draw cables + rubber-band on a foreground layer ──
        self.draw_cables(ctx, &out);

        // ── Modal ──
        if self.show_vsink_modal {
            self.draw_modal(ctx, &mut out);
        }

        // ── Apply frame outcomes to state ──
        self.apply_frame(ctx, out);
    }
}

fn column_header(ui: &mut egui::Ui, title: &str, action: Option<&str>, action_clicked: &mut bool) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(title.to_uppercase())
                .color(TEXT_DIM)
                .size(13.0)
                .strong(),
        );
        if let Some(label) = action {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(12.0);
                let r = ui.button(egui::RichText::new(label).color(TEAL).size(11.0));
                a11y(ui.ctx(), r.id, Some("New virtual sink"), None, None);
                if r.clicked() {
                    *action_clicked = true;
                }
            });
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
}

fn card_frame(virtual_sink: bool) -> egui::Frame {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, if virtual_sink { TEAL_BORDER } else { CARD_BORDER }))
        .rounding(Rounding::same(8.0))
        .inner_margin(Margin::symmetric(0.0, 10.0))
        .outer_margin(Margin {
            left: 12.0,
            right: 12.0,
            top: 0.0,
            bottom: 8.0,
        })
}

fn card_name(ui: &mut egui::Ui, name: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(egui::RichText::new(truncate(name, 34)).color(color).size(14.0).strong());
    });
    ui.add_space(4.0);
}

fn render_left_column(
    ui: &mut egui::Ui,
    app: &AudioPlumberApp,
    connected_out: &HashSet<String>,
    out: &mut FrameOut,
) {
    if app.virtual_sinks.is_empty() {
        empty_state(ui, "No virtual sinks yet. Click \u{ff0b} New Virtual Sink to create one.");
        return;
    }

    for vs in &app.virtual_sinks {
        let node_name = format!("AudioPlumber_{}", vs.name);
        let ports: Vec<&Port> = app
            .outputs
            .iter()
            .filter(|p| {
                p.node == node_name
                    || p.node.starts_with(&format!("{}.", node_name))
                    || p.node.starts_with(&format!("{}_", node_name))
            })
            .collect();

        let show_delete = app
            .sink_card_rects
            .get(&vs.name)
            .zip(ui.ctx().pointer_hover_pos())
            .map(|(rect, p)| rect.contains(p))
            .unwrap_or(false);

        let inner = card_frame(true).show(ui, |ui| {
            card_name(ui, &vs.name, TEAL);

            if ports.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        egui::RichText::new("Waiting for PipeWire\u{2026}")
                            .italics()
                            .color(TEXT_DIM)
                            .size(11.0),
                    );
                });
            } else if app.simple_mode {
                let actual_node = ports[0].node.clone();
                let port_ids: Vec<String> = ports.iter().map(|p| p.id.clone()).collect();
                let key = DotKey::OutNode(actual_node.clone());
                let selected = app.pending.as_ref().map(|p| p.key == key).unwrap_or(false);
                let connected = connected_out.contains(&actual_node);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(6.0);
                    port_dot(
                        ui, PORT_OUT, key, Side::Output, port_ids, selected, connected,
                        &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                    );
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(truncate(&vs.name, 30)).color(TEXT_DIM).size(11.0));
                });
            } else {
                for p in &ports {
                    let key = DotKey::OutPort(p.id.clone());
                    let selected = app.pending.as_ref().map(|pd| pd.key == key).unwrap_or(false);
                    let connected = connected_out.contains(&p.node);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(6.0);
                        port_dot(
                            ui, PORT_OUT, key, Side::Output, vec![p.id.clone()], selected, connected,
                            &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                        );
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(truncate(&p.port, 30)).color(TEXT_DIM).size(11.0));
                    });
                }
            }

            // Hover-reveal delete button.
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                if show_delete {
                    let btn = egui::Button::new(
                        egui::RichText::new("Delete sink").color(BANNER_BORDER).size(11.0),
                    )
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(0x5a, 0x1a, 0x1a)));
                    let r = ui.add(btn);
                    a11y(ui.ctx(), r.id, Some(&format!("Delete virtual sink {}", vs.name)), None, None);
                    if r.clicked() {
                        out.delete_sink = Some(vs.module_id);
                    }
                } else {
                    ui.add_space(DOT_SIZE);
                }
            });
        });

        out.card_rects.insert(vs.name.clone(), inner.response.rect);
    }
}

fn render_right_column(
    ui: &mut egui::Ui,
    app: &AudioPlumberApp,
    connected_in: &HashSet<String>,
    out: &mut FrameOut,
) {
    // Group inputs by node, preserving first-seen order.
    let mut order: Vec<String> = Vec::new();
    let mut by_node: HashMap<String, Vec<&Port>> = HashMap::new();
    for p in &app.inputs {
        if !by_node.contains_key(&p.node) {
            order.push(p.node.clone());
        }
        by_node.entry(p.node.clone()).or_default().push(p);
    }

    let mut any = false;
    for node_name in &order {
        let ports = &by_node[node_name];
        if node_name.starts_with("AudioPlumber_") {
            continue;
        }
        if node_name.to_lowercase().contains("midi") {
            continue; // MIDI excluded from the right column
        }
        if node_name.to_lowercase().starts_with("bluez_capture_internal") {
            continue;
        }
        if !ports.iter().any(|p| STEREO_PORT_NAMES.contains(&p.port.as_str())) {
            continue;
        }
        any = true;

        let display = app
            .node_names
            .get(node_name)
            .cloned()
            .unwrap_or_else(|| node_name.clone());

        card_frame(false).show(ui, |ui| {
            card_name(ui, &display, TEXT);

            if app.simple_mode {
                let port_ids: Vec<String> = ports.iter().map(|p| p.id.clone()).collect();
                let key = DotKey::InNode(node_name.clone());
                let selected = app.pending.as_ref().map(|p| p.key == key).unwrap_or(false);
                let connected = connected_in.contains(node_name);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(6.0);
                    port_dot(
                        ui, PORT_IN, key, Side::Input, port_ids, selected, connected,
                        &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                    );
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(truncate(&display, 30)).color(TEXT_DIM).size(11.0));
                });
            } else {
                for p in ports {
                    let key = DotKey::InPort(p.id.clone());
                    let selected = app.pending.as_ref().map(|pd| pd.key == key).unwrap_or(false);
                    let connected = connected_in.contains(&p.node);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add_space(6.0);
                        port_dot(
                            ui, PORT_IN, key, Side::Input, vec![p.id.clone()], selected, connected,
                            &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                        );
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(truncate(&p.port, 30)).color(TEXT_DIM).size(11.0));
                    });
                }
            }
        });
    }

    if !any {
        empty_state(ui, "No audio outputs found. Is PipeWire running?");
    }
}

fn empty_state(ui: &mut egui::Ui, msg: &str) {
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(msg).italics().color(TEXT_DIM).size(12.0));
    });
}

impl AudioPlumberApp {
    /// Build the list of cables to draw (and hit-test) for the current mode.
    fn build_cables(&self, anchors: &HashMap<DotKey, Pos2>) -> Vec<(Pos2, Pos2, Color32, CableId)> {
        let mut cables = Vec::new();
        if self.simple_mode {
            let mut seen: Vec<(String, String)> = Vec::new();
            for l in &self.links {
                let fnode = node_name_from_port_id(&l.from);
                let tnode = node_name_from_port_id(&l.to);
                if !seen.iter().any(|(f, t)| f == &fnode && t == &tnode) {
                    seen.push((fnode, tnode));
                }
            }
            for (idx, (fnode, tnode)) in seen.into_iter().enumerate() {
                if let (Some(&from), Some(&to)) = (
                    anchors.get(&DotKey::OutNode(fnode.clone())),
                    anchors.get(&DotKey::InNode(tnode.clone())),
                ) {
                    cables.push((from, to, cable_color(idx), CableId::Nodes(fnode, tnode)));
                }
            }
        } else {
            for (idx, l) in self.links.iter().enumerate() {
                if let (Some(&from), Some(&to)) = (
                    anchors.get(&DotKey::OutPort(l.from.clone())),
                    anchors.get(&DotKey::InPort(l.to.clone())),
                ) {
                    cables.push((from, to, cable_color(idx), CableId::Link(l.from.clone(), l.to.clone())));
                }
            }
        }
        cables
    }

    fn draw_cables(&mut self, ctx: &egui::Context, out: &FrameOut) {
        let painter = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::new("cables")));
        let painter = match out.patchbay_clip {
            Some(clip) => painter.with_clip_rect(clip),
            None => painter,
        };

        let pointer = ctx.pointer_hover_pos();
        let cables = self.build_cables(&out.anchors);

        // Determine hovered cable (closest within threshold) for highlight/click.
        // Not gated on the pending state: clicking a cable while a selection is
        // pending disconnects that cable (matches the old webview behaviour).
        let mut hovered_idx: Option<usize> = None;
        if let Some(p) = pointer {
            let mut best = 7.0f32;
            for (i, (from, to, _, _)) in cables.iter().enumerate() {
                let pts = sample_cable(*from, *to);
                let d = dist_to_polyline(&pts, p);
                if d < best {
                    best = d;
                    hovered_idx = Some(i);
                }
            }
        }

        for (i, (from, to, color, _)) in cables.iter().enumerate() {
            let pts = sample_cable(*from, *to);
            paint_cable(&painter, &pts, *color, Some(i) == hovered_idx);
        }

        // Rubber-band dangling cable while a connection is pending.
        if let Some(p) = &self.pending {
            if let Some(&anchor) = out.anchors.get(&p.key) {
                let free = pointer.unwrap_or(anchor);
                let (from, to) = if p.side == Side::Output {
                    (anchor, free)
                } else {
                    (free, anchor)
                };
                let pts = sample_cable(from, to);
                let glow = Color32::from_rgba_unmultiplied(0x8a, 0x8a, 0xaa, 90);
                painter.add(Shape::line(pts.clone(), Stroke::new(6.0_f32, glow)));
                for seg in Shape::dashed_line(&pts, Stroke::new(3.0_f32, TEXT_DIM), 7.0, 6.0) {
                    painter.add(seg);
                }
            }
            ctx.request_repaint(); // keep the rubber-band tracking the cursor
        }

        // Stash hovered cable id for click handling in apply_frame.
        self.hovered_cable = hovered_idx.map(|i| cables[i].3.clone());
    }

    fn draw_modal(&mut self, ctx: &egui::Context, out: &mut FrameOut) {
        // Dim backdrop.
        let bp = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::new("modal_bg")));
        bp.rect_filled(ctx.screen_rect(), Rounding::ZERO, Color32::from_rgba_unmultiplied(0, 0, 0, 180));

        let win = egui::Window::new(egui::RichText::new("NEW VIRTUAL SINK").color(TEXT).size(14.0).strong())
            .id(Id::new("vsink_modal"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::none()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0_f32, SURFACE2))
                    .rounding(Rounding::same(10.0))
                    .inner_margin(Margin::same(18.0)),
            )
            .show(ctx, |ui| {
                ui.set_width(300.0);
                ui.label(egui::RichText::new("NAME").color(TEXT_DIM).size(12.0));
                ui.add_space(6.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.vsink_input)
                        .hint_text("e.g. Recording Mix")
                        .char_limit(64) // parity with the old input's maxlength=64
                        .desired_width(f32::INFINITY),
                );
                if self.vsink_focus_pending {
                    resp.request_focus();
                    self.vsink_focus_pending = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    out.modal_confirm = true;
                }
                ui.add_space(14.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let confirm = egui::Button::new(egui::RichText::new("Create").color(Color32::WHITE).size(13.0))
                        .fill(ACCENT);
                    if ui.add(confirm).clicked() {
                        out.modal_confirm = true;
                    }
                    if ui
                        .button(egui::RichText::new("Cancel").color(TEXT_DIM).size(13.0))
                        .clicked()
                    {
                        out.modal_cancel = true;
                    }
                });
            });

        // ARIA role="dialog" aria-modal equivalent on the window node.
        if let Some(w) = win {
            a11y(ctx, w.response.id, Some("New Virtual Sink"), Some(Role::Dialog), None);
        }
    }

    fn apply_frame(&mut self, ctx: &egui::Context, out: FrameOut) {
        self.sink_card_rects = out.card_rects;

        // Keyboard: Esc and D.
        let (esc, d_key, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::D),
                i.key_pressed(Key::Enter),
            )
        });

        if out.banner_dismissed {
            self.error_banner = None;
        }
        if out.refresh_clicked {
            self.clear_selection();
            self.refresh();
        }
        if out.mode_clicked {
            self.simple_mode = !self.simple_mode;
            self.clear_selection();
        }
        if out.new_sink_clicked {
            self.show_vsink_modal = true;
            self.vsink_input.clear();
            self.vsink_focus_pending = true;
        }

        // Modal handling.
        if self.show_vsink_modal {
            if esc {
                // Esc closes the modal AND cancels any pending selection (parity).
                self.show_vsink_modal = false;
                self.clear_selection();
            } else if out.modal_cancel {
                self.show_vsink_modal = false;
            } else if out.modal_confirm || (enter && !self.vsink_input.trim().is_empty()) {
                let sanitized: String = self
                    .vsink_input
                    .trim()
                    .chars()
                    .filter(|c| *c != '\'' && *c != '"' && *c != '\\')
                    .collect();
                let name: String = collapse_whitespace(&sanitized);
                if !name.is_empty() {
                    self.show_vsink_modal = false;
                    let _ = self.cmd_tx.send(Cmd::CreateSink {
                        name,
                        sinks: self.virtual_sinks.clone(),
                    });
                }
            }
            // While the modal is open, swallow other interactions.
            return;
        }

        // Esc cancels any pending selection (modal already handled above).
        if esc {
            self.clear_selection();
        }

        // D toggles debug panel (never while the modal input had focus, handled above).
        if d_key {
            self.show_debug = !self.show_debug;
            if self.show_debug {
                self.debug_text.clear();
                let _ = self.cmd_tx.send(Cmd::FetchDebug);
            }
        }

        // Dot clicks (selection state machine).
        let dot_clicked = !out.dot_clicks.is_empty();
        for ep in out.dot_clicks {
            if let Some(pairs) = self.on_port_click(ep) {
                let _ = self.cmd_tx.send(Cmd::Connect(pairs));
            }
        }

        // Right-click disconnect.
        for port_ids in out.dot_right_clicks {
            let set: HashSet<&String> = port_ids.iter().collect();
            let pairs: Vec<(String, String)> = self
                .links
                .iter()
                .filter(|l| set.contains(&l.from) || set.contains(&l.to))
                .map(|l| (l.from.clone(), l.to.clone()))
                .collect();
            if !pairs.is_empty() {
                let _ = self.cmd_tx.send(Cmd::Disconnect(pairs));
            }
        }

        // Delete sink.
        if let Some(module_id) = out.delete_sink {
            let _ = self.cmd_tx.send(Cmd::DeleteSink {
                module_id,
                sinks: self.virtual_sinks.clone(),
            });
        }

        // Empty-space / cable click (only when not consumed by a widget).
        let click_consumed = dot_clicked
            || out.refresh_clicked
            || out.mode_clicked
            || out.new_sink_clicked
            || out.banner_dismissed
            || out.delete_sink.is_some();
        let primary_clicked = ctx.input(|i| i.pointer.primary_clicked());
        if primary_clicked && !click_consumed {
            if let Some(id) = self.hovered_cable.clone() {
                let pairs = match id {
                    CableId::Nodes(fnode, tnode) => self
                        .links
                        .iter()
                        .filter(|l| {
                            node_name_from_port_id(&l.from) == fnode
                                && node_name_from_port_id(&l.to) == tnode
                        })
                        .map(|l| (l.from.clone(), l.to.clone()))
                        .collect::<Vec<_>>(),
                    CableId::Link(f, t) => vec![(f, t)],
                };
                if !pairs.is_empty() {
                    self.clear_selection();
                    let _ = self.cmd_tx.send(Cmd::Disconnect(pairs));
                }
            } else {
                self.clear_selection();
            }
        }

        // Cancel pending if its anchor vanished (port removed, sink deleted…).
        if let Some(p) = &self.pending {
            if !out.anchors.contains_key(&p.key) {
                self.pending = None;
            }
        }

        // Validate: nothing else to do.
        let _ = ctx;
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws && !out.is_empty() {
                out.push('_');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

#[derive(Clone)]
enum CableId {
    Nodes(String, String),
    Link(String, String),
}

/// Launches the native egui application.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 700.0])
            .with_min_inner_size([700.0, 400.0])
            .with_title("AudioPlumber \u{2014} Visual Patchbay"),
        ..Default::default()
    };
    eframe::run_native(
        "AudioPlumber",
        options,
        Box::new(|cc| Ok(Box::new(AudioPlumberApp::new(cc)))),
    )
}
