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
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [pkgs.makeWrapper];

        postInstall = ''
          mkdir -p $out/share/hey-computer
          cp -r skills $out/share/hey-computer/skills
          wrapProgram $out/bin/hey-computer \
            --prefix PATH : "${pkgs.lib.makeBinPath (runtimeTools pkgs)}"
        '';

        meta = with pkgs.lib; {
          description = "A voice assistant daemon that keeps one ACP agent warm and answers one-shot requests";
          homepage = "https://github.com/m4r1vs/hey-computer";
          license = licenses.mit;
          platforms = platforms.linux;
          mainProgram = "hey-computer";
        };
      };

    homeManagerModule = import ./nix/home-manager.nix {
      inherit self;
      obsidianSkills = obsidian-skills;
    };
  in {
    overlays.default = final: _prev: {
      hey-computer = mkPackage final;
    };

    packages = forAllSystems (system: {
      default = mkPackage (pkgsFor system);
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
