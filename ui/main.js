// AudioPlumber — main.js
// Simplified patchbay: virtual sinks on the left, real outputs on the right

// ─── Tauri IPC ───────────────────────────────────────────────────────────────
// Tauri 2 exposes invoke via window.__TAURI__.core
function invoke(cmd, args = {}) {
  return window.__TAURI__.core.invoke(cmd, args);
}

// ─── Constants ────────────────────────────────────────────────────────────────
const VIRTUAL_SINKS_KEY = 'audioplumber_virtual_sinks';

// ─── State ────────────────────────────────────────────────────────────────────
let allOutputs = [];  // [{ node, port, id }] — all PipeWire output ports (pw-link -o)
let allInputs  = [];  // [{ node, port, id }] — all PipeWire input ports (pw-link -i)
let links      = [];  // [{ from, to }]

let virtualSinks    = [];       // [{ name, moduleId }]
let nodeDescriptions = new Map(); // node.name → human-readable description from pw-dump

// A pending connection has exactly one endpoint selected, waiting for a
// complementary click on the other side to complete it. The endpoint may be on
// either side ('output' = left/virtual-sink monitor, 'input' = right/real output).
let pending          = null;  // null | { side: 'output'|'input', portInfo, dotEl }
let danglingPath     = null;  // the live SVG <path> that follows the cursor while pending
let pendingMouse     = null;  // { x, y } last cursor position (client coords) while pending
let autoRefreshTimer = null;
let simpleMode       = true;  // toggle between simple (bundled) and advanced (per-port) modes

// ─── Virtual sinks persistence ────────────────────────────────────────────────
function loadVirtualSinks() {
  try {
    const raw = localStorage.getItem(VIRTUAL_SINKS_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      virtualSinks = Array.isArray(parsed) ? parsed : [];
    }
  } catch (_) { /* ignore corrupt data */ }
}

function saveVirtualSinks() {
  localStorage.setItem(VIRTUAL_SINKS_KEY, JSON.stringify(virtualSinks));
}

// Does a virtual sink's PipeWire null-sink currently exist? We match the node
// name the same (fuzzy) way renderLeftColumn does, since PipeWire may append a
// suffix like `.2` / `_1` to the requested sink_name.
function virtualSinkPortsPresent(name, outputs) {
  const node = `AudioPlumber_${name}`;
  return outputs.some(p =>
    p.node === node || p.node.startsWith(node + '.') || p.node.startsWith(node + '_')
  );
}

// Virtual-sink *definitions* persist in localStorage, but their underlying
// PipeWire null-sink modules do NOT survive a logout / reboot / PipeWire
// restart — the stored moduleId becomes stale and the monitor ports vanish.
// Without those ports the sink card is stuck on "Waiting for PipeWire…" and its
// saved connections can never be restored. So on startup we recreate any
// persisted sink whose ports are not currently present, and refresh its
// moduleId. (Sinks whose ports already exist are left untouched.)
async function ensureVirtualSinks() {
  if (virtualSinks.length === 0) return;

  let outputs;
  try {
    outputs = await invoke('get_outputs');
  } catch (_) {
    return;  // if discovery fails, skip recreation rather than risk duplicates
  }

  let changed = false;
  for (const vs of virtualSinks) {
    if (virtualSinkPortsPresent(vs.name, outputs)) continue;
    try {
      vs.moduleId = await invoke('create_virtual_sink', { name: vs.name });
      changed = true;
    } catch (err) {
      console.warn(`Failed to recreate virtual sink "${vs.name}":`, err);
    }
  }

  if (changed) {
    saveVirtualSinks();
    // Give PipeWire a moment to register the recreated sinks' ports before the
    // first refresh/reconnect runs.
    await new Promise(resolve => setTimeout(resolve, 500));
  }
}

// ─── DOM helpers ──────────────────────────────────────────────────────────────
const outputsList = document.getElementById('outputs-list');
const inputsList  = document.getElementById('inputs-list');
const cableSvg    = document.getElementById('cable-svg');
const btnRefresh  = document.getElementById('btn-refresh');
const btnNewVsink = document.getElementById('btn-new-vsink');
const btnModeToggle = document.getElementById('btn-mode-toggle');
const debugPanel  = document.getElementById('debug-panel');

const errorBanner      = document.getElementById('error-banner');
const errorBannerMsg   = document.getElementById('error-banner-msg');
const errorBannerClose = document.getElementById('error-banner-close');

// Modal — virtual sink
const modalVsink       = document.getElementById('modal-vsink');
const vsinkNameInput   = document.getElementById('vsink-name-input');
const modalVsinkCancel = document.getElementById('modal-vsink-cancel');
const modalVsinkConfirm = document.getElementById('modal-vsink-confirm');

// ─── Error banner ─────────────────────────────────────────────────────────────
function showErrorBanner(msg) {
  errorBannerMsg.textContent = msg;
  errorBanner.style.display = 'flex';
}

function hideErrorBanner() {
  errorBanner.style.display = 'none';
  errorBannerMsg.textContent = '';
}

errorBannerClose.addEventListener('click', hideErrorBanner);

// Header status display has been removed; setStatus is intentionally a no-op.
// Call sites are preserved so selection/connect/disconnect state logic is untouched.
function setStatus(_msg, _isError = false) {}

// ─── Cable colour palette ─────────────────────────────────────────────────────
const CABLE_COLORS = [
  '#53d8fb', '#ff6b9d', '#a8ff78', '#ffde59',
  '#c77dff', '#ff9f43', '#48dbfb', '#ffeaa7',
];
function cableColor(index) {
  return CABLE_COLORS[index % CABLE_COLORS.length];
}

// ─── Group ports by node ──────────────────────────────────────────────────────
function groupByNode(ports) {
  const map = new Map();
  for (const p of ports) {
    if (!map.has(p.node)) map.set(p.node, []);
    map.get(p.node).push(p);
  }
  return map;
}

// ─── Node name from port ID ───────────────────────────────────────────────────
function nodeNameFromPortId(portId) {
  const colonIdx = portId.indexOf(':');
  return colonIdx >= 0 ? portId.slice(0, colonIdx) : portId;
}

// ─── Mode toggle ──────────────────────────────────────────────────────────────
function updateModeToggleLabel() {
  if (btnModeToggle) {
    btnModeToggle.textContent = simpleMode ? '\u2699 Advanced' : '\u25ce Simple';
    btnModeToggle.title = simpleMode
      ? 'Switch to Advanced mode (individual ports)'
      : 'Switch to Simple mode (bundled stereo)';
  }
}

if (btnModeToggle) {
  btnModeToggle.addEventListener('click', () => {
    simpleMode = !simpleMode;
    clearSelectedOutput();
    updateModeToggleLabel();
    renderAll();
    requestAnimationFrame(() => requestAnimationFrame(drawCables));
  });
}

// ─── Port dot listeners (click + right-click to disconnect) ──────────────────
function addPortDotListeners(dot, portInfo, side) {
  dot.addEventListener('click', (ev) => {
    ev.stopPropagation();
    onPortClick(portInfo, side, dot);
  });

  dot.addEventListener('contextmenu', async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();

    // Collect port IDs to disconnect (simple mode: all ports in the node; advanced: just this port)
    let portIds;
    if (dot.dataset.portIds) {
      portIds = dot.dataset.portIds.split(',');
    } else {
      portIds = [dot.dataset.portId];
    }

    // Find all links involving any of these port IDs
    const toDisconnect = links.filter(l =>
      portIds.includes(l.from) || portIds.includes(l.to)
    );

    if (toDisconnect.length === 0) {
      setStatus('No connections on this port.', true);
      return;
    }

    setStatus(`Removing ${toDisconnect.length} connection(s)…`);
    const errors = [];
    for (const link of toDisconnect) {
      try {
        await invoke('disconnect', { from: link.from, to: link.to });
      } catch (err) {
        errors.push(err);
      }
    }

    if (errors.length > 0) {
      showErrorBanner(`Disconnect failed: ${errors.join('; ')}`);
    } else {
      setStatus(`Removed ${toDisconnect.length} connection(s)`);
    }

    await refresh();
    await saveCurrentLinks();
  });
}

// ─── Render left column: virtual sink cards ───────────────────────────────────
// Virtual sink monitor ports come from get_outputs() (pw-link -o)
// Node names are AudioPlumber_{name}, but we do a fuzzy lookup in case PipeWire
// appends a suffix like AudioPlumber_Foo.2 or AudioPlumber_Foo_1.
function renderLeftColumn() {
  outputsList.innerHTML = '';

  if (virtualSinks.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = 'No virtual sinks yet. Click "\uFF0B New Virtual Sink" to create one.';
    outputsList.appendChild(empty);
    return;
  }

  for (const vs of virtualSinks) {
    const nodeName = `AudioPlumber_${vs.name}`;

    // Fuzzy lookup: exact match OR PipeWire-appended suffix variants
    const ports = allOutputs.filter(p =>
      p.node === nodeName ||
      p.node.startsWith(nodeName + '.') ||
      p.node.startsWith(nodeName + '_')
    );

    const card = document.createElement('div');
    card.className = 'node-card node-card-virtual';

    const nameEl = document.createElement('div');
    nameEl.className = 'node-name';
    nameEl.textContent = vs.name;
    nameEl.title = nodeName;
    card.appendChild(nameEl);

    if (ports.length === 0) {
      const pending = document.createElement('div');
      pending.className = 'port-row';
      pending.style.padding = '4px 14px';
      pending.style.fontSize = '11px';
      pending.style.color = 'var(--text-dim)';
      pending.style.fontStyle = 'italic';
      pending.textContent = 'Waiting for PipeWire\u2026';
      card.appendChild(pending);
    } else if (simpleMode) {
      // Simple mode: one bundled dot for all ports of this node
      // Use the actual node name from the first matched port
      const actualNodeName = ports[0].node;
      const portIds = ports.map(p => p.id);

      const row = document.createElement('div');
      row.className = 'port-row';

      const dot = document.createElement('div');
      dot.className = 'port-dot';
      dot.dataset.portIds = portIds.join(',');
      dot.dataset.node = actualNodeName;
      dot.dataset.side = 'output';
      dot.title = portIds.join(', ');

      const label = document.createElement('div');
      label.className = 'port-name';
      label.textContent = vs.name;
      label.title = vs.name;

      row.appendChild(dot);
      row.appendChild(label);
      card.appendChild(row);

      addPortDotListeners(dot, { bundled: true, portIds }, 'output');
    } else {
      // Advanced mode: individual port dots
      for (const p of ports) {
        const row = document.createElement('div');
        row.className = 'port-row';

        const dot = document.createElement('div');
        dot.className = 'port-dot';
        dot.dataset.portId = p.id;
        dot.dataset.node = p.node;
        dot.dataset.side = 'output';
        dot.title = p.id;

        const label = document.createElement('div');
        label.className = 'port-name';
        label.textContent = p.port;
        label.title = p.port;

        row.appendChild(dot);
        row.appendChild(label);
        card.appendChild(row);

        addPortDotListeners(dot, { bundled: false, portId: p.id }, 'output');
      }
    }

    // Delete button — hover-reveal red button
    const deleteBtn = document.createElement('button');
    deleteBtn.className = 'btn-delete-sink';
    deleteBtn.textContent = 'Delete sink';
    deleteBtn.addEventListener('click', (ev) => {
      ev.stopPropagation();
      doDeleteVirtualSink(vs);
    });
    card.appendChild(deleteBtn);

    outputsList.appendChild(card);
  }
}

// ─── Render right column: real audio outputs ──────────────────────────────────
// Real outputs come from get_inputs() (pw-link -i) — PipeWire sink playback ports.
// Filters out virtual sinks, MIDI nodes, internal capture devices, and any node
// that doesn't have at least one stereo audio port.
const STEREO_PORT_NAMES = new Set(['playback_FL', 'playback_FR', 'capture_FL', 'capture_FR']);

function renderRightColumn() {
  inputsList.innerHTML = '';

  const portsByNode = groupByNode(allInputs);

  // Filter to real, stereo playback/capture sinks only
  const filteredEntries = [];
  for (const [nodeName, ports] of portsByNode) {
    if (nodeName.startsWith('AudioPlumber_')) continue;
    if (nodeName.toLowerCase().includes('midi')) continue;
    if (/^bluez_capture_internal/i.test(nodeName)) continue;
    // Must have at least one recognised stereo audio port
    if (!ports.some(p => STEREO_PORT_NAMES.has(p.port))) continue;
    filteredEntries.push([nodeName, ports]);
  }

  if (filteredEntries.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = 'No audio outputs found. Is PipeWire running?';
    inputsList.appendChild(empty);
    return;
  }

  for (const [nodeName, ports] of filteredEntries) {
    const displayName = nodeDescriptions.get(nodeName) || nodeName;

    const card = document.createElement('div');
    card.className = 'node-card';

    const nameEl = document.createElement('div');
    nameEl.className = 'node-name';
    nameEl.textContent = displayName;
    nameEl.title = nodeName;  // raw PipeWire name as tooltip
    card.appendChild(nameEl);

    if (simpleMode) {
      // Simple mode: one bundled dot for all ports of this node
      const portIds = ports.map(p => p.id);

      const row = document.createElement('div');
      row.className = 'port-row';

      const dot = document.createElement('div');
      dot.className = 'port-dot';
      dot.dataset.portIds = portIds.join(',');
      dot.dataset.node = nodeName;
      dot.dataset.side = 'input';
      dot.title = portIds.join(', ');

      const label = document.createElement('div');
      label.className = 'port-name';
      label.textContent = displayName;
      label.title = displayName;

      row.appendChild(dot);
      row.appendChild(label);
      card.appendChild(row);

      addPortDotListeners(dot, { bundled: true, portIds }, 'input');
    } else {
      // Advanced mode: individual port dots
      for (const p of ports) {
        const row = document.createElement('div');
        row.className = 'port-row';

        const dot = document.createElement('div');
        dot.className = 'port-dot';
        dot.dataset.portId = p.id;
        dot.dataset.node = p.node;
        dot.dataset.side = 'input';
        dot.title = p.id;

        const label = document.createElement('div');
        label.className = 'port-name';
        label.textContent = p.port;
        label.title = p.port;

        row.appendChild(dot);
        row.appendChild(label);
        card.appendChild(row);

        addPortDotListeners(dot, { bundled: false, portId: p.id }, 'input');
      }
    }

    inputsList.appendChild(card);
  }
}

function renderAll() {
  renderLeftColumn();
  renderRightColumn();
  reapplyPendingSelection();
}

// ─── Port click logic ─────────────────────────────────────────────────────────
// Connections can be initiated from EITHER side. The first clicked endpoint
// becomes "pending"; clicking a complementary endpoint on the other side
// completes the connection (output->input, identical regardless of start order).
function onPortClick(portInfo, side, dotEl) {
  // No pending selection yet -> begin a pending connection from this endpoint.
  if (!pending) {
    selectEndpoint(portInfo, side, dotEl);
    return;
  }

  // Clicking the already-selected endpoint again -> cancel.
  if (pending.dotEl === dotEl) {
    clearSelectedOutput();
    return;
  }

  // Clicking another endpoint on the SAME side -> move the selection there.
  if (pending.side === side) {
    selectEndpoint(portInfo, side, dotEl);
    return;
  }

  // Complementary side -> complete the connection.
  const outputInfo = side === 'output' ? portInfo : pending.portInfo;
  const inputInfo  = side === 'input'  ? portInfo : pending.portInfo;
  completeConnection(outputInfo, inputInfo);
}

function selectEndpoint(portInfo, side, dotEl) {
  clearSelectedOutput();
  pending = { side, portInfo, dotEl };
  dotEl.classList.add('selected');
  const label = portInfo.bundled ? portInfo.portIds.join(', ') : portInfo.portId;
  setStatus(`Selected: ${label}`);
  startDanglingCable();
}

// Perform the actual connect. `outputInfo` is always the virtual-sink/monitor
// (left) endpoint and `inputInfo` the real output (right) endpoint, so the
// pw-link call is identical no matter which side initiated.
function completeConnection(outputInfo, inputInfo) {
    if (outputInfo.bundled && inputInfo.bundled) {
      // Simple mode: pair up ports by sorted order
      const fromPorts = outputInfo.portIds.slice().sort();
      const toPorts   = inputInfo.portIds.slice().sort();
      const pairs = fromPorts
        .map((f, i) => ({ from: f, to: toPorts[i] }))
        .filter(p => p.from && p.to);

      clearSelectedOutput();
      setStatus('Connecting\u2026');

      Promise.all(pairs.map(p => invoke('connect', { from: p.from, to: p.to })))
        .then(() => {
          setStatus(`Connected ${pairs.length} port(s)`);
          return refresh();
        })
        .then(() => saveCurrentLinks())
        .catch((err) => {
          showErrorBanner(`Connect failed: ${err}`);
          setStatus(`Connect failed: ${err}`, true);
        });
    } else if (!outputInfo.bundled && !inputInfo.bundled) {
      // Advanced mode: single port connect
      const from = outputInfo.portId;
      const to   = inputInfo.portId;
      clearSelectedOutput();
      setStatus('Connecting\u2026');
      invoke('connect', { from, to })
        .then(() => {
          setStatus(`Connected: ${from} \u2192 ${to}`);
          return refresh();
        })
        .then(() => saveCurrentLinks())
        .catch((err) => {
          showErrorBanner(`Connect failed: ${err}`);
          setStatus(`Connect failed: ${err}`, true);
        });
    } else {
      // Mixed mode (shouldn't normally happen, but handle gracefully)
      clearSelectedOutput();
      setStatus('Mode mismatch — please click a matching port type.', true);
    }
}

// Cancels any pending connection and clears all selection / dangling-cable
// state. (Name retained: called from refresh, mode toggle, cable click/disconnect,
// empty-canvas click, and Escape.)
function clearSelectedOutput() {
  pending = null;
  document.querySelectorAll('.port-dot.selected').forEach(el => el.classList.remove('selected'));
  removeDanglingCable();
}

// After a re-render (refresh rebuilds the port DOM), re-resolve the pending
// endpoint's dot element and re-apply the selection highlight, so the dangling
// cable anchors to the live element instead of a detached one. If the endpoint
// no longer exists (e.g. its virtual sink was deleted), cancel the pending state.
function reapplyPendingSelection() {
  if (!pending) return;
  const { side, portInfo } = pending;
  const dot = portInfo.bundled
    ? document.querySelector(`.port-dot[data-port-ids="${CSS.escape(portInfo.portIds.join(','))}"][data-side="${side}"]`)
    : document.querySelector(`.port-dot[data-port-id="${CSS.escape(portInfo.portId)}"][data-side="${side}"]`);
  if (dot) {
    pending.dotEl = dot;
    dot.classList.add('selected');
  } else {
    clearSelectedOutput();
  }
}

// ─── Connection persistence helpers ──────────────────────────────────────────
async function saveCurrentLinks() {
  try {
    await invoke('save_connections', { connections: links });
  } catch (err) {
    console.warn('Failed to save connections:', err);
  }
}

// ─── Cable drawing ────────────────────────────────────────────────────────────
function getPortCenter(portId) {
  const dot = document.querySelector(`.port-dot[data-port-id="${CSS.escape(portId)}"]`);
  if (!dot) return null;
  const rect    = dot.getBoundingClientRect();
  const svgRect = cableSvg.getBoundingClientRect();
  return {
    x: rect.left + rect.width  / 2 - svgRect.left,
    y: rect.top  + rect.height / 2 - svgRect.top,
  };
}

function getNodePortCenter(nodeName, side) {
  const dot = document.querySelector(`.port-dot[data-node="${CSS.escape(nodeName)}"][data-side="${side}"]`);
  if (!dot) return null;
  const rect    = dot.getBoundingClientRect();
  const svgRect = cableSvg.getBoundingClientRect();
  return {
    x: rect.left + rect.width  / 2 - svgRect.left,
    y: rect.top  + rect.height / 2 - svgRect.top,
  };
}

// Shared bezier shape helper — the real cables and the dangling cable use the
// exact same curve so the tentative cable reads like a real patch cable.
function cablePathD(from, to) {
  const dx = Math.abs(to.x - from.x) * 0.5;
  return `M ${from.x} ${from.y} C ${from.x + dx} ${from.y}, ${to.x - dx} ${to.y}, ${to.x} ${to.y}`;
}

function drawCable(from, to, color, cableData) {
  if (!from || !to) return;
  const d = cablePathD(from, to);

  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('class', 'cable');
  path.setAttribute('d', d);
  path.setAttribute('stroke', color);
  Object.assign(path.dataset, cableData);
  path.style.pointerEvents = 'stroke';

  path.addEventListener('click', (e) => {
    e.stopPropagation();
    if (cableData.fromNode && cableData.toNode) {
      onNodeCableClick(cableData.fromNode, cableData.toNode, path);
    } else {
      onCableClick(cableData.from, cableData.to, path);
    }
  });

  cableSvg.appendChild(path);
}

function drawCables() {
  cableSvg.querySelectorAll('path.cable').forEach(p => p.remove());

  if (simpleMode) {
    // Simple mode: one cable per unique (fromNode, toNode) pair
    const nodePairs = new Map(); // "fromNode|toNode" → index
    for (const link of links) {
      const fromNode = nodeNameFromPortId(link.from);
      const toNode   = nodeNameFromPortId(link.to);
      const key = `${fromNode}|${toNode}`;
      if (!nodePairs.has(key)) {
        nodePairs.set(key, nodePairs.size);
      }
    }

    let colorIdx = 0;
    for (const [key, idx] of nodePairs) {
      const [fromNode, toNode] = key.split('|');
      const from = getNodePortCenter(fromNode, 'output');
      const to   = getNodePortCenter(toNode,   'input');
      const color = cableColor(colorIdx++);
      drawCable(from, to, color, { fromNode, toNode });
    }

    // Mark connected bundled dots
    document.querySelectorAll('.port-dot.connected').forEach(el => el.classList.remove('connected'));
    const connectedNodes = { output: new Set(), input: new Set() };
    for (const link of links) {
      connectedNodes.output.add(nodeNameFromPortId(link.from));
      connectedNodes.input.add(nodeNameFromPortId(link.to));
    }
    document.querySelectorAll('.port-dot[data-node][data-side="output"]').forEach(dot => {
      if (connectedNodes.output.has(dot.dataset.node)) dot.classList.add('connected');
    });
    document.querySelectorAll('.port-dot[data-node][data-side="input"]').forEach(dot => {
      if (connectedNodes.input.has(dot.dataset.node)) dot.classList.add('connected');
    });
  } else {
    // Advanced mode: one cable per link
    links.forEach((link, index) => {
      const from = getPortCenter(link.from);
      const to   = getPortCenter(link.to);
      const color = cableColor(index);
      drawCable(from, to, color, { from: link.from, to: link.to });
    });

    // Mark connected port dots
    document.querySelectorAll('.port-dot.connected').forEach(el => el.classList.remove('connected'));
    const connectedIds = new Set(links.flatMap(l => [l.from, l.to]));
    connectedIds.forEach(id => {
      const dot = document.querySelector(`.port-dot[data-port-id="${CSS.escape(id)}"]`);
      if (dot) dot.classList.add('connected');
    });
  }

  // Keep the tentative cable in sync whenever cables are redrawn (refresh,
  // scroll, resize) so it tracks the moved anchor under the current cursor.
  if (pending) renderDanglingCable();
}

// ─── Dangling "rubber-band" cable (follows cursor while a connection is pending) ─
// Centre of a port dot, in the SVG's coordinate space (matches getPortCenter).
function dotCenter(dotEl) {
  const rect    = dotEl.getBoundingClientRect();
  const svgRect = cableSvg.getBoundingClientRect();
  return {
    x: rect.left + rect.width  / 2 - svgRect.left,
    y: rect.top  + rect.height / 2 - svgRect.top,
  };
}

function startDanglingCable() {
  removeDanglingCable();
  if (!pending) return;
  danglingPath = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  danglingPath.setAttribute('class', 'cable-dangling');
  cableSvg.appendChild(danglingPath);
  // Draw a zero-length stub at the anchor until the first mouse move.
  const anchor = dotCenter(pending.dotEl);
  danglingPath.setAttribute('d', cablePathD(anchor, anchor));
  // Listener is attached ONLY while a connection is pending, and removed on
  // cancel/complete — no permanent high-frequency listener is leaked.
  document.addEventListener('mousemove', onMouseMoveDangling);
}

function onMouseMoveDangling(ev) {
  if (!pending) return;  // cheap early-return guard
  pendingMouse = { x: ev.clientX, y: ev.clientY };
  renderDanglingCable();
}

function renderDanglingCable() {
  if (!danglingPath || !pending) return;
  const anchor  = dotCenter(pending.dotEl);
  let free = anchor;
  if (pendingMouse) {
    const svgRect = cableSvg.getBoundingClientRect();
    free = { x: pendingMouse.x - svgRect.left, y: pendingMouse.y - svgRect.top };
  }
  // Curve from whichever side initiated: an output anchor leaves rightward
  // toward the cursor; an input anchor is approached from the left.
  const ends = pending.side === 'output'
    ? { from: anchor, to: free }
    : { from: free,   to: anchor };
  danglingPath.setAttribute('d', cablePathD(ends.from, ends.to));
}

function removeDanglingCable() {
  document.removeEventListener('mousemove', onMouseMoveDangling);
  if (danglingPath) { danglingPath.remove(); danglingPath = null; }
  pendingMouse = null;
}

// ─── Cable click (disconnect) ─────────────────────────────────────────────────
function onCableClick(from, to, pathEl) {
  clearSelectedOutput();
  setStatus(`Disconnecting: ${from} \u2192 ${to}\u2026`);
  pathEl.style.opacity = '0.3';
  invoke('disconnect', { from, to })
    .then(() => {
      setStatus(`Disconnected: ${from} \u2192 ${to}`);
      return refresh();
    })
    .then(() => saveCurrentLinks())
    .catch((err) => {
      showErrorBanner(`Disconnect failed: ${err}`);
      setStatus(`Disconnect failed: ${err}`, true);
      pathEl.style.opacity = '';
    });
}

// ─── Node-level cable click (simple mode — disconnect all in pair) ────────────
function onNodeCableClick(fromNode, toNode, pathEl) {
  clearSelectedOutput();
  const pairsToDisconnect = links.filter(
    l => nodeNameFromPortId(l.from) === fromNode && nodeNameFromPortId(l.to) === toNode
  );
  if (pairsToDisconnect.length === 0) return;

  setStatus(`Disconnecting: ${fromNode} \u2192 ${toNode}\u2026`);
  pathEl.style.opacity = '0.3';

  Promise.all(pairsToDisconnect.map(l => invoke('disconnect', { from: l.from, to: l.to })))
    .then(() => {
      setStatus(`Disconnected: ${fromNode} \u2192 ${toNode}`);
      return refresh();
    })
    .then(() => saveCurrentLinks())
    .catch((err) => {
      showErrorBanner(`Disconnect failed: ${err}`);
      setStatus(`Disconnect failed: ${err}`, true);
      pathEl.style.opacity = '';
    });
}

// ─── Virtual Sink Modal ───────────────────────────────────────────────────────
function openVsinkModal() {
  vsinkNameInput.value = '';
  modalVsink.style.display = 'flex';
  setTimeout(() => vsinkNameInput.focus(), 50);
}

function closeVsinkModal() {
  modalVsink.style.display = 'none';
}

modalVsinkCancel.addEventListener('click', closeVsinkModal);
modalVsink.addEventListener('click', (e) => {
  if (e.target === modalVsink) closeVsinkModal();
});

async function doCreateVirtualSink() {
  const raw = vsinkNameInput.value.trim();
  const name = raw.replace(/['"\\]/g, '').replace(/\s+/g, '_');
  if (!name) {
    vsinkNameInput.focus();
    return;
  }

  closeVsinkModal();
  setStatus(`Creating virtual sink "${name}"\u2026`);

  try {
    const moduleId = await invoke('create_virtual_sink', { name });
    const vs = { name, moduleId };
    virtualSinks.push(vs);
    saveVirtualSinks();

    setStatus(`Created virtual sink "${name}" (module ${moduleId}) — waiting for PipeWire\u2026`);

    // Wait for PipeWire to register the new ports before refreshing
    await new Promise(resolve => setTimeout(resolve, 500));
    await refresh();
  } catch (err) {
    setStatus(`Failed to create virtual sink: ${err}`, true);
    showErrorBanner(`Failed to create virtual sink "${name}": ${err}`);
  }
}

modalVsinkConfirm.addEventListener('click', doCreateVirtualSink);
vsinkNameInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter')  doCreateVirtualSink();
  if (e.key === 'Escape') closeVsinkModal();
});

btnNewVsink.addEventListener('click', openVsinkModal);

// ─── Virtual sink deletion ────────────────────────────────────────────────────
async function doDeleteVirtualSink(vs) {
  setStatus(`Deleting virtual sink "${vs.name}"\u2026`);
  try {
    await invoke('delete_virtual_sink', { moduleId: vs.moduleId });

    virtualSinks = virtualSinks.filter(v => v.moduleId !== vs.moduleId);
    saveVirtualSinks();

    setStatus(`Deleted virtual sink "${vs.name}"`);
    await refresh();
    await saveCurrentLinks();
  } catch (err) {
    setStatus(`Failed to delete virtual sink: ${err}`, true);
  }
}

// ─── Auto-reconnect ───────────────────────────────────────────────────────────
// Re-establishes saved connections whose devices/ports are present. The backend
// skips absent endpoints silently and only re-links what currently exists, so
// calling this on every refresh lets a saved link reconnect automatically when
// its device reappears.
//
// It is deliberately decoupled from the UI-populate path (see refresh): it is
// fire-and-forget and must NEVER block, delay, or abort rendering. A single
// in-flight guard stops slow restores from piling up across the 3s poll.
let restoreInFlight = false;
async function reconnectSavedConnections({ surfaceErrors = false } = {}) {
  if (restoreInFlight) return;
  restoreInFlight = true;
  try {
    const errors = await invoke('restore_connections');
    // Only genuine failures (both endpoints present, link still failed) come
    // back here; absent-device cases are skipped quietly by the backend.
    if (surfaceErrors && Array.isArray(errors) && errors.length > 0) {
      showErrorBanner(`Some saved connections failed to restore: ${errors.join('; ')}`);
    }
  } catch (_) {
    // Non-fatal: a restore failure must never affect the UI.
  } finally {
    restoreInFlight = false;
  }
}

// ─── Refresh ──────────────────────────────────────────────────────────────────
async function refresh() {
  try {
    let nodeNamesResult;
    [allOutputs, allInputs, links, nodeNamesResult] = await Promise.all([
      invoke('get_outputs'),
      invoke('get_inputs'),
      invoke('get_links'),
      invoke('get_node_names'),
    ]);

    // nodeNamesResult is a plain object; convert to Map
    nodeDescriptions = new Map(Object.entries(nodeNamesResult));

    renderAll();

    // Cables must be drawn after DOM update
    requestAnimationFrame(() => {
      requestAnimationFrame(drawCables);
    });

    setStatus(`Refreshed — ${allOutputs.length} outputs, ${allInputs.length} inputs, ${links.length} links`);
  } catch (err) {
    setStatus(`Refresh error: ${err}`, true);
  }

  // Kick off auto-reconnect AFTER the UI has been populated and rendered, and
  // without awaiting it — a slow, hanging, or failing restore can therefore
  // never delay or block the patchbay from showing. Newly re-established links
  // appear on the next refresh cycle.
  reconnectSavedConnections();
}

// ─── Auto-refresh ─────────────────────────────────────────────────────────────
function startAutoRefresh() {
  if (autoRefreshTimer) clearInterval(autoRefreshTimer);
  autoRefreshTimer = setInterval(refresh, 3000);
}

// ─── Global event wiring ──────────────────────────────────────────────────────
btnRefresh.addEventListener('click', () => {
  clearSelectedOutput();
  refresh();
});

window.addEventListener('resize', drawCables);

document.addEventListener('click', (e) => {
  if (!e.target.closest('.port-dot') && !e.target.closest('.cable')) {
    clearSelectedOutput();
  }
});

// ESC closes any open modal; D toggles debug panel
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    closeVsinkModal();
    // Cancel any pending connection and remove the dangling cable.
    clearSelectedOutput();
  }
  if (e.key === 'd' || e.key === 'D') {
    // Don't trigger when typing in an input
    if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
    if (debugPanel) {
      const isVisible = debugPanel.style.display !== 'none';
      if (isVisible) {
        debugPanel.style.display = 'none';
      } else {
        debugPanel.style.display = 'block';
        debugPanel.textContent = 'Loading debug info\u2026';
        invoke('get_debug_info')
          .then(info => { debugPanel.textContent = info; })
          .catch(err => { debugPanel.textContent = `Error: ${err}`; });
      }
    }
  }
});

// Scroll in either column → redraw cables
document.getElementById('column-outputs').addEventListener('scroll', drawCables);
document.getElementById('column-inputs').addEventListener('scroll',  drawCables);

// ─── Init ─────────────────────────────────────────────────────────────────────
async function init() {
  if (!window.__TAURI__?.core?.invoke) {
    setTimeout(init, 50);
    return;
  }

  loadVirtualSinks();
  updateModeToggleLabel();

  // Check for required system tools before first refresh
  try {
    const missing = await invoke('check_deps');
    if (missing.length > 0) {
      showErrorBanner(
        `Missing tools: ${missing.join(', ')}. Make sure PipeWire and pipewire-pulse are installed.`
      );
    }
  } catch (_) {
    // Non-fatal: if check_deps itself fails, proceed anyway
  }

  // Recreate any persisted virtual sinks whose PipeWire modules did not survive
  // the last session, so their ports exist before we restore connections and
  // render — otherwise the sink cards stay stuck on "Waiting for PipeWire…".
  await ensureVirtualSinks();

  // Attempt to restore previously saved connections. Connections whose device
  // or port is not currently present are skipped silently by the backend and
  // retried automatically on later refreshes, so only genuine link failures
  // (both endpoints present, link still failed) are reported here. Fire it
  // without awaiting so a slow/failing restore never delays the first paint.
  reconnectSavedConnections({ surfaceErrors: true });

  refresh();
  startAutoRefresh();
}
init();
