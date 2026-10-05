# AudioPlumber

> Visual PipeWire patchbay — a desktop GUI for wiring up your audio graph.

![Platform: Linux](https://img.shields.io/badge/platform-Linux-informational)
![Built with egui](https://img.shields.io/badge/built%20with-egui-black)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584)
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
single native binary written entirely in Rust, with a pure-Rust
[egui](https://github.com/emilk/egui)/[eframe](https://github.com/emilk/egui/tree/master/crates/eframe)
GUI. There is no webview and no web runtime — it draws its own window with
OpenGL, which keeps the dependency footprint small (no GTK/WebKit).

<!-- TODO: screenshot -->

## Features

- **Two-column visual patchbay** — virtual sinks on the left, real audio
  outputs (auto-discovered) on the right, with hand-drawn glowing bezier cables
  rendered directly with egui's painter.
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

- **PipeWire** plus its command-line tools — `pw-link` and `pw-dump`. On
  Debian/Ubuntu these live in the **`pipewire-bin`** package; on Fedora in
  **`pipewire-utils`**.
- **`pactl`** — used to create and remove virtual sinks. Ships in
  **`pulseaudio-utils`** (it works against `pipewire-pulse`).

The app checks for `pw-link` and `pactl` on startup and shows a banner if they
are not found. These runtime dependencies are declared by the `.deb` and `.rpm`
packages, so installing those pulls them in automatically.

## Installation

### Prebuilt packages (.deb / .rpm / .AppImage)

Each tagged release publishes three Linux bundles on the GitHub Releases page:

- **`.deb`** (Debian/Ubuntu) — `sudo apt install ./audioplumber*.deb`. Declares
  its runtime dependencies (`pipewire`, `pipewire-bin`, `pulseaudio-utils`), so
  they are pulled in automatically.
- **`.rpm`** (Fedora/openSUSE) — `sudo dnf install ./audioplumber*.rpm`. Requires
  `pipewire`, `pipewire-utils` and `pulseaudio-utils`.
- **`.AppImage`** — `chmod +x audioplumber*.AppImage && ./audioplumber*.AppImage`.
  The AppImage bundles the OpenGL/Wayland/X11 **client** libraries it needs, so
  it runs on most distros without extra packages. It still relies on the host's
  GPU/GL **driver** (the Mesa DRI module), which is hardware-specific and comes
  with any normal desktop install. You still need the PipeWire tools
  (`pw-link`/`pactl`) present at runtime.

### NixOS (flake module)

The flake input URL is `github:ExpressoCodes/AudioPlumber`.

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

The flake ships a dev shell with the Rust toolchain and the OpenGL/Wayland/X11
system libraries the egui GUI links against:

```sh
nix develop
```

Inside the shell:

```sh
cargo run                 # build and run the app
cargo build --release     # produce an optimised build
cargo test                # run the Rust unit tests
```

The optimised binary is written to `src-tauri/target/release/audio-plumber`.
There is no JavaScript, npm, or webview build step — the whole app is Rust.

### Project layout

```
.
├── flake.nix                  # dev shell, package, `nix run` app, NixOS module
├── LICENSE
├── packaging/
│   └── audioplumber.desktop   # desktop entry installed by the packages
└── src-tauri/                 # Rust crate (directory name kept from the Tauri era)
    ├── Cargo.toml             # crate manifest + .deb / .rpm packaging metadata
    ├── icons/
    └── src/
        ├── main.rs            # eframe entry point
        ├── app.rs             # native egui/eframe patchbay UI
        ├── pipewire.rs        # GUI-free backend over pw-link / pw-dump / pactl
        └── lib.rs             # library crate exposing `pipewire` + `app`
```

### Toolchain (for non-Nix builds)

If you are building outside the Nix dev shell you will need:

- A stable **Rust** toolchain and `pkg-config`.
- The OpenGL/windowing development libraries. On **Debian/Ubuntu** (matching what
  CI installs):

  ```sh
  sudo apt install \
    libgl1-mesa-dev libegl1-mesa-dev libgbm-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
    pkg-config build-essential
  ```

  On **Nix**, the flake declares the equivalents (`libGL`, `libxkbcommon`,
  `wayland`, `libx11`, `libxcursor`, `libxrandr`, `libxi`).

Build with `cargo build --release` from the `src-tauri/` directory (or pass
`--manifest-path src-tauri/Cargo.toml`).

### Packaging

Tagged releases build the `.deb`, `.rpm` and `.AppImage` bundles in CI
(`.github/workflows/release.yml`) with `cargo-deb`, `cargo-generate-rpm` and
`linuxdeploy`; the deb/rpm metadata lives in `src-tauri/Cargo.toml`.

## Configuration

AudioPlumber stores its state under `~/.config/audioplumber`:

- `~/.config/audioplumber/connections.json` — the saved link list, written when
  you change connections and read back on startup to restore them.

The config directory honours `XDG_CONFIG_HOME` if it is set. There is nothing to
edit by hand; the file is managed by the app.
