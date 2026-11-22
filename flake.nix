{
  description = "hey-computer: a voice assistant daemon that keeps one ACP agent warm";

  inputs = {
    # A consuming flake may make this follow its own nixpkgs; the package
    # needs nothing newer than a Rust toolchain that knows edition 2024.
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs = {
    self,
    nixpkgs,
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
        src = pkgs.lib.cleanSource ./.;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [pkgs.makeWrapper];

        postInstall = ''
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
  in {
    overlays.default = final: _prev: {
      hey-computer = mkPackage final;
    };

    packages = forAllSystems (system: {
      default = mkPackage (pkgsFor system);
    });

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
          nativeBuildInputs = [pkgs.rustfmt];
        } ''
          find ${./src} -name '*.rs' -exec rustfmt --check --edition 2024 {} +
          touch $out
        '';
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
