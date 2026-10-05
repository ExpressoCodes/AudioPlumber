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
    Align, Align2, Color32, FontId, Id, Key, Layout, Margin, Order, Pos2, Rect, Rounding, Sense,
    Shape, Stroke, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::pipewire::{self, Link, NodeLabel, Port, VirtualSink};
use crate::vsinks::{self, DeletePlan, LiveSink, SinkRow, SinkState, SinkTarget};

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
/// Minimum width of each side column. Columns grow beyond this to fit their
/// card labels (see [`column_width`]).
const COL_WIDTH: f32 = 320.0;
/// Upper bound for a content-sized side column.
const COL_MAX_WIDTH: f32 = 520.0;
/// Horizontal room always left for the centre cable zone when columns widen.
const CABLE_ZONE_MIN: f32 = 160.0;
/// Extra column width so content never sits under the floating scroll bar.
const COL_SLACK: f32 = 8.0;
/// Card outer margin, left/right (the gap between a card and its column edge).
const CARD_OUTER_X: f32 = 12.0;
/// Card inner padding: the same on left/right (`X`) and top/bottom (`Y`) for
/// every card. In egui points, so already scaled for DPI.
const CARD_PAD_X: f32 = 10.0;
const CARD_PAD_Y: f32 = 8.0;
/// Vertical gap between stacked cards; matches the inner vertical padding.
const CARD_GAP: f32 = CARD_PAD_Y;
/// Vertical gap between a card's lines (title → subtitle → port rows).
const LINE_GAP: f32 = 3.0;
/// Gap between a port dot and its text.
const DOT_GAP: f32 = 8.0;
/// Port-dot gutter on a card's cable side. Every line of a card (title,
/// subtitle, port rows) reserves it, so all text shares one edge.
const DOT_GUTTER: f32 = DOT_SIZE + DOT_GAP;
/// Font sizes of a card's title line and its dim sub-lines / port rows.
const TITLE_SIZE: f32 = 14.0;
const SUB_SIZE: f32 = 11.0;
const REFRESH_SECS: u64 = 3;
/// Fixed height of a single port row. Bounding the row height keeps node cards
/// sized to their content (and stacked from the top) instead of stretching to
/// fill the column — and keeps each port dot anchored on its own row.
const ROW_H: f32 = 20.0;
/// Slot at the end of a virtual-sink card's title line for its hover-revealed
/// delete button. Always reserved (button shown or not), so the title elides
/// the same and the card never changes size when the button appears.
const DELETE_SLOT: f32 = ROW_H + 4.0;
/// Width of the "New Virtual Sink" modal card (matches the old `.modal-card-sm`).
const MODAL_W: f32 = 340.0;

// ─── Backend worker protocol ───────────────────────────────────────────────

#[derive(Default, Clone)]
struct Snapshot {
    outputs: Vec<Port>,
    inputs: Vec<Port>,
    links: Vec<Link>,
    node_labels: HashMap<String, NodeLabel>,
    /// Live AudioPlumber sinks (`None` when pw-dump is unavailable).
    live_sinks: Option<Vec<LiveSink>>,
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
    /// Recreate persisted sinks that are not loaded (startup J6 flow).
    EnsureSinks(Vec<VirtualSink>),
    Connect(Vec<(String, String)>),
    Disconnect(Vec<(String, String)>),
    CreateSink {
        name: String,
        sinks: Vec<VirtualSink>,
    },
    DeleteSink {
        target: SinkTarget,
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
        node_labels: pipewire::get_node_labels(),
        live_sinks: vsinks::list_live_sinks(),
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
            // Recreate only sinks verifiably absent from PipeWire's sink list;
            // if that list cannot be read, recreating blindly is exactly how
            // duplicate sinks appear, so do nothing.
            if let Some(live) = vsinks::list_live_sinks() {
                let (mut changed, missing) = vsinks::reconcile_saved(&mut sinks, &live);
                let mut recreated = false;
                for i in missing {
                    match pipewire::create_virtual_sink(&sinks[i].name) {
                        Ok(id) => {
                            sinks[i].module_id = id;
                            changed = true;
                            recreated = true;
                        }
                        Err(e) => {
                            eprintln!("Failed to recreate virtual sink \"{}\": {}", sinks[i].name, e);
                        }
                    }
                }
                if changed {
                    let _ = pipewire::save_virtual_sinks(&sinks);
                }
                if recreated {
                    std::thread::sleep(Duration::from_millis(500));
                }
            } else {
                eprintln!("EnsureSinks: pw-dump unavailable; not recreating virtual sinks");
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
        // Duplicate-name policy: refuse. pipewire-pulse would happily load a
        // second sink with the same node.name, whose ports are then
        // indistinguishable from the first one's.
        Cmd::CreateSink { name, sinks }
            if vsinks::name_taken(&name, &sinks, vsinks::list_live_sinks().as_deref()) =>
        {
            let _ = evt.send(Evt::CreateSinkError(format!(
                "A virtual sink named \"{}\" already exists. Choose a different name.",
                name
            )));
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
        Cmd::DeleteSink { target, sinks } => {
            // Only a module verified to be a live AudioPlumber sink is unloaded
            // (pipewire-pulse reuses module ids, so a stale id is unsafe).
            let live = vsinks::list_live_sinks();
            let remaining = match vsinks::plan_delete(&target, &sinks, live.as_deref()) {
                DeletePlan::Unload { module_id, remaining } => {
                    match pipewire::delete_virtual_sink(module_id) {
                        Ok(()) => remaining,
                        // Parity: delete failure is invisible (old code only setStatus()'d).
                        Err(e) => {
                            eprintln!("Failed to unload virtual sink module {}: {}", module_id, e);
                            return;
                        }
                    }
                }
                DeletePlan::Forget { remaining } => remaining,
                DeletePlan::Refuse => {
                    eprintln!("Not deleting {:?}: not a live AudioPlumber sink", target);
                    let _ = evt.send(Evt::Snapshot(poll_snapshot()));
                    return;
                }
            };
            let _ = pipewire::save_virtual_sinks(&remaining);
            let snap = poll_snapshot();
            let _ = pipewire::save_connections(snap.links.clone());
            let _ = evt.send(Evt::SinksChanged(remaining));
            let _ = evt.send(Evt::Snapshot(snap));
        }
        Cmd::FetchDebug => {
            let _ = evt.send(Evt::DebugInfo(pipewire::get_debug_info()));
        }
    }
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
    node_labels: HashMap<String, NodeLabel>,
    /// Saved definitions (`virtual_sinks.json`), used to recreate sinks.
    virtual_sinks: Vec<VirtualSink>,
    /// Live AudioPlumber sinks from the last snapshot.
    live_sinks: Option<Vec<LiveSink>>,
    /// Left-column cards: `virtual_sinks` reconciled with `live_sinks`.
    sink_rows: Vec<SinkRow>,

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
    sink_card_rects: HashMap<SinkTarget, Rect>,
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
            node_labels: HashMap::new(),
            sink_rows: vsinks::build_rows(&virtual_sinks, None),
            virtual_sinks,
            live_sinks: None,
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
                    self.node_labels = s.node_labels;
                    self.live_sinks = s.live_sinks;
                    self.sink_rows =
                        vsinks::build_rows(&self.virtual_sinks, self.live_sinks.as_deref());
                }
                Evt::SinksChanged(list) => {
                    self.virtual_sinks = list;
                    self.sink_rows =
                        vsinks::build_rows(&self.virtual_sinks, self.live_sinks.as_deref());
                }
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

// ─── Card labels & sizing ──────────────────────────────────────────────────

/// Where to cut a label that does not fit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Elide {
    /// Keep the start (the label leads with its distinguishing part).
    End,
    /// Keep both ends (for unsplit labels, whose distinguishing part is
    /// usually at the end, e.g. `alsa_output…HiFi__Speaker__sink`).
    Middle,
}

/// Shorten `text` with an ellipsis until `measure` says it fits in `max_w`.
fn elide_to_width(text: &str, max_w: f32, mode: Elide, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |keep: usize| -> String {
        match mode {
            Elide::End => {
                let head: String = chars[..keep].iter().collect();
                format!("{}\u{2026}", head.trim_end())
            }
            Elide::Middle => {
                let head_n = keep / 2;
                let head: String = chars[..head_n].iter().collect();
                let tail: String = chars[chars.len() - (keep - head_n)..].iter().collect();
                format!("{}\u{2026}{}", head.trim_end(), tail.trim_start())
            }
        }
    };
    // Largest number of kept characters whose elided form still fits.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&build(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    build(lo)
}

fn text_width(ctx: &egui::Context, text: &str, size: f32) -> f32 {
    ctx.fonts(|f| {
        f.layout_no_wrap(text.to_string(), FontId::proportional(size), TEXT)
            .size()
            .x
    })
}

/// `text` elided to fit `max_w` at font `size`.
fn fit_text(ui: &egui::Ui, text: &str, size: f32, max_w: f32, mode: Elide) -> String {
    let ctx = ui.ctx();
    elide_to_width(text, max_w.max(0.0), mode, |s| text_width(ctx, s, size))
}

/// Byte length of the longest whole-word prefix `a` shares with `b`: the
/// prefix must be followed by whitespace in `a` (and whitespace or the end of
/// `b`). Returns 0 when they share no whole word.
fn common_word_prefix(a: &str, b: &str) -> usize {
    let mut last = 0;
    let mut bi = b.chars();
    for (i, ca) in a.char_indices() {
        let cb = bi.next();
        if ca.is_whitespace() && cb.is_none_or(char::is_whitespace) {
            last = i;
        }
        if cb != Some(ca) {
            break;
        }
    }
    last
}

/// Split a node description into `(distinguishing, shared)` parts so sibling
/// outputs of one controller can be told apart, e.g.
/// "Meteor Lake-P HD Audio Controller Speaker" →
/// ("Speaker", Some("Meteor Lake-P HD Audio Controller")).
///
/// Tried in order: the parent device name as a prefix; the longest multi-word
/// prefix shared with another visible card (`siblings`); the node nick as a
/// suffix. Falls back to the whole description with no shared part.
fn split_description(
    desc: &str,
    device: Option<&str>,
    nick: Option<&str>,
    siblings: &[&str],
) -> (String, Option<String>) {
    let desc = desc.trim();

    if let Some(dev) = device.map(str::trim).filter(|d| !d.is_empty()) {
        if let Some(rest) = desc.strip_prefix(dev) {
            if rest.starts_with(char::is_whitespace) && !rest.trim().is_empty() {
                return (rest.trim().to_string(), Some(dev.to_string()));
            }
        }
    }

    let shared = siblings
        .iter()
        .map(|s| common_word_prefix(desc, s.trim()))
        .max()
        .unwrap_or(0);
    if shared > 0 {
        let (pre, rest) = (desc[..shared].trim(), desc[shared..].trim());
        // Require 2+ shared words so a lone common word ("HDMI", "USB") is not
        // mistaken for the device name.
        if pre.contains(char::is_whitespace) && !rest.is_empty() {
            return (rest.to_string(), Some(pre.to_string()));
        }
    }

    if let Some(nick) = nick.map(str::trim).filter(|n| !n.is_empty()) {
        if let Some(pre) = desc.strip_suffix(nick) {
            if pre.ends_with(char::is_whitespace) && !pre.trim().is_empty() {
                return (nick.to_string(), Some(pre.trim().to_string()));
            }
        }
    }

    (desc.to_string(), None)
}

/// Display text for an output node card.
struct CardLabel {
    /// Title line: the part that differs between sibling outputs.
    primary: String,
    /// Dim sub-line: the shared controller/device name, else the raw node name
    /// (empty when that would just repeat `primary`).
    secondary: String,
    /// How to elide `primary` if it still does not fit.
    primary_elide: Elide,
    /// Full, untruncated description (tooltip).
    description: String,
}

fn card_label(node_name: &str, info: Option<&NodeLabel>, siblings: &[&str]) -> CardLabel {
    let description = info
        .map(|l| l.description.clone())
        .unwrap_or_else(|| node_name.to_string());
    let (primary, shared) = split_description(
        &description,
        info.and_then(|l| l.device.as_deref()),
        info.and_then(|l| l.nick.as_deref()),
        siblings,
    );
    let primary_elide = if shared.is_some() { Elide::End } else { Elide::Middle };
    let secondary = shared.unwrap_or_else(|| {
        if node_name == primary {
            String::new()
        } else {
            node_name.to_string()
        }
    });
    CardLabel {
        primary,
        secondary,
        primary_elide,
        description,
    }
}

/// One card in the right ("Outputs") column.
struct OutputCard<'a> {
    node_name: &'a str,
    ports: Vec<&'a Port>,
    label: CardLabel,
}

/// The right-column cards (input ports grouped by node, first-seen order,
/// filtered as before), with labels split against each other.
fn output_cards(app: &AudioPlumberApp) -> Vec<OutputCard<'_>> {
    let mut order: Vec<&str> = Vec::new();
    let mut by_node: HashMap<&str, Vec<&Port>> = HashMap::new();
    for p in &app.inputs {
        if !by_node.contains_key(p.node.as_str()) {
            order.push(&p.node);
        }
        by_node.entry(&p.node).or_default().push(p);
    }

    let shown: Vec<(&str, Vec<&Port>)> = order
        .into_iter()
        .filter_map(|node_name| {
            let ports = by_node.remove(node_name)?;
            let lower = node_name.to_lowercase();
            let keep = !node_name.starts_with("AudioPlumber_")
                && !lower.contains("midi") // MIDI excluded from the right column
                && !lower.starts_with("bluez_capture_internal")
                && ports.iter().any(|p| STEREO_PORT_NAMES.contains(&p.port.as_str()));
            keep.then_some((node_name, ports))
        })
        .collect();

    let descs: Vec<&str> = shown
        .iter()
        .map(|(n, _)| app.node_labels.get(*n).map_or(*n, |l| l.description.as_str()))
        .collect();

    shown
        .into_iter()
        .enumerate()
        .map(|(i, (node_name, ports))| {
            let siblings: Vec<&str> = descs
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, d)| *d)
                .collect();
            let label = card_label(node_name, app.node_labels.get(node_name), &siblings);
            OutputCard {
                node_name,
                ports,
                label,
            }
        })
        .collect()
}

/// Output ports belonging to a virtual-sink card (exact `node.name` match for
/// live sinks; none for duplicated names — see [`vsinks::row_owns_port`]).
fn sink_ports<'a>(app: &'a AudioPlumberApp, vs: &SinkRow) -> Vec<&'a Port> {
    app.outputs
        .iter()
        .filter(|p| vsinks::row_owns_port(vs, &p.node))
        .collect()
}

/// Column width a card needs to show its title (plus `title_extra` points
/// reserved after it on the same line) and its sub-lines (`rows`) without
/// eliding: the widest line plus the dot gutter, the inner padding and the
/// outer margin. Mirrors the layout in [`card_row`].
fn card_needed_width<'a>(
    ctx: &egui::Context,
    title: &str,
    title_extra: f32,
    rows: impl IntoIterator<Item = &'a str>,
) -> f32 {
    let text_w = rows
        .into_iter()
        .map(|t| text_width(ctx, t, SUB_SIZE))
        .fold(text_width(ctx, title, TITLE_SIZE) + title_extra, f32::max);
    text_w + DOT_GUTTER + 2.0 * (CARD_PAD_X + CARD_OUTER_X)
}

/// Side-column width: sized to `content_w`, never below [`COL_WIDTH`], and
/// capped so the centre cable zone keeps at least [`CABLE_ZONE_MIN`].
fn column_width(screen_w: f32, content_w: f32) -> f32 {
    let cap = ((screen_w - CABLE_ZONE_MIN) / 2.0).clamp(COL_WIDTH, COL_MAX_WIDTH);
    (content_w + COL_SLACK).clamp(COL_WIDTH, cap)
}

fn left_column_width(ctx: &egui::Context, app: &AudioPlumberApp) -> f32 {
    let content = app
        .sink_rows
        .iter()
        .map(|vs| {
            let ports = sink_ports(app, vs);
            let rows = std::iter::once(vs.label.as_str()).chain(ports.iter().map(|p| p.port.as_str()));
            card_needed_width(ctx, &vs.label, DELETE_SLOT, rows)
        })
        .fold(0.0, f32::max);
    column_width(ctx.screen_rect().width(), content)
}

fn right_column_width(ctx: &egui::Context, cards: &[OutputCard<'_>]) -> f32 {
    let content = cards
        .iter()
        .map(|c| {
            let rows = std::iter::once(c.label.secondary.as_str())
                .chain(c.ports.iter().map(|p| p.port.as_str()));
            card_needed_width(ctx, &c.label.primary, 0.0, rows)
        })
        .fold(0.0, f32::max);
    column_width(ctx.screen_rect().width(), content)
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
    delete_sink: Option<SinkTarget>,
    banner_dismissed: bool,
    modal_cancel: bool,
    modal_confirm: bool,
    modal_rect: Option<Rect>,
    patchbay_clip: Option<Rect>,
    card_rects: HashMap<SinkTarget, Rect>,
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

            // Column widths follow their card labels (bounded; see column_width).
            let cards = output_cards(this);
            let left_w = left_column_width(ctx, this);
            let right_w = right_column_width(ctx, &cards);

            // ── Left column: virtual sinks ──
            let left = egui::SidePanel::left("vsinks")
                .resizable(false)
                .exact_width(left_w)
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
                .exact_width(right_w)
                .frame(egui::Frame::none().fill(BG).inner_margin(Margin::symmetric(0.0, 12.0)))
                .show(ctx, |ui| {
                    column_header(ui, "Outputs (auto)", None, &mut out.new_sink_clicked);
                    egui::ScrollArea::vertical().id_source("right_scroll").show(ui, |ui| {
                        render_right_column(ui, this, &cards, &connected_in, &mut out);
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
        .inner_margin(Margin::symmetric(CARD_PAD_X, CARD_PAD_Y))
        .outer_margin(Margin {
            left: CARD_OUTER_X,
            right: CARD_OUTER_X,
            top: 0.0,
            bottom: CARD_GAP,
        })
}

/// Start of a card's contents: span the full column width (so every card in a
/// column is the same width) and use only explicit horizontal gaps, so text
/// widths computed by [`card_needed_width`] match the real layout.
fn card_body(ui: &mut egui::Ui) {
    ui.set_min_width(ui.available_width());
    ui.spacing_mut().item_spacing = Vec2::new(0.0, LINE_GAP);
}

/// Fills a card row's dot gutter with nothing.
fn no_dot(ui: &mut egui::Ui) {
    ui.add_space(DOT_SIZE);
}

/// One fixed-height ([`ROW_H`]) line of a card. The dot gutter sits on the
/// card's cable side (left for inputs, right for outputs) and is reserved on
/// every line; `dot` fills it (a port dot or [`no_dot`]). `text` is laid out
/// left-aligned in the remaining width, so all lines share one text edge.
fn card_row(
    ui: &mut egui::Ui,
    side: Side,
    dot: impl FnOnce(&mut egui::Ui),
    text: impl FnOnce(&mut egui::Ui),
) {
    let size = Vec2::new(ui.available_width(), ROW_H);
    match side {
        Side::Input => {
            ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_size(size);
                dot(ui);
                ui.add_space(DOT_GAP);
                text(ui);
            });
        }
        Side::Output => {
            ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), |ui| {
                ui.set_min_size(size);
                dot(ui);
                ui.add_space(DOT_GAP);
                ui.with_layout(Layout::left_to_right(Align::Center), text);
            });
        }
    }
}

fn card_name(ui: &mut egui::Ui, side: Side, name: &str, color: Color32, elide: Elide) {
    card_row(ui, side, no_dot, |ui| {
        let text = fit_text(ui, name, TITLE_SIZE, ui.available_width(), elide);
        ui.label(egui::RichText::new(text).color(color).size(TITLE_SIZE).strong());
    });
}

/// Dim sub-line label (subtitle / port name), elided to the remaining width.
fn row_label(ui: &mut egui::Ui, text: &str) {
    let text = fit_text(ui, text, SUB_SIZE, ui.available_width(), Elide::End);
    ui.label(egui::RichText::new(text).color(TEXT_DIM).size(SUB_SIZE));
}

/// Card hover tooltip: the full, untruncated name(s).
fn card_tooltip(ui: &mut egui::Ui, name: &str, detail: Option<&str>) {
    ui.label(egui::RichText::new(name).color(TEXT).strong());
    if let Some(d) = detail.filter(|d| !d.is_empty() && *d != name) {
        ui.label(egui::RichText::new(d).monospace().size(SUB_SIZE).color(TEXT_DIM));
    }
}

fn render_left_column(
    ui: &mut egui::Ui,
    app: &AudioPlumberApp,
    connected_out: &HashSet<String>,
    out: &mut FrameOut,
) {
    if app.sink_rows.is_empty() {
        empty_state(ui, "No virtual sinks yet. Click \u{ff0b} New Virtual Sink to create one.");
        return;
    }

    // One card per live sink (keyed by module id) or saved-but-unloaded
    // definition (keyed by name) — see `vsinks::build_rows`.
    for vs in &app.sink_rows {
        let ports = sink_ports(app, vs);

        let show_delete = app
            .sink_card_rects
            .get(&vs.target)
            .zip(ui.ctx().pointer_hover_pos())
            .map(|(rect, p)| rect.contains(p))
            .unwrap_or(false);

        let inner = card_frame(true).show(ui, |ui| {
            card_body(ui);
            // Title line: name, then the delete slot (button on hover). The
            // slot sits in the text area, left of the (empty) dot gutter, so it
            // never covers a port dot and adds no height to the card.
            card_row(ui, Side::Output, no_dot, |ui| {
                let max_w = (ui.available_width() - DELETE_SLOT).max(0.0);
                let text = fit_text(ui, &vs.label, TITLE_SIZE, max_w, Elide::End);
                ui.label(egui::RichText::new(text).color(TEAL).size(TITLE_SIZE).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if show_delete {
                        let btn = egui::Button::new(
                            egui::RichText::new("\u{1f5d1}").color(BANNER_BORDER).size(SUB_SIZE),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(0x5a, 0x1a, 0x1a)))
                        .min_size(Vec2::new(ROW_H, ROW_H - 4.0));
                        let r = ui.add(btn).on_hover_text("Delete sink");
                        a11y(ui.ctx(), r.id, Some(&format!("Delete virtual sink {}", vs.label)), None, None);
                        if r.clicked() {
                            out.delete_sink = Some(vs.target.clone());
                        }
                    }
                });
            });

            if ports.is_empty() {
                // Same-named sinks share port ids, so neither can be routed
                // until one copy is deleted.
                let status = match vs.state {
                    SinkState::Live { duplicate: true, .. } => "Duplicate name \u{2014} delete one copy",
                    _ => "Waiting for PipeWire\u{2026}",
                };
                card_row(ui, Side::Output, no_dot, |ui| {
                    ui.label(
                        egui::RichText::new(status)
                            .italics()
                            .color(TEXT_DIM)
                            .size(SUB_SIZE),
                    );
                });
            } else if app.simple_mode {
                let actual_node = ports[0].node.clone();
                let port_ids: Vec<String> = ports.iter().map(|p| p.id.clone()).collect();
                let key = DotKey::OutNode(actual_node.clone());
                let selected = app.pending.as_ref().map(|p| p.key == key).unwrap_or(false);
                let connected = connected_out.contains(&actual_node);
                card_row(
                    ui,
                    Side::Output,
                    |ui| {
                        port_dot(
                            ui, PORT_OUT, key, Side::Output, port_ids, selected, connected,
                            &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                        );
                    },
                    |ui| row_label(ui, &vs.label),
                );
            } else {
                for p in &ports {
                    let key = DotKey::OutPort(p.id.clone());
                    let selected = app.pending.as_ref().map(|pd| pd.key == key).unwrap_or(false);
                    let connected = connected_out.contains(&p.node);
                    card_row(
                        ui,
                        Side::Output,
                        |ui| {
                            port_dot(
                                ui, PORT_OUT, key, Side::Output, vec![p.id.clone()], selected, connected,
                                &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                            );
                        },
                        |ui| row_label(ui, &p.port),
                    );
                }
            }
        });

        out.card_rects.insert(vs.target.clone(), inner.response.rect);
        inner.response.on_hover_ui(|ui| {
            card_tooltip(ui, &vs.label, ports.first().map(|p| p.node.as_str()));
        });
    }
}

fn render_right_column(
    ui: &mut egui::Ui,
    app: &AudioPlumberApp,
    cards: &[OutputCard<'_>],
    connected_in: &HashSet<String>,
    out: &mut FrameOut,
) {
    for card in cards {
        let node_name = card.node_name;
        let ports = &card.ports;
        let label = &card.label;

        let inner = card_frame(false).show(ui, |ui| {
            card_body(ui);
            card_name(ui, Side::Input, &label.primary, TEXT, label.primary_elide);

            if app.simple_mode {
                let port_ids: Vec<String> = ports.iter().map(|p| p.id.clone()).collect();
                let key = DotKey::InNode(node_name.to_string());
                let selected = app.pending.as_ref().map(|p| p.key == key).unwrap_or(false);
                let connected = connected_in.contains(node_name);
                card_row(
                    ui,
                    Side::Input,
                    |ui| {
                        port_dot(
                            ui, PORT_IN, key, Side::Input, port_ids, selected, connected,
                            &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                        );
                    },
                    |ui| row_label(ui, &label.secondary),
                );
            } else {
                if !label.secondary.is_empty() {
                    card_row(ui, Side::Input, no_dot, |ui| row_label(ui, &label.secondary));
                }
                for p in ports {
                    let key = DotKey::InPort(p.id.clone());
                    let selected = app.pending.as_ref().map(|pd| pd.key == key).unwrap_or(false);
                    let connected = connected_in.contains(&p.node);
                    card_row(
                        ui,
                        Side::Input,
                        |ui| {
                            port_dot(
                                ui, PORT_IN, key, Side::Input, vec![p.id.clone()], selected, connected,
                                &mut out.anchors, &mut out.dot_clicks, &mut out.dot_right_clicks,
                            );
                        },
                        |ui| row_label(ui, &p.port),
                    );
                }
            }
        });
        inner.response.on_hover_ui(|ui| {
            card_tooltip(ui, &label.description, Some(node_name));
        });
    }

    if cards.is_empty() {
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
        // While the modal is open, the dim backdrop covers the patchbay — don't
        // draw cables over/under it (keeps the backdrop uniform).
        if self.show_vsink_modal {
            self.hovered_cable = None;
            return;
        }
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
        // Semi-transparent dim backdrop that dims only the content BEHIND the card.
        // Painted on the foreground layer (above the cables) first; the modal
        // window below is forced to a higher order so the card itself is NOT dimmed.
        let bp = ctx.layer_painter(egui::LayerId::new(Order::Foreground, Id::new("modal_backdrop")));
        bp.rect_filled(
            ctx.screen_rect(),
            Rounding::ZERO,
            Color32::from_rgba_unmultiplied(0, 0, 0, 180),
        );

        let inner_w = MODAL_W - 36.0; // minus the 18px inner margins on each side
        let card = egui::Frame::none()
            .fill(SURFACE)
            .stroke(Stroke::new(1.0_f32, SURFACE2))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::same(18.0))
            .shadow(egui::epaint::Shadow {
                offset: egui::vec2(0.0, 8.0),
                blur: 32.0,
                spread: 0.0,
                color: Color32::from_black_alpha(140),
            });

        // A contained, centered dialog CARD with a fixed width (auto height).
        let win = egui::Window::new("vsink_modal")
            .id(Id::new("vsink_modal"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .order(Order::Tooltip) // strictly above the Foreground backdrop
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(card)
            .show(ctx, |ui| {
                ui.set_width(inner_w);

                // Styled title header (like the old `.modal-title`).
                ui.label(egui::RichText::new("NEW VIRTUAL SINK").color(TEXT).size(14.0).strong());
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(10.0);

                ui.label(egui::RichText::new("NAME").color(TEXT_DIM).size(12.0));
                ui.add_space(6.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.vsink_input)
                        .hint_text("e.g. Recording Mix")
                        .char_limit(64) // parity with the old input's maxlength=64
                        .desired_width(inner_w), // finite: keeps the card from stretching
                );
                if self.vsink_focus_pending {
                    resp.request_focus();
                    self.vsink_focus_pending = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    out.modal_confirm = true;
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(10.0);

                // Cancel / Create row, right-aligned, bounded height.
                ui.allocate_ui_with_layout(
                    Vec2::new(inner_w, 28.0),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        let confirm = egui::Button::new(
                            egui::RichText::new("Create").color(Color32::WHITE).size(13.0),
                        )
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
                    },
                );
            });

        // Record the card rect (for click-outside-to-close) and set the
        // ARIA role="dialog" + accessible name on the window node.
        if let Some(w) = win {
            out.modal_rect = Some(w.response.rect);
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

        // ── Modal: handled FIRST and exclusively while open, so it is truly
        //    modal — background buttons/clicks are ignored behind it. ──
        if self.show_vsink_modal {
            let outside_click = {
                let (clicked, pos) =
                    ctx.input(|i| (i.pointer.primary_clicked(), i.pointer.interact_pos()));
                match (clicked, pos, out.modal_rect) {
                    (true, Some(p), Some(r)) => !r.contains(p), // click outside the card
                    _ => false,
                }
            };
            if esc {
                // Esc closes the modal AND cancels any pending selection (parity).
                self.show_vsink_modal = false;
                self.clear_selection();
            } else if out.modal_cancel || outside_click {
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
            return;
        }

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

        // Esc cancels any pending selection (no modal open here).
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
        if let Some(target) = &out.delete_sink {
            let _ = self.cmd_tx.send(Cmd::DeleteSink {
                target: target.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "Meteor Lake-P HD Audio Controller";

    fn info(desc: &str, nick: Option<&str>, device: Option<&str>) -> NodeLabel {
        NodeLabel {
            description: desc.to_string(),
            nick: nick.map(str::to_string),
            device: device.map(str::to_string),
        }
    }

    #[test]
    fn split_uses_device_prefix() {
        let desc = format!("{} HDMI / DisplayPort 1 Output (Stereo)", DEV);
        let (p, s) = split_description(&desc, Some(DEV), Some("HDMI 1"), &[]);
        assert_eq!(p, "HDMI / DisplayPort 1 Output (Stereo)");
        assert_eq!(s.as_deref(), Some(DEV));
    }

    #[test]
    fn split_falls_back_to_shared_sibling_prefix() {
        let a = format!("{} Speaker", DEV);
        let b = format!("{} HDMI / DisplayPort 2 Output (Stereo)", DEV);
        let (p, s) = split_description(&a, None, None, &[&b]);
        assert_eq!(p, "Speaker");
        assert_eq!(s.as_deref(), Some(DEV));
        // A sibling that is exactly the shared part also counts.
        let (p, s) = split_description("Built-in Audio Analog Stereo", None, None, &["Built-in Audio"]);
        assert_eq!(p, "Analog Stereo");
        assert_eq!(s.as_deref(), Some("Built-in Audio"));
    }

    #[test]
    fn split_ignores_single_shared_word() {
        let (p, s) = split_description("HDMI Output", None, None, &["HDMI Input"]);
        assert_eq!(p, "HDMI Output");
        assert_eq!(s, None);
    }

    #[test]
    fn split_uses_nick_suffix() {
        let desc = format!("{} Speaker", DEV);
        let (p, s) = split_description(&desc, None, Some("Speaker"), &[]);
        assert_eq!(p, "Speaker");
        assert_eq!(s.as_deref(), Some(DEV));
    }

    #[test]
    fn split_keeps_unsplittable_description_whole() {
        // Bluetooth-style: description == device == nick.
        let (p, s) = split_description("WH-1000XM4", Some("WH-1000XM4"), Some("WH-1000XM4"), &[]);
        assert_eq!(p, "WH-1000XM4");
        assert_eq!(s, None);
        // A nick that is the leading (shared) part is not used as the title.
        let (p, s) = split_description("Scarlett 2i2 USB Analog Stereo", None, Some("Scarlett 2i2 USB"), &[]);
        assert_eq!(p, "Scarlett 2i2 USB Analog Stereo");
        assert_eq!(s, None);
    }

    #[test]
    fn sibling_cards_get_distinct_primaries() {
        let names = ["Speaker", "HDMI / DisplayPort 1 Output (Stereo)", "HDMI / DisplayPort 2 Output (Stereo)"];
        let descs: Vec<String> = names.iter().map(|n| format!("{} {}", DEV, n)).collect();
        for (i, d) in descs.iter().enumerate() {
            let siblings: Vec<&str> = descs.iter().filter(|o| *o != d).map(String::as_str).collect();
            let l = card_label("node", Some(&info(d, None, Some(DEV))), &siblings);
            assert_eq!(l.primary, names[i]);
            assert_eq!(l.secondary, DEV);
            assert_eq!(l.description, *d);
            assert_eq!(l.primary_elide, Elide::End);
        }
    }

    #[test]
    fn card_label_without_metadata_uses_node_name() {
        let n = "alsa_output.pci-0000_00_1f.3.HiFi__Speaker__sink";
        let l = card_label(n, None, &[]);
        assert_eq!(l.primary, n);
        assert_eq!(l.secondary, "");
        assert_eq!(l.primary_elide, Elide::Middle);
        // Unsplit description: the raw node name becomes the sub-line.
        let l = card_label("bluez_output.AA_BB.1", Some(&info("WH-1000XM4", None, None)), &[]);
        assert_eq!(l.primary, "WH-1000XM4");
        assert_eq!(l.secondary, "bluez_output.AA_BB.1");
    }

    /// 1 unit per char.
    fn chars(s: &str) -> f32 {
        s.chars().count() as f32
    }

    #[test]
    fn elide_end_and_middle() {
        assert_eq!(elide_to_width("Speaker", 7.0, Elide::End, chars), "Speaker");
        assert_eq!(elide_to_width("Headphones", 6.0, Elide::End, chars), "Headp\u{2026}");
        assert_eq!(elide_to_width("abcdefghij", 7.0, Elide::Middle, chars), "abc\u{2026}hij");
        assert_eq!(elide_to_width("abc", 0.0, Elide::End, chars), "\u{2026}");
        // Trailing whitespace before the ellipsis is dropped.
        assert_eq!(elide_to_width("HDMI / DisplayPort", 8.0, Elide::End, chars), "HDMI /\u{2026}");
    }

    #[test]
    fn column_width_bounds() {
        // Short content keeps the historical minimum.
        assert_eq!(column_width(1200.0, 100.0), COL_WIDTH);
        // Content-sized in between.
        assert_eq!(column_width(1200.0, 400.0), 400.0 + COL_SLACK);
        // Capped by the max width…
        assert_eq!(column_width(3000.0, 900.0), COL_MAX_WIDTH);
        // …and by the window, leaving room for cables, but never below the minimum.
        assert_eq!(column_width(900.0, 900.0), (900.0 - CABLE_ZONE_MIN) / 2.0);
        assert_eq!(column_width(700.0, 900.0), COL_WIDTH);
    }
}
