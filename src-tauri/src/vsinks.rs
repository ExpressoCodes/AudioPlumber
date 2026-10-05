//! Virtual-sink identity: which live PipeWire sinks belong to AudioPlumber,
//! how they relate to the saved definitions in `virtual_sinks.json`, and what
//! the left column shows for them.
//!
//! Identity rules
//! - A *live* AudioPlumber sink is an `Audio/Sink` node loaded through a
//!   PulseAudio module (it has `pulse.module.id`) that either carries the
//!   `audioplumber.managed` marker (set at creation since this change) or whose
//!   `node.name` starts with `AudioPlumber_` (sinks created by older builds).
//!   Live sinks are discovered from `pw-dump` on every refresh, so sinks left
//!   over from a previous run or a crash are listed and can be deleted.
//! - The unique key of a live sink is its `pulse.module.id` — exactly what
//!   `pactl unload-module` needs. `node.name` is NOT unique: pipewire-pulse
//!   happily loads two null-sinks with the same `sink_name`.
//! - A saved definition with no live sink is shown as "not loaded" and keyed by
//!   its (unique) name; deleting it only forgets the definition — nothing is
//!   unloaded, because its stored module id is stale and pipewire-pulse reuses
//!   module ids.

use crate::pipewire::{self, VirtualSink};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Prefix of every AudioPlumber null-sink's `sink_name` / `node.name`.
pub const SINK_PREFIX: &str = "AudioPlumber_";
/// Node property set to `true` on sinks created by AudioPlumber.
pub const MANAGED_PROP: &str = "audioplumber.managed";
/// Node property holding the user-facing sink name.
pub const NAME_PROP: &str = "audioplumber.name";

/// `node.name` AudioPlumber requests for a sink called `name`.
pub fn node_name_for(name: &str) -> String {
    format!("{}{}", SINK_PREFIX, name)
}

/// One AudioPlumber null-sink currently loaded in PipeWire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSink {
    /// `pulse.module.id` — unique while loaded; the unload target.
    pub module_id: u32,
    /// `object.serial` — the sink index shown by `pactl` / pavucontrol.
    pub serial: u64,
    /// `node.name` (not unique).
    pub node_name: String,
    /// User-facing name: `audioplumber.name`, else `node.name` minus the prefix.
    pub name: String,
    /// Carries the `audioplumber.managed` marker.
    pub marked: bool,
}

impl LiveSink {
    fn matches_name(&self, name: &str) -> bool {
        self.name == name || self.node_name == node_name_for(name)
    }
}

fn prop_u64(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn prop_true(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim() == "true",
        _ => false,
    }
}

/// Extracts the live AudioPlumber sinks from a parsed `pw-dump` document,
/// sorted by sink index (creation order).
pub fn parse_live_sinks(json: &Value) -> Vec<LiveSink> {
    let mut out = Vec::new();
    for item in json.as_array().map(Vec::as_slice).unwrap_or_default() {
        if item.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = item.get("info").and_then(|i| i.get("props")) else {
            continue;
        };
        if props.get("media.class").and_then(Value::as_str) != Some("Audio/Sink") {
            continue;
        }
        let Some(module_id) = prop_u64(props.get("pulse.module.id"))
            .and_then(|id| u32::try_from(id).ok())
        else {
            continue; // not loaded via a pulse module → nothing we could unload
        };
        let Some(node_name) = props.get("node.name").and_then(Value::as_str) else {
            continue;
        };
        let marked = prop_true(props.get(MANAGED_PROP));
        if !marked && !node_name.starts_with(SINK_PREFIX) {
            continue;
        }
        let name = props
            .get(NAME_PROP)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                node_name
                    .strip_prefix(SINK_PREFIX)
                    .unwrap_or(node_name)
                    .to_string()
            });
        let serial = prop_u64(props.get("object.serial"))
            .or_else(|| item.get("id").and_then(Value::as_u64))
            .unwrap_or(0);
        out.push(LiveSink {
            module_id,
            serial,
            node_name: node_name.to_string(),
            name,
            marked,
        });
    }
    out.sort_by_key(|s| (s.serial, s.module_id));
    out
}

/// Live AudioPlumber sinks, or `None` when PipeWire's sink list could not be
/// read (callers must then avoid anything that could create duplicates or
/// unload the wrong module).
pub fn list_live_sinks() -> Option<Vec<LiveSink>> {
    pipewire::pw_dump_json().map(|json| parse_live_sinks(&json))
}

/// For each saved definition, the index of the live sink it refers to.
///
/// A live sink is claimed by at most one definition. Preference: the live sink
/// whose module id equals the saved one (and whose name matches), else the
/// oldest unclaimed live sink with a matching name.
fn associate(saved: &[VirtualSink], live: &[LiveSink]) -> Vec<Option<usize>> {
    let mut claimed: HashSet<usize> = HashSet::new();
    saved
        .iter()
        .map(|v| {
            let exact = live
                .iter()
                .position(|l| l.module_id == v.module_id && l.matches_name(&v.name))
                .filter(|i| !claimed.contains(i));
            let pick = exact.or_else(|| {
                live.iter()
                    .enumerate()
                    .find(|(i, l)| !claimed.contains(i) && l.matches_name(&v.name))
                    .map(|(i, _)| i)
            });
            if let Some(i) = pick {
                claimed.insert(i);
            }
            pick
        })
        .collect()
}

/// Startup reconciliation of the saved definitions against the live sinks.
///
/// Drops repeated names (keeping the first), points each definition at the
/// module id of its live sink (ids change across PipeWire restarts), and
/// returns `(changed, missing)` where `missing` are indices of definitions with
/// no live sink that should be recreated.
pub fn reconcile_saved(saved: &mut Vec<VirtualSink>, live: &[LiveSink]) -> (bool, Vec<usize>) {
    let before = saved.len();
    let mut seen: HashSet<String> = HashSet::new();
    saved.retain(|v| seen.insert(v.name.clone()));
    let mut changed = saved.len() != before;

    let mut missing = Vec::new();
    for (i, assoc) in associate(saved, live).into_iter().enumerate() {
        match assoc {
            Some(li) => {
                if saved[i].module_id != live[li].module_id {
                    saved[i].module_id = live[li].module_id;
                    changed = true;
                }
            }
            None => missing.push(i),
        }
    }
    (changed, missing)
}

/// Duplicate-name policy: creation is refused when the name is already used by
/// a saved definition or a live AudioPlumber sink.
pub fn name_taken(name: &str, saved: &[VirtualSink], live: Option<&[LiveSink]>) -> bool {
    saved.iter().any(|v| v.name == name)
        || live
            .unwrap_or_default()
            .iter()
            .any(|l| l.matches_name(name))
}

/// Unique identity of a left-column card, and what "Delete sink" removes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SinkTarget {
    /// A loaded sink, by `pulse.module.id`.
    Module(u32),
    /// A saved definition with no loaded sink, by name.
    Saved(String),
}

/// How a card finds its ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkState {
    /// Loaded. `duplicate` = another live sink has the same `node.name`, so
    /// port ids (`node.name:port`) cannot tell the two apart.
    Live { node_name: String, duplicate: bool },
    /// Saved but not loaded.
    NotLoaded,
    /// The live sink list is unavailable; fall back to name-based matching.
    Unknown,
}

/// One card in the "Virtual Sinks" column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkRow {
    pub target: SinkTarget,
    /// User-facing name.
    pub name: String,
    /// Display label: `name`, plus `#<sink index>` when names repeat.
    pub label: String,
    pub state: SinkState,
}

/// Builds the left-column rows: saved definitions in their saved order (each
/// followed by any same-named duplicates), then live sinks that no definition
/// refers to (e.g. left over from a previous run), oldest first.
pub fn build_rows(saved: &[VirtualSink], live: Option<&[LiveSink]>) -> Vec<SinkRow> {
    let Some(live) = live else {
        return saved
            .iter()
            .map(|v| SinkRow {
                target: SinkTarget::Module(v.module_id),
                name: v.name.clone(),
                label: v.name.clone(),
                state: SinkState::Unknown,
            })
            .collect();
    };

    let mut node_counts: HashMap<&str, usize> = HashMap::new();
    for l in live {
        *node_counts.entry(l.node_name.as_str()).or_default() += 1;
    }
    let live_row = |l: &LiveSink| SinkRow {
        target: SinkTarget::Module(l.module_id),
        name: l.name.clone(),
        label: l.name.clone(),
        state: SinkState::Live {
            node_name: l.node_name.clone(),
            duplicate: node_counts[l.node_name.as_str()] > 1,
        },
    };

    let assoc = associate(saved, live);
    let mut used: HashSet<usize> = assoc.iter().flatten().copied().collect();
    let mut rows: Vec<(SinkRow, Option<u64>)> = Vec::new();
    for (v, a) in saved.iter().zip(&assoc) {
        match a {
            Some(li) => {
                rows.push((live_row(&live[*li]), Some(live[*li].serial)));
                for (j, l) in live.iter().enumerate() {
                    if !used.contains(&j) && l.node_name == live[*li].node_name {
                        used.insert(j);
                        rows.push((live_row(l), Some(l.serial)));
                    }
                }
            }
            None => rows.push((
                SinkRow {
                    target: SinkTarget::Saved(v.name.clone()),
                    name: v.name.clone(),
                    label: v.name.clone(),
                    state: SinkState::NotLoaded,
                },
                None,
            )),
        }
    }
    for (j, l) in live.iter().enumerate() {
        if !used.contains(&j) {
            rows.push((live_row(l), Some(l.serial)));
        }
    }

    // Disambiguate repeated names with the sink index pavucontrol/pactl show.
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for (r, _) in &rows {
        *name_counts.entry(r.name.clone()).or_default() += 1;
    }
    rows.into_iter()
        .map(|(mut r, serial)| {
            if let (true, Some(s)) = (name_counts[&r.name] > 1, serial) {
                r.label = format!("{} #{}", r.name, s);
            }
            r
        })
        .collect()
}

/// Whether a port on PipeWire node `port_node` belongs to the card `row`.
pub fn row_owns_port(row: &SinkRow, port_node: &str) -> bool {
    match &row.state {
        SinkState::Live { node_name, duplicate } => !duplicate && port_node == node_name,
        SinkState::NotLoaded => false,
        // Legacy fuzzy match, used only when pw-dump is unavailable.
        SinkState::Unknown => {
            let node = node_name_for(&row.name);
            port_node == node
                || port_node.starts_with(&format!("{}.", node))
                || port_node.starts_with(&format!("{}_", node))
        }
    }
}

/// What deleting `target` should do, given the current live sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletePlan {
    /// `pactl unload-module <id>`, then save `remaining`.
    Unload { module_id: u32, remaining: Vec<VirtualSink> },
    /// Nothing loaded to unload; just save `remaining`.
    Forget { remaining: Vec<VirtualSink> },
    /// Cannot verify what the id refers to — do nothing.
    Refuse,
}

/// Plans a delete. A module id is only unloaded when it is verified to be a
/// live AudioPlumber sink right now (pipewire-pulse reuses module ids, so a
/// stale id may name an unrelated module).
pub fn plan_delete(
    target: &SinkTarget,
    saved: &[VirtualSink],
    live: Option<&[LiveSink]>,
) -> DeletePlan {
    match target {
        SinkTarget::Saved(name) => DeletePlan::Forget {
            remaining: saved.iter().filter(|v| &v.name != name).cloned().collect(),
        },
        SinkTarget::Module(id) => {
            let Some(live) = live else {
                return DeletePlan::Refuse;
            };
            let Some(li) = live.iter().position(|l| l.module_id == *id) else {
                return DeletePlan::Refuse; // already gone or not ours
            };
            let assoc = associate(saved, live);
            let remaining = saved
                .iter()
                .zip(&assoc)
                .filter(|(_, a)| **a != Some(li))
                .map(|(v, _)| v.clone())
                .collect();
            DeletePlan::Unload {
                module_id: *id,
                remaining,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vs(name: &str, module_id: u32) -> VirtualSink {
        VirtualSink {
            name: name.to_string(),
            module_id,
        }
    }

    fn live(name: &str, module_id: u32, serial: u64) -> LiveSink {
        LiveSink {
            module_id,
            serial,
            node_name: node_name_for(name),
            name: name.to_string(),
            marked: false,
        }
    }

    /// The state observed on the reporting machine: two pre-marker sinks with
    /// the same node.name, only the newer one in virtual_sinks.json.
    fn reported() -> (Vec<VirtualSink>, Vec<LiveSink>) {
        (
            vec![vs("test", 536870917)],
            vec![live("test", 536870916, 1652), live("test", 536870917, 15813)],
        )
    }

    #[test]
    fn parse_live_sinks_filters_and_keys_by_module() {
        let json: Value = serde_json::from_str(
            r#"[
              {"id": 117, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "AudioPlumber_test",
                "device.description": "test", "pulse.module.id": 536870917,
                "object.serial": 15813}}},
              {"id": 168, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "AudioPlumber_test",
                "pulse.module.id": "536870916", "object.serial": "1652"}}},
              {"id": 200, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "AudioPlumber_Mix",
                "audioplumber.managed": true, "audioplumber.name": "Mix",
                "pulse.module.id": 536870920, "object.serial": 20000}}},
              {"id": 201, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "someone_elses_null",
                "pulse.module.id": 536870921, "object.serial": 20001}}},
              {"id": 202, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "alsa_output.speaker",
                "object.serial": 20002}}},
              {"id": 203, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Stream/Output/Audio", "node.name": "AudioPlumber_x",
                "pulse.module.id": 536870922}}},
              {"id": 5, "type": "PipeWire:Interface:Module", "info": {"props": {}}}
            ]"#,
        )
        .unwrap();
        let sinks = parse_live_sinks(&json);
        assert_eq!(
            sinks,
            vec![
                LiveSink {
                    module_id: 536870916,
                    serial: 1652,
                    node_name: "AudioPlumber_test".into(),
                    name: "test".into(),
                    marked: false,
                },
                LiveSink {
                    module_id: 536870917,
                    serial: 15813,
                    node_name: "AudioPlumber_test".into(),
                    name: "test".into(),
                    marked: false,
                },
                LiveSink {
                    module_id: 536870920,
                    serial: 20000,
                    node_name: "AudioPlumber_Mix".into(),
                    name: "Mix".into(),
                    marked: true,
                },
            ]
        );
    }

    #[test]
    fn parse_live_sinks_accepts_marker_without_prefix_and_string_bool() {
        let json: Value = serde_json::from_str(
            r#"[{"id": 1, "type": "PipeWire:Interface:Node", "info": {"props": {
                "media.class": "Audio/Sink", "node.name": "renamed",
                "audioplumber.managed": "true", "pulse.module.id": 7}}}]"#,
        )
        .unwrap();
        let sinks = parse_live_sinks(&json);
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "renamed");
        assert_eq!(sinks[0].serial, 1, "falls back to the object id");
        assert!(sinks[0].marked);
    }

    #[test]
    fn parse_live_sinks_tolerates_garbage() {
        assert!(parse_live_sinks(&Value::Null).is_empty());
        assert!(parse_live_sinks(&serde_json::json!({"a": 1})).is_empty());
    }

    #[test]
    fn duplicate_names_are_separate_distinguishable_rows() {
        let (saved, live) = reported();
        let rows = build_rows(&saved, Some(&live));
        assert_eq!(rows.len(), 2, "both loaded copies are listed");
        // The tracked copy first (saved order), then its same-named duplicate.
        assert_eq!(rows[0].target, SinkTarget::Module(536870917));
        assert_eq!(rows[0].label, "test #15813");
        assert_eq!(rows[1].target, SinkTarget::Module(536870916));
        assert_eq!(rows[1].label, "test #1652");
        for r in &rows {
            assert_eq!(
                r.state,
                SinkState::Live {
                    node_name: "AudioPlumber_test".into(),
                    duplicate: true
                }
            );
        }
    }

    #[test]
    fn untracked_leftover_sink_is_listed() {
        let live = vec![live("old", 10, 5)];
        let rows = build_rows(&[], Some(&live));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, SinkTarget::Module(10));
        assert_eq!(rows[0].label, "old");
    }

    #[test]
    fn unique_names_have_plain_labels_and_saved_order() {
        let saved = vec![vs("b", 2), vs("a", 1)];
        let live = vec![live("a", 1, 10), live("b", 2, 20)];
        let rows = build_rows(&saved, Some(&live));
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["b", "a"]);
        assert!(rows.iter().all(|r| matches!(
            r.state,
            SinkState::Live { duplicate: false, .. }
        )));
    }

    #[test]
    fn saved_but_not_loaded_is_keyed_by_name() {
        // Module id 5 was reused by pipewire-pulse for an unrelated sink "foo".
        let saved = vec![vs("test", 5)];
        let live = vec![live("foo", 5, 1)];
        let rows = build_rows(&saved, Some(&live));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].target, SinkTarget::Saved("test".into()));
        assert_eq!(rows[0].state, SinkState::NotLoaded);
        assert_eq!(rows[1].target, SinkTarget::Module(5));
        assert_eq!(rows[1].name, "foo");
    }

    #[test]
    fn saved_id_is_healed_by_name() {
        let saved = vec![vs("test", 99)];
        let live = vec![live("test", 7, 1)];
        let rows = build_rows(&saved, Some(&live));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, SinkTarget::Module(7));
    }

    #[test]
    fn unknown_live_state_falls_back_to_saved_list() {
        let rows = build_rows(&[vs("x", 3)], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, SinkTarget::Module(3));
        assert_eq!(rows[0].state, SinkState::Unknown);
    }

    #[test]
    fn port_ownership_is_exact_for_live_sinks() {
        let live = vec![live("test", 1, 1), live("test_2", 2, 2)];
        let rows = build_rows(&[], Some(&live));
        assert!(row_owns_port(&rows[0], "AudioPlumber_test"));
        assert!(!row_owns_port(&rows[0], "AudioPlumber_test_2"));
        assert!(row_owns_port(&rows[1], "AudioPlumber_test_2"));

        let (saved, live) = reported();
        let rows = build_rows(&saved, Some(&live));
        assert!(rows.iter().all(|r| !row_owns_port(r, "AudioPlumber_test")));

        let fallback = build_rows(&[vs("test", 1)], None);
        assert!(row_owns_port(&fallback[0], "AudioPlumber_test.2"));
        let missing = build_rows(&[vs("test", 1)], Some(&[]));
        assert!(!row_owns_port(&missing[0], "AudioPlumber_test"));
    }

    #[test]
    fn reconcile_heals_ids_reports_missing_and_dedupes() {
        let mut saved = vec![vs("a", 1), vs("b", 2), vs("a", 3)];
        let live = vec![live("a", 10, 1)];
        let (changed, missing) = reconcile_saved(&mut saved, &live);
        assert!(changed);
        assert_eq!(saved, vec![vs("a", 10), vs("b", 2)]);
        assert_eq!(missing, vec![1]);

        let mut saved = vec![vs("a", 10)];
        assert_eq!(reconcile_saved(&mut saved, &live), (false, vec![]));
    }

    #[test]
    fn reconcile_does_not_recreate_reported_duplicate() {
        let (mut saved, live) = reported();
        let (changed, missing) = reconcile_saved(&mut saved, &live);
        assert!(!changed);
        assert!(missing.is_empty());
        assert_eq!(saved, vec![vs("test", 536870917)]);
    }

    #[test]
    fn name_taken_checks_saved_and_live() {
        let (saved, live) = reported();
        assert!(name_taken("test", &saved, Some(&live)));
        assert!(name_taken("test", &[], Some(&live)));
        assert!(name_taken("test", &saved, None));
        assert!(!name_taken("test2", &saved, Some(&live)));
        assert!(!name_taken("Test", &saved, Some(&live)));
    }

    #[test]
    fn deleting_leftover_copy_keeps_tracked_definition() {
        let (saved, live) = reported();
        assert_eq!(
            plan_delete(&SinkTarget::Module(536870916), &saved, Some(&live)),
            DeletePlan::Unload {
                module_id: 536870916,
                remaining: saved.clone()
            }
        );
    }

    #[test]
    fn deleting_tracked_copy_forgets_definition() {
        let (saved, live) = reported();
        assert_eq!(
            plan_delete(&SinkTarget::Module(536870917), &saved, Some(&live)),
            DeletePlan::Unload {
                module_id: 536870917,
                remaining: vec![]
            }
        );
    }

    #[test]
    fn deleting_healed_sink_forgets_stale_definition() {
        let saved = vec![vs("test", 99), vs("other", 50)];
        let live = vec![live("test", 7, 1)];
        assert_eq!(
            plan_delete(&SinkTarget::Module(7), &saved, Some(&live)),
            DeletePlan::Unload {
                module_id: 7,
                remaining: vec![vs("other", 50)]
            }
        );
    }

    /// Talks to the real PipeWire server: creates a uniquely named sink, checks
    /// it is discovered with the marker, then deletes it through the same plan
    /// the UI uses. Run with `cargo test -- --ignored live_`.
    #[test]
    #[ignore = "creates and removes a real PipeWire sink"]
    fn live_create_discover_delete_roundtrip() {
        let name = format!("ZZ_vsinks_selftest_{}", std::process::id());
        let id = pipewire::create_virtual_sink(&name).expect("create");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let live = list_live_sinks().expect("pw-dump");
        let found: Vec<&LiveSink> = live.iter().filter(|l| l.module_id == id).collect();
        let ok = found.len() == 1
            && found[0].marked
            && found[0].name == name
            && found[0].node_name == node_name_for(&name)
            && name_taken(&name, &[], Some(&live));
        let saved = vec![VirtualSink {
            name: name.clone(),
            module_id: id,
        }];
        let plan = plan_delete(&SinkTarget::Module(id), &saved, Some(&live));
        // Always clean up, even if the assertions below fail.
        pipewire::delete_virtual_sink(id).expect("unload");
        assert!(ok, "sink not discovered correctly: {:?}", found);
        assert_eq!(
            plan,
            DeletePlan::Unload {
                module_id: id,
                remaining: vec![]
            }
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let after = list_live_sinks().expect("pw-dump");
        assert!(after.iter().all(|l| l.module_id != id && l.name != name));
    }

    #[test]
    fn delete_never_unloads_unverified_ids() {
        let saved = vec![vs("test", 5)];
        assert_eq!(
            plan_delete(&SinkTarget::Module(5), &saved, None),
            DeletePlan::Refuse
        );
        assert_eq!(
            plan_delete(&SinkTarget::Module(5), &saved, Some(&[])),
            DeletePlan::Refuse
        );
        assert_eq!(
            plan_delete(&SinkTarget::Saved("test".into()), &saved, Some(&[])),
            DeletePlan::Forget { remaining: vec![] }
        );
    }
}
