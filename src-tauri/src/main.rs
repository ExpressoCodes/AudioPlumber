// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Port {
    pub node: String,
    pub port: String,
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Link {
    pub from: String,
    pub to: String,
}

/// Find an executable by name: try `which` first, fall back to NixOS system path.
fn find_binary(name: &str) -> String {
    if let Ok(output) = Command::new("which").arg(name).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return path;
            }
        }
    }
    format!("/run/current-system/sw/bin/{}", name)
}

/// For read-only pw-link queries (-o, -i, -l): lenient — non-zero exit is OK
/// as long as there is some stdout (or even if empty, just return it).
fn run_pw_link_query(args: &[&str]) -> Result<String, String> {
    let pw_link = find_binary("pw-link");
    let output = Command::new(&pw_link)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run pw-link ({}): {}", pw_link, e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        // pw-link sometimes exits non-zero even on success (e.g., no ports found)
        // so we return stdout even on failure, only error if there's a real stderr message
        if stdout.is_empty() && !stderr.is_empty() {
            Err(format!("pw-link error: {}", stderr))
        } else {
            Ok(stdout)
        }
    }
}

/// For connect/disconnect pw-link commands: strict — any non-zero exit is an error.
fn run_pw_link_cmd(args: &[&str]) -> Result<String, String> {
    let pw_link = find_binary("pw-link");
    let output = Command::new(&pw_link)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run pw-link ({}): {}", pw_link, e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let code = output.status.code().unwrap_or(-1);
        Err(format!("pw-link exited with status {}: {}", code, stderr))
    }
}

/// Trim whitespace from a port identifier.
fn normalize_port_id(id: &str) -> String {
    id.trim().to_string()
}

fn parse_ports(raw: &str) -> Vec<Port> {
    let mut ports = Vec::new();
    for line in raw.lines() {
        let line = normalize_port_id(line);
        if line.is_empty() {
            continue;
        }
        // Each line is "NodeName:port_name"
        if let Some(colon_pos) = line.find(':') {
            let node = line[..colon_pos].to_string();
            let port = line[colon_pos + 1..].to_string();
            let id = line.clone();
            ports.push(Port { node, port, id });
        }
    }
    ports
}

fn parse_links(raw: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut current_from: Option<String> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(arrow_pos) = trimmed.find("->") {
            // Could be "A:port -> B:port" or "|-> B:port"
            let before = trimmed[..arrow_pos].trim();
            let to = normalize_port_id(trimmed[arrow_pos + 2..].trim());
            if before.is_empty() || before == "|" {
                // Format B continuation — use current_from
                if let Some(ref from) = current_from {
                    if !to.is_empty() {
                        links.push(Link { from: from.clone(), to });
                    }
                }
            } else {
                // Format A — both on same line
                let from = normalize_port_id(before);
                current_from = Some(from.clone());
                if !to.is_empty() {
                    links.push(Link { from, to });
                }
            }
        } else if trimmed.contains(':') && !trimmed.starts_with('|') {
            // A port name line without arrow — this is the "from" in Format B
            current_from = Some(normalize_port_id(trimmed));
        }
    }
    links
}

#[tauri::command]
async fn get_outputs() -> Vec<Port> {
    match run_pw_link_query(&["-o"]) {
        Ok(raw) => parse_ports(&raw),
        Err(_) => Vec::new(),
    }
}

#[tauri::command]
async fn get_inputs() -> Vec<Port> {
    match run_pw_link_query(&["-i"]) {
        Ok(raw) => parse_ports(&raw),
        Err(_) => Vec::new(),
    }
}

#[tauri::command]
async fn get_links() -> Vec<Link> {
    match run_pw_link_query(&["-l"]) {
        Ok(raw) => parse_links(&raw),
        Err(_) => Vec::new(),
    }
}

#[tauri::command]
async fn connect(from: String, to: String) -> Result<(), String> {
    run_pw_link_cmd(&[&from, &to]).map(|_| ())
}

#[tauri::command]
async fn disconnect(from: String, to: String) -> Result<(), String> {
    run_pw_link_cmd(&["-d", &from, &to]).map(|_| ())
}

#[tauri::command]
async fn create_virtual_sink(name: String) -> Result<u32, String> {
    let pactl = find_binary("pactl");
    let sink_name = format!("AudioPlumber_{}", name);
    let sink_props = format!("device.description=\"{}\"", name);
    let output = Command::new(&pactl)
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", sink_name),
            &format!("sink_properties={}", sink_props),
        ])
        .output()
        .map_err(|e| format!("Failed to run pactl ({}): {}", pactl, e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let module_id: u32 = stdout
            .trim()
            .parse()
            .map_err(|_| format!("Unexpected pactl output: {}", stdout.trim()))?;
        Ok(module_id)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("pactl error: {}", stderr))
    }
}

#[tauri::command]
async fn delete_virtual_sink(module_id: u32) -> Result<(), String> {
    let pactl = find_binary("pactl");
    let output = Command::new(&pactl)
        .args(["unload-module", &module_id.to_string()])
        .output()
        .map_err(|e| format!("Failed to run pactl ({}): {}", pactl, e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("pactl error: {}", stderr))
    }
}

/// Returns a list of tool names that are not found on the system.
#[tauri::command]
async fn check_deps() -> Vec<String> {
    let mut missing = Vec::new();
    for tool in &["pactl", "pw-link"] {
        let path = find_binary(tool);
        if !std::path::Path::new(&path).exists() {
            missing.push(tool.to_string());
        }
    }
    missing
}

/// Runs pw-dump and returns a map of node.name → human-readable description
/// for all Audio/Sink, Audio/Source, and Audio/Duplex nodes.
#[tauri::command]
async fn get_node_names() -> HashMap<String, String> {
    let pw_dump = find_binary("pw-dump");
    let output = match Command::new(&pw_dump).output() {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };

    if !output.status.success() {
        return HashMap::new();
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();

    if let Some(arr) = json.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str())
                != Some("PipeWire:Interface:Node")
            {
                continue;
            }

            let props = match item.get("info").and_then(|i| i.get("props")) {
                Some(p) => p,
                None => continue,
            };

            let media_class = props
                .get("media.class")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !matches!(media_class, "Audio/Sink" | "Audio/Source" | "Audio/Duplex") {
                continue;
            }

            let node_name = match props.get("node.name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let description = props
                .get("node.description")
                .and_then(|v| v.as_str())
                .or_else(|| props.get("node.nick").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| node_name.clone());

            map.insert(node_name, description);
        }
    }

    map
}

/// Returns the path to the connections persistence file.
fn connections_file_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{}/.config", home)
        });
    PathBuf::from(base)
        .join("audioplumber")
        .join("connections.json")
}

/// Saves the full connections list to ~/.config/audioplumber/connections.json.
#[tauri::command]
async fn save_connections(connections: Vec<Link>) -> Result<(), String> {
    let path = connections_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
    }
    let json = serde_json::to_string_pretty(&connections)
        .map_err(|e| format!("Failed to serialize connections: {}", e))?;
    fs::write(&path, json)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(())
}

/// Reads saved connections. Returns empty vec if file doesn't exist or parse fails.
#[tauri::command]
async fn load_saved_connections() -> Vec<Link> {
    let path = connections_file_path();
    let data = match fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Vec<Link>>(&data).unwrap_or_default()
}

/// Process-global set of absent saved endpoints already logged, so the benign
/// "device not present" skip is reported once rather than on every refresh poll.
fn logged_absent_endpoints() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Collects the set of port identifiers (`Node:port`) currently present in
/// PipeWire, across both output (-o) and input (-i) ports.
fn current_port_ids() -> HashSet<String> {
    let mut ids = HashSet::new();
    for flag in ["-o", "-i"] {
        if let Ok(raw) = run_pw_link_query(&[flag]) {
            for port in parse_ports(&raw) {
                ids.insert(port.id);
            }
        }
    }
    ids
}

/// Reads saved connections and attempts to re-establish each one via pw-link.
///
/// This is called both at startup and on every refresh, so it doubles as the
/// auto-reconnect mechanism: a saved connection whose device/port is absent is
/// retried on each call and established as soon as the endpoints reappear.
///
/// Returns only *genuine* errors. A saved connection whose endpoint port is not
/// currently present (the device is unplugged/disabled) is a normal, expected
/// condition — it is skipped silently, kept in the config, and retried later.
/// A connection that is already active ("File exists") is likewise not an error.
/// Only a failure where *both* endpoints are present but the link still fails is
/// surfaced to the caller.
#[tauri::command]
async fn restore_connections() -> Vec<String> {
    let saved = {
        let path = connections_file_path();
        let data = match fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        serde_json::from_str::<Vec<Link>>(&data).unwrap_or_default()
    };

    let present = current_port_ids();

    let mut errors = Vec::new();
    for link in saved {
        // Benign skip: an endpoint is not currently present (device absent).
        // Keep the connection in the saved config and retry on a future call.
        if !present.contains(&link.from) || !present.contains(&link.to) {
            // restore_connections runs on every refresh poll, so log each absent
            // endpoint at most once per process instead of spamming every cycle.
            let key = format!("{} -> {}", link.from, link.to);
            if let Ok(mut seen) = logged_absent_endpoints().lock() {
                if seen.insert(key.clone()) {
                    eprintln!(
                        "restore_connections: skipping absent endpoint {} (device not present; will retry when it appears)",
                        key
                    );
                }
            }
            continue;
        }
        if let Err(e) = run_pw_link_cmd(&[&link.from, &link.to]) {
            // "File exists" means the link is already active in PipeWire — not an error.
            if e.contains("File exists") {
                continue;
            }
            // Both endpoints are present but the link genuinely failed — report it.
            errors.push(format!("{} -> {}: {}", link.from, link.to, e));
        }
    }
    errors
}

/// Returns raw output of pw-link -o, -i, -l for debugging port name issues.
#[tauri::command]
async fn get_debug_info() -> String {
    let outputs = run_pw_link_query(&["-o"])
        .unwrap_or_else(|e| format!("(error: {})", e));
    let inputs = run_pw_link_query(&["-i"])
        .unwrap_or_else(|e| format!("(error: {})", e));
    let links = run_pw_link_query(&["-l"])
        .unwrap_or_else(|e| format!("(error: {})", e));

    format!(
        "=== pw-link -o (outputs) ===\n{}\n=== pw-link -i (inputs) ===\n{}\n=== pw-link -l (links) ===\n{}",
        outputs, inputs, links
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ports_basic() {
        let raw = "Firefox:output_FL\nFirefox:output_FR\nBuilt-in Audio:playback_FL\n";
        let ports = parse_ports(raw);
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0].node, "Firefox");
        assert_eq!(ports[0].port, "output_FL");
        assert_eq!(ports[0].id, "Firefox:output_FL");
        assert_eq!(ports[2].node, "Built-in Audio");
    }

    #[test]
    fn test_parse_ports_empty() {
        let ports = parse_ports("");
        assert!(ports.is_empty());
    }

    #[test]
    fn test_parse_links_basic() {
        let raw = "Firefox:output_FL        ->      Built-in Audio:playback_FL\nFirefox:output_FR        ->      Built-in Audio:playback_FR\n";
        let links = parse_links(&raw);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].from, "Firefox:output_FL");
        assert_eq!(links[0].to, "Built-in Audio:playback_FL");
    }

    #[test]
    fn test_parse_links_midi() {
        let raw = "PipeWire Midi Bridge:input_ALSA-0       ->      Midi Through:Midi Through Port-0\n";
        let links = parse_links(&raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].from, "PipeWire Midi Bridge:input_ALSA-0");
        assert_eq!(links[0].to, "Midi Through:Midi Through Port-0");
    }

    #[test]
    fn test_parse_links_empty() {
        let links = parse_links("");
        assert!(links.is_empty());
    }

    #[test]
    fn test_parse_links_format_b() {
        let raw = "NodeA:port_FL\n      |-> NodeB:port_FL\n      |-> NodeC:port_FL\n";
        let links = parse_links(raw);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].from, "NodeA:port_FL");
        assert_eq!(links[0].to, "NodeB:port_FL");
        assert_eq!(links[1].from, "NodeA:port_FL");
        assert_eq!(links[1].to, "NodeC:port_FL");
    }

    #[test]
    fn test_normalize_port_id() {
        assert_eq!(normalize_port_id("  Firefox:output_FL  "), "Firefox:output_FL");
        assert_eq!(normalize_port_id("NodeA:port"), "NodeA:port");
    }
}
