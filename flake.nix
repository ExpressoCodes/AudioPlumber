{
  description = "AudioPlumber — Visual PipeWire Patchbay (pure-Rust egui/eframe)";

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

        # Native build tools. The webkit2gtk/javascriptcore/GObject chain that
        # Tauri required is gone — the app is now a pure-Rust egui/eframe binary.
        nativeBuildDeps = with pkgs; [
          pkg-config
          sccache       # compilation cache (dev shell only — see RUSTC_WRAPPER below)
        ];

        # Graphics/runtime libraries for the native egui/eframe binary
        # (glow OpenGL backend + winit X11/Wayland windowing + keyboard).
        buildDeps = with pkgs; [
          libGL
          libxkbcommon
          wayland
          libx11
          libxcursor
          libxrandr
          libxi
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
            echo "  AudioPlumber dev shell (pure-Rust egui)"
            echo "  Rust : $(rustc --version)"
            echo ""
            echo "  Commands:"
            echo "    cargo run           — start the app"
            echo "    cargo build --release — production build"
            echo "    cargo test          — run unit tests"
            echo ""
          '';

          PKG_CONFIG_PATH = with pkgs; lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildDeps;

          # winit/glow dlopen these at runtime; expose them to `cargo run`.
          LD_LIBRARY_PATH = with pkgs; lib.makeLibraryPath buildDeps;

          # Cache compiled crates across clean builds / branch switches.
          # DEV-SHELL-ONLY and deliberately NOT set on packages.default: the
          # pure `nix build` sandbox has no cache dir / network, where sccache
          # can break or non-determinize the reproducible output.
          RUSTC_WRAPPER = "sccache";
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

          # This derivation builds ONLY the plain Rust binary: buildRustPackage's
          # cargoBuildHook runs `cargo build --release`. The deb/rpm/AppImage
          # bundles that tauri-action used to emit are now produced separately
          # by cargo-deb / cargo-generate-rpm / linuxdeploy in release.yml.
          #
          # Keep incremental compilation off here so the reproducible package
          # output stays deterministic (the dev shell drops this for fast
          # interactive rebuilds).
          CARGO_INCREMENTAL = "0";

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ];

          # Graphics/runtime libraries linked/dlopened by the egui binary.
          buildInputs = buildDeps;

          # winit/glow resolve X11/Wayland/GL via dlopen at runtime, so the
          # installed binary needs them on its library path.
          postFixup = ''
            wrapProgram $out/bin/audio-plumber \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath buildDeps}"
          '';

          PKG_CONFIG_PATH = with pkgs; lib.makeSearchPathOutput "dev" "lib/pkgconfig" buildDeps;

          postInstall = ''
            # Provide audioplumber as an alias for audio-plumber
            ln -s $out/bin/audio-plumber $out/bin/audioplumber

            # Install .desktop file and icon so the app appears in launchers
            mkdir -p $out/share/applications $out/share/icons/hicolor/128x128/apps
            cp ${./packaging/audioplumber.desktop} $out/share/applications/audioplumber.desktop
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
