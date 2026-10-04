# AudioPlumber

> Visual PipeWire patchbay — a desktop GUI for wiring up your audio graph.

![Platform: Linux](https://img.shields.io/badge/platform-Linux-informational)
![Built with Tauri 2](https://img.shields.io/badge/built%20with-Tauri%202-24C8DB)
![Backend: Rust](https://img.shields.io/badge/backend-Rust-dea584)
![Audio: PipeWire](https://img.shields.io/badge/audio-PipeWire-blue)

## What is AudioPlumber?

On Linux, [PipeWire](https://pipewire.org/) is the system that carries sound
(and MIDI) between your applications and your hardware. Every app and device
exposes **ports** — a browser has audio *output* ports, your speakers have
*input* ports, a recording tool has inputs it wants to capture from, and so on.
Normally those connections are made for you automatically, but the moment you
want something non-default — recording one app but not another, sending game
audio to a stream but keeping voice chat on your headphones, feeding a
synth's MIDI into a particular instrument — you have to rewire the graph
yourself.

A **patchbay** is the classic tool for exactly that. Picture a board full of
sockets where you run a cable from an *output* on one side to an *input* on the
other; whatever is plugged in at one end flows to the other. AudioPlumber is
that board for PipeWire: instead of running `pw-link` commands in a terminal and
remembering cryptic port names, you get a window with outputs on one side,
inputs on the other, and cables you can make and break with a click.

**What you can do with it:**

- **See your routing as a graph** — AudioPlumber lists the real audio outputs it
  finds on your system and shows the connections (cables) between them, labelled
  with friendly, human-readable device names rather than raw PipeWire IDs.
- **Connect and disconnect with clicks** — click a port on one side and a port on
  the other to patch them together; click a cable (or right-click a port) to
  break the connection. No command line required.
- **Create virtual sinks** — make on-the-fly "virtual" audio devices you can
  route multiple apps into (handy for recording or streaming a custom mix), then
  remove them when you're done.
- **Keep it simple or go granular** — *Simple* mode treats stereo pairs as a
  single cable so routing stays tidy; *Advanced* mode exposes every individual
  port when you need fine control.
- **Set it and forget it** — your connections are saved and automatically
  restored the next time you launch, so your routing survives reboots.

Because PipeWire also carries MIDI through the same `pw-link` mechanism, MIDI
ports show up alongside audio ones and can be patched the same way.

**Why use it?** If the built-in defaults don't do what you want, the usual
alternative is hand-typing `pw-link` commands and decoding machine-generated
port names — error-prone and hard to visualise. AudioPlumber turns that into a
point-and-click picture of your audio graph, while staying a thin, transparent
wrapper over the standard PipeWire tools (so it is predictable and does nothing
behind your back).

Under the hood it is a small, dependency-light front end over the standard
PipeWire / PulseAudio command-line tools (`pw-link`, `pw-dump`, `pactl`): a
[Tauri 2](https://v2.tauri.app/) shell with a Rust backend and a plain
HTML/CSS/JavaScript UI (no bundler, no framework).

<!-- TODO: screenshot -->

## Features

- **Two-column visual patchbay** — virtual sinks on the left, real audio
  outputs (auto-discovered) on the right, with cables drawn as an SVG overlay.
- **Click to connect, click to disconnect** — click a monitor port on a virtual
  sink, then an output port, to create a link; click a cable to remove it.
  Right-click a port to disconnect everything attached to it.
- **Create and delete virtual sinks** — spin up null sinks on demand (via
  `pactl load-module module-null-sink`) and tear them down from the UI.
- **Simple and Advanced modes** — *Simple* mode bundles stereo ports into a
  single cable per node; *Advanced* mode exposes every individual port.
- **Persistent connections** — your links are saved and automatically restored
  on the next launch.
- **Friendly node names** — resolves raw PipeWire node names to their
  human-readable descriptions using `pw-dump`.
- **Dependency check** — warns on startup if required PipeWire tools are
  missing.
- **Debug panel** — press `D` to view the raw `pw-link` output for
  troubleshooting port-naming issues.
- **Live refresh** — manual refresh button plus periodic auto-refresh.

## Requirements

AudioPlumber drives the standard PipeWire tooling, so you need a working
PipeWire stack at runtime:

- **PipeWire** — provides `pw-link` and `pw-dump`.
- **pipewire-pulse** (PulseAudio compatibility) — provides `pactl`, used to
  create and remove virtual sinks.

The app checks for `pw-link` and `pactl` on startup and shows a banner if they
are not found.

## Installation

The flake input URL is `github:ExpressoCodes/AudioPlumber`.

### NixOS (flake module)

Add AudioPlumber as a flake input and import its NixOS module. The module is
**enabled by default**, so importing it is all you need to install the app:

```nix
{
  inputs.audioplumber.url = "github:ExpressoCodes/AudioPlumber";

  # In your NixOS configuration:
  # imports = [ inputs.audioplumber.nixosModules.default ];
}
```

```nix
{ inputs, ... }:
{
  imports = [ inputs.audioplumber.nixosModules.default ];

  # Opt out if you ever want to disable it:
  # programs.audioplumber.enable = false;
}
```

Alternatively, add the package directly to `systemPackages`:

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.audioplumber.packages.${pkgs.system}.default
  ];
}
```

### Try it without installing

The flake exposes a runnable default app:

```sh
nix run github:ExpressoCodes/AudioPlumber
```

### Build the package locally

```sh
git clone git@github.com:ExpressoCodes/AudioPlumber.git
cd AudioPlumber
nix build        # result -> ./result/bin/audio-plumber
./result/bin/audio-plumber
```

> The installed binary is named `audio-plumber`; an `audioplumber` alias is also
> provided. A desktop entry and icon are installed so the app shows up in your
> application launcher.

## Building from source / development

The flake ships a dev shell with the full toolchain (a Tauri 2 Rust toolchain,
`cargo-tauri`, and all the GTK/WebKit/PipeWire system libraries):

```sh
nix develop
```

Inside the shell:

```sh
cargo tauri dev      # run the app with a live dev window
cargo tauri build    # produce a production build
cargo test           # run the Rust unit tests
```

The frontend (in `ui/`) is plain static `index.html` / `style.css` / `main.js`
with `withGlobalTauri` enabled — there is no JavaScript build step or package
manager to install.

### Project layout

```
.
├── flake.nix              # dev shell, package, nix run app, NixOS module
├── ui/                    # static front end (HTML/CSS/JS)
│   ├── index.html
│   ├── main.js
│   └── style.css
└── src-tauri/             # Rust / Tauri backend
    ├── Cargo.toml
    ├── src/main.rs        # Tauri commands wrapping pw-link / pw-dump / pactl
    ├── tauri.conf.json
    └── icons/
```

### Toolchain (for non-Nix builds)

If you are building outside the Nix dev shell, you will need roughly what the
flake provides:

- Rust (stable, 1.77+ for Tauri 2)
- `cargo-tauri` (Tauri CLI v2)
- GTK 3, WebKitGTK 4.1, libsoup 3, and the usual Tauri Linux system libraries
  (`glib`, `cairo`, `pango`, `gdk-pixbuf`, `atk`, `dbus`, `openssl`, `librsvg`,
  `libayatana-appindicator`, `xdotool`), plus `pkg-config` and
  `gobject-introspection`.

## Configuration

AudioPlumber stores its state under `~/.config/audioplumber`:

- `~/.config/audioplumber/connections.json` — the saved link list, written when
  you change connections and read back on startup to restore them.

The config directory honours `XDG_CONFIG_HOME` if it is set. There is nothing to
edit by hand; the file is managed by the app.
