{
  description = "AudioPlumber — Visual PipeWire Patchbay (Tauri 2 + Rust)";

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

          # Disable Rust incremental to keep builds deterministic
          CARGO_INCREMENTAL = "0";

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

          # Cargo.lock lives in src-tauri/
          cargoLock.lockFile = ./src-tauri/Cargo.lock;

          # Build only the src-tauri subdirectory
          buildAndTestSubdir = "src-tauri";

          nativeBuildInputs = with pkgs; [
            pkg-config
            gobject-introspection
            wrapGAppsHook3  # wraps the binary with GTK/GSettings env
          ];

          # Reuse the shared buildDeps list
          buildInputs = buildDeps;

          # Tauri's build.rs looks for frontendDist = "../ui" relative to
          # src-tauri/. With src = ./., the ui/ dir is present at the right
          # relative path during the build.

          PKG_CONFIG_PATH = with pkgs; lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildDeps;

          postInstall = ''
            wrapProgram $out/bin/audio-plumber \
              --prefix XDG_DATA_DIRS : "${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}" \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath buildDeps}"

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
      nixosModules.default = { config, lib, pkgs, ... }: {
        options.programs.audioplumber.enable =
          lib.mkEnableOption "AudioPlumber visual PipeWire patchbay";

        config = lib.mkIf config.programs.audioplumber.enable {
          environment.systemPackages = [
            self.packages.${pkgs.system}.default
          ];
        };
      };
    };
}
