{
  description = "hey-computer: a voice assistant daemon that keeps one ACP agent warm";

  inputs = {
    # A consuming flake should make this follow its own nixpkgs; the package
    # needs nothing newer than a Rust toolchain that knows edition 2024.
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    # Only for checking the Home Manager module here (checks.module); the
    # module itself does not use it. A consumer should make it follow its own.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # The Obsidian skills linked into the agent's directory. Pinned to a
    # commit so that what the agent is told does not change under it.
    obsidian-skills = {
      url = "github:obsidianmd/obsidian-skills/b110b2f8e9eb1f287892f937050dfd40af862cbb";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    home-manager,
    obsidian-skills,
    ...
  }: let
    supportedSystems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    pkgsFor = system: import nixpkgs {inherit system;};

    # Tools the daemon itself shells out to. The agent's own PATH (agy and
    # whatever it should be able to run) is the service's business.
    runtimeTools = pkgs:
      with pkgs; [
        libnotify # notify-send, the default notifier
      ];

    mkPackage = pkgs:
      pkgs.rustPlatform.buildRustPackage {
        pname = "hey-computer";
        version = "0.1.0";
        # Only what the build reads, so editing the docs or the Nix files
        # does not rebuild the program.
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./rustfmt.toml
            ./src
            ./tests
            ./skills
            ./assets
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [
          "--bin"
          "hey-computer"
          "--bin"
          "hey-computer-viewer"
        ];

        nativeBuildInputs = [
          pkgs.makeWrapper
          pkgs.pkg-config
          pkgs.wrapGAppsHook4
        ];
        buildInputs = [
          pkgs.glib-networking
          pkgs.gtk4
          pkgs.sqlite
          pkgs.webkitgtk_6_0
        ];

        # Wrap `hey-computer-viewer` with GApps/WebKitGTK environment variables
        # while keeping `hey-computer` (the daemon & CLI) free of GUI wrapper
        # variables that would otherwise leak into `agy acp`.
        dontWrapGApps = true;

        postInstall = ''
          mkdir -p $out/share/hey-computer
          cp -r skills $out/share/hey-computer/skills
          cp -r assets $out/share/hey-computer/assets
        '';

        postFixup = ''
          wrapProgram $out/bin/hey-computer \
            --prefix PATH : "${pkgs.lib.makeBinPath (runtimeTools pkgs)}"
          wrapProgram $out/bin/hey-computer-viewer \
            "''${gappsWrapperArgs[@]}" \
            --set-default HC_VIEWER_ASSETS_DIR "$out/share/hey-computer/assets"
        '';

        meta = with pkgs.lib; {
          description = "A voice assistant daemon that keeps one ACP agent warm and answers one-shot requests";
          homepage = "https://github.com/m4r1vs/hey-computer";
          license = licenses.mit;
          platforms = platforms.linux;
          mainProgram = "hey-computer";
        };
      };

    mkBaileysReader = pkgs:
      pkgs.buildNpmPackage {
        pname = "hey-computer-baileys-reader";
        version = "0.1.0";
        src = ./bridge/baileys-reader;
        nodejs = pkgs.nodejs_22;
        npmDepsHash = "sha256-6r70im6uK0Bha+UWmg1Bst4QA2kjjd0+dmbM3dP+kZs=";
        npmFlags = ["--ignore-scripts"];
        dontNpmBuild = true;
        nativeBuildInputs = [pkgs.makeWrapper];

        installPhase = ''
          runHook preInstall
          mkdir -p $out/lib/hey-computer-baileys-reader $out/bin
          cp -r index.mjs package.json node_modules $out/lib/hey-computer-baileys-reader/
          makeWrapper ${pkgs.nodejs_22}/bin/node $out/bin/hey-computer-baileys-reader \
            --add-flags "$out/lib/hey-computer-baileys-reader/index.mjs"
          runHook postInstall
        '';

        meta = with pkgs.lib; {
          description = "Strictly read-only WhatsApp Web linked-device reader for hey-computer-bridge";
          homepage = "https://github.com/m4r1vs/hey-computer";
          license = licenses.mit;
          platforms = platforms.linux;
          mainProgram = "hey-computer-baileys-reader";
        };
      };

    mkBridgePackage = pkgs: let
      baileysReader = mkBaileysReader pkgs;
    in
      pkgs.rustPlatform.buildRustPackage {
        pname = "hey-computer-messenger-bridge";
        version = "0.1.0";
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./rustfmt.toml
            ./src
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [
          "--bin"
          "hey-computer-bridge"
        ];
        doCheck = false;

        nativeBuildInputs = [
          pkgs.makeWrapper
          pkgs.pkg-config
        ];
        buildInputs = [
          pkgs.glib-networking
          pkgs.gtk4
          pkgs.sqlite
          pkgs.webkitgtk_6_0
        ];

        postFixup = ''
          wrapProgram $out/bin/hey-computer-bridge \
            --prefix PATH : "${pkgs.lib.makeBinPath [baileysReader pkgs.signal-cli]}"
        '';

        meta = with pkgs.lib; {
          description = "Strictly read-only WhatsApp and Signal messenger bridge for hey-computer";
          homepage = "https://github.com/m4r1vs/hey-computer";
          license = licenses.mit;
          platforms = platforms.linux;
          mainProgram = "hey-computer-bridge";
        };
      };

    homeManagerModule = import ./nix/home-manager.nix {
      inherit self;
      obsidianSkills = obsidian-skills;
    };
  in {
    overlays.default = final: _prev: {
      hey-computer = mkPackage final;
      hey-computer-messenger-bridge = mkBridgePackage final;
    };

    packages = forAllSystems (system: let
      pkgs = pkgsFor system;
    in {
      default = mkPackage pkgs;
      messenger-bridge = mkBridgePackage pkgs;
    });

    homeManagerModules.default = homeManagerModule;

    checks = forAllSystems (system: let
      pkgs = pkgsFor system;
    in {
      # Also runs `cargo test` (unit and integration tests, the mock agent
      # included) through buildRustPackage's checkPhase.
      build = mkPackage pkgs;

      clippy = (mkPackage pkgs).overrideAttrs (old: {
        pname = "${old.pname}-clippy";
        nativeBuildInputs = old.nativeBuildInputs ++ [pkgs.clippy];
        buildPhase = ''
          cargo clippy --all-targets --all-features -- -D warnings
        '';
        doCheck = false;
        installPhase = "touch $out";
        dontFixup = true;
      });

      fmt =
        pkgs.runCommand "hey-computer-fmt" {
          nativeBuildInputs = [pkgs.rustfmt pkgs.alejandra];
        } ''
          find ${./src} ${./tests} -name '*.rs' -exec rustfmt --check --edition 2024 {} +
          alejandra --check --quiet ${./flake.nix} ${./nix}
          touch $out
        '';

      # The module in a minimal Home Manager configuration: an option typo
      # fails here, and the generated configuration and service are checked.
      module = import ./nix/module-check.nix {
        inherit pkgs home-manager;
        module = homeManagerModule;
      };
    });

    formatter = forAllSystems (system: (pkgsFor system).alejandra);

    devShells = forAllSystems (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        nativeBuildInputs = [pkgs.pkg-config];
        buildInputs = [
          pkgs.glib-networking
          pkgs.gtk4
          pkgs.sqlite
          pkgs.webkitgtk_6_0
        ];
        packages = with pkgs;
          [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            alejandra
            jq
            socat
          ]
          ++ (runtimeTools pkgs);

        env = {
          RUST_LOG = "hey_computer=debug,info";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      };
    });
  };
}
