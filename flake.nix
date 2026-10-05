{
  description = "AudioPlumber — Visual PipeWire Patchbay (Tauri 2 + Rust)";

  # Lets flake consumers pull prebuilt binaries from our GitHub Pages binary
  # cache instead of recompiling the 417-crate tree from source. The cache is
  # populated by the nix-cache CI workflow on every push to main / v* tag.
  # Consumers who are not trusted users will be prompted to accept this
  # substituter + key the first time (trusted users / root get it silently).
  nixConfig = {
    extraSubstituters = [ "https://expressocodes.github.io/AudioPlumber" ];
    extraTrustedPublicKeys = [ "audioplumber-1:qqVyHW95S+ijbXZK8rXDGMMYtq9uhvyQaIbCrCmLsNc=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    # Merge per-system outputs with top-level outputs (nixosModules must be
    # top-level — it is not architecture-specific).
    (flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Use a stable Rust toolchain (Tauri 2 needs at least 1.77)
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "x86_64-unknown-linux-gnu" ];
        };

        # Tauri 2 needs these native libs on NixOS
        nativeBuildDeps = with pkgs; [
          pkg-config
          gobject-introspection
          cargo-tauri   # tauri-cli v2 from nixpkgs
          nodejs_22     # includes npm
          sccache       # compilation cache (dev shell only — see RUSTC_WRAPPER below)
        ];

        buildDeps = with pkgs; [
          # GTK / WebKit stack
          gtk3
          webkitgtk_4_1   # tauri 2 uses webkitgtk 4.1
          libsoup_3
          glib
          cairo
          pango
          gdk-pixbuf
          atk
          dbus
          openssl
          xdotool

          # Audio
          pipewire

          # Other system libs Tauri links against
          librsvg
          libayatana-appindicator

          # Graphics stack for the native egui/eframe binary (glow/winit):
          # OpenGL loader + X11/Wayland + keyboard handling.
          libGL
          libxkbcommon
          wayland
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
        ];

      in
      {
        # ------------------------------------------------------------------ #
        #  Dev shell                                                           #
        # ------------------------------------------------------------------ #
        devShells.default = pkgs.mkShell {
          name = "audioplumber-dev";

          nativeBuildInputs = nativeBuildDeps;
          buildInputs = buildDeps ++ [ rustToolchain ];

          shellHook = ''
            echo ""
            echo "  AudioPlumber dev shell"
            echo "  Rust : $(rustc --version)"
            echo "  Tauri CLI: $(cargo tauri --version 2>/dev/null || echo 'run: cargo install tauri-cli')"
            echo ""
            echo "  Commands:"
            echo "    cargo tauri dev     — start dev server"
            echo "    cargo tauri build   — production build"
            echo "    cargo test          — run unit tests"
            echo ""
          '';

          # Required for Tauri / WebKitGTK to find system libraries
          PKG_CONFIG_PATH = with pkgs; lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildDeps;

          LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath buildDeps;

          # WebKit requires a valid XDG runtime dir
          XDG_DATA_DIRS = with pkgs; lib.concatStringsSep ":" [
            "${gtk3}/share/gsettings-schemas/${gtk3.name}"
            "${gsettings-desktop-schemas}/share/gsettings-schemas/${gsettings-desktop-schemas.name}"
          ];

          # Leave CARGO_INCREMENTAL unset in the dev shell so `cargo tauri dev`
          # gets fast incremental rebuilds. Determinism of the reproducible
          # `nix build` package output is controlled separately by
          # buildRustPackage (see packages.default below) and is unaffected.

          # Cache compiled crates across clean builds / branch switches. The
          # GTK/WebKit crate tree (webkit2gtk, tao, gdkx11, ...) is identical
          # across builds and dominates compile time, so sccache gives a big
          # win on clean rebuilds. This is DEV-SHELL-ONLY and deliberately NOT
          # set on packages.default: the pure `nix build` sandbox has no cache
          # dir / network, where sccache can break or non-determinize the
          # reproducible output. Keep it here only.
          RUSTC_WRAPPER = "sccache";

          # OpenSSL config
          OPENSSL_DIR = "${pkgs.openssl.dev}";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
        };

        # ------------------------------------------------------------------ #
        #  Installable package — `nix build` / `nix profile install`          #
        # ------------------------------------------------------------------ #
        # NOTE: src-tauri/Cargo.lock is present. If it ever goes missing,
        # run `cargo build` once inside the dev shell to regenerate it before
        # running `nix build`.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "audioplumber";
          version = "0.1.0";
          src = ./.;

          # Cargo.lock lives in src-tauri/ — copy it to root where buildRustPackage expects it
          cargoLock.lockFile = ./src-tauri/Cargo.lock;
          postPatch = "cp src-tauri/Cargo.lock Cargo.lock";

          # Build only the src-tauri subdirectory
          buildAndTestSubdir = "src-tauri";

          # Skip the check phase: unit tests run in CI, not in the install build
          # — avoids a full second compile of the 417-crate tree.
          doCheck = false;

          # IMPORTANT: this path builds ONLY the Rust/NixOS binary and never the
          # Tauri bundler. buildRustPackage's build phase (cargoBuildHook) invokes
          # plain `cargo build --release`; it does NOT run `cargo tauri build`, so
          # no deb/rpm/AppImage bundles are ever produced here. This is guaranteed
          # by construction — we intentionally do not use cargo-tauri in this
          # derivation. (Regular `cargo tauri build` in the dev shell still
          # produces all three bundle formats via tauri.conf.json's bundle.targets
          # = "all"; that path is deliberately left untouched.)
          #
          # Keep incremental compilation off here so the reproducible package
          # output stays deterministic (the dev shell drops this for fast
          # interactive rebuilds).
          CARGO_INCREMENTAL = "0";

          nativeBuildInputs = with pkgs; [
            pkg-config
            gobject-introspection
            wrapGAppsHook3  # wraps the binary with GTK/GSettings env; do NOT also call wrapProgram manually
          ];

          # Pass extra env to wrapGAppsHook3 instead of calling wrapProgram ourselves
          preFixup = ''
            gappsWrapperArgs+=(
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath buildDeps}"
            )
          '';

          # Reuse the shared buildDeps list
          buildInputs = buildDeps;

          # Tauri's build.rs looks for frontendDist = "../ui" relative to
          # src-tauri/. With src = ./., the ui/ dir is present at the right
          # relative path during the build.

          PKG_CONFIG_PATH = with pkgs; lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildDeps;

          postInstall = ''
            # Provide audioplumber as an alias for audio-plumber
            ln -s $out/bin/audio-plumber $out/bin/audioplumber

            # Install .desktop file and icon so the app appears in launchers
            mkdir -p $out/share/applications $out/share/icons/hicolor/128x128/apps

            cat > $out/share/applications/audioplumber.desktop <<EOF
[Desktop Entry]
Name=AudioPlumber
Comment=Visual PipeWire audio patchbay
Exec=$out/bin/audio-plumber
Icon=audioplumber
Type=Application
Categories=AudioVideo;Audio;Mixer;
Keywords=audio;pipewire;patchbay;routing;virtual;sink;
StartupNotify=true
EOF

            cp ${./src-tauri/icons/128x128.png} $out/share/icons/hicolor/128x128/apps/audioplumber.png
          '';
        };

        # ------------------------------------------------------------------ #
        #  App — `nix run`                                                    #
        # ------------------------------------------------------------------ #
        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/audio-plumber";
        };
      }
    )) // {
      # -------------------------------------------------------------------- #
      #  NixOS module — system-level installation                             #
      # -------------------------------------------------------------------- #
      # nixosModules must be top-level (not inside eachDefaultSystem) because
      # NixOS modules are architecture-independent declarations.
      nixosModules.default = { config, lib, pkgs, ... }:
        let system = pkgs.stdenv.hostPlatform.system; in {
        options.programs.audioplumber.enable = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''
            Whether to install the AudioPlumber visual PipeWire patchbay.
            Enabled by default: importing this module is enough to install
            the app. Set to false to opt out.
          '';
        };

        config = lib.mkIf config.programs.audioplumber.enable {
          environment.systemPackages = [
            self.packages.${system}.default
          ];
          # Ensure icon cache is updated so the launcher icon shows
          environment.pathsToLink = [ "/share/icons" "/share/applications" ];
        };
      };
    };
}
