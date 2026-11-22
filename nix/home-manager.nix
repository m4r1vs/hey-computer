# The Home Manager module: programs.hey-computer.
#
# `self` is this flake (for the default package), `obsidianSkills` the pinned
# checkout of github:obsidianmd/obsidian-skills.
{
  self,
  obsidianSkills,
}: {
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkOption mkEnableOption mkIf mkDefault types literalExpression;

  cfg = config.programs.hey-computer;
  tomlFormat = pkgs.formats.toml {};
  system = pkgs.stdenv.hostPlatform.system;

  /*
  The upstream obsidian-cli skill runs `obsidian …`. On NixOS `obsidian` is
  the Electron application and the command-line client is `obsidian-cli`
  (nixpkgs' obsidian package ships both), so a shim named `obsidian` comes
  first on the agent's PATH and the upstream skill works unchanged.
  */
  obsidianShim = pkgs.writeShellScriptBin "obsidian" ''
    exec ${lib.getExe' cfg.obsidian.package "obsidian-cli"} "$@"
  '';

  # From obsidian-skills, the ones that need no npm tools.
  obsidianSkillDirs =
    map (name: "${obsidianSkills}/plugins/obsidian/skills/${name}")
    ["obsidian-cli" "obsidian-markdown" "obsidian-bases" "json-canvas"];

  skillDirs =
    lib.optionals cfg.skills.enable (
      ["${cfg.package}/share/hey-computer/skills/hyprland-desktop"]
      ++ lib.optionals cfg.obsidian.enable obsidianSkillDirs
    )
    ++ map toString cfg.skills.extra;

  # What every agent can expect: a shell, the usual text tools, and what the
  # bundled skill names (jq, notify-send, playerctl, wpctl).
  basics = with pkgs; [
    bashInteractive
    coreutils
    findutils
    diffutils
    gnugrep
    gnused
    gawk
    procps
    curl
    jq
    ripgrep
    libnotify
    playerctl
    wireplumber
  ];

  agentPath = lib.concatStringsSep ":" (
    [
      (lib.makeBinPath (
        lib.optional cfg.obsidian.enable obsidianShim
        ++ lib.optional (cfg.agent.package != null) cfg.agent.package
        ++ cfg.extraPackages
        ++ basics
      ))
    ]
    ++ cfg.systemPath
  );

  /*
  A long answer's window must not belong to the service: with the default
  KillMode a restart of the daemon would close it. systemd-run --scope puts
  the viewer into a scope unit of its own.
  */
  viewerLauncher = [
    "${pkgs.systemd}/bin/systemd-run"
    "--user"
    "--scope"
    "--quiet"
    "--collect"
    "--"
  ];

  # Refused at build time when the program would refuse it at start.
  checkedConfig = settings: let
    toml = tomlFormat.generate "hey-computer-config.toml" settings;
  in
    pkgs.runCommand "hey-computer-config.toml" {} ''
      ${lib.getExe cfg.package} --config ${toml} debug config > /dev/null
      cp ${toml} $out
    '';

  configFile = checkedConfig cfg.settings;
in {
  options.programs.hey-computer = {
    enable = mkEnableOption "hey-computer, a voice assistant daemon that keeps one ACP agent warm";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.default;
      defaultText = literalExpression "hey-computer.packages.\${system}.default";
      description = "The hey-computer package.";
    };

    settings = mkOption {
      type = types.submodule {freeformType = tomlFormat.type;};
      default = {};
      example = literalExpression ''
        {
          agent.inactivity_timeout_secs = 60;
          conversation.follow_up_secs = 120;
          permissions.deny = ["sudo" "su" "doas" "pkexec" "run0" "shutdown"];
          instructions.extra = "Prefer metric units. Answer in kilometres and degrees Celsius.";
          present.max_chars = 300;
        }
      '';
      description = ''
        Contents of {file}`~/.config/hey-computer/config.toml`, see the
        README for every key. The module fills in `instructions.vault`,
        `instructions.skills` and `present.viewer` from its own options.
        The file is checked with the package's own binary when it is built,
        so an unknown key fails the build.
      '';
    };

    agent.package = mkOption {
      type = types.nullOr types.package;
      default = pkgs.antigravity-cli or null;
      defaultText = literalExpression "pkgs.antigravity-cli or null";
      description = ''
        The package that provides the agent command (`agy` by default), put
        on the agent's PATH. Null expects it elsewhere on the PATH, or an
        absolute path in `settings.agent.command`.
      '';
    };

    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [];
      example = literalExpression "[pkgs.python3 pkgs.wl-clipboard]";
      description = ''
        Put on the agent's PATH, ahead of {option}`systemPath`. Everything
        on that PATH can be run by the agent without confirmation.
      '';
    };

    systemPath = mkOption {
      type = types.listOf types.str;
      default = [
        "${config.home.profileDirectory}/bin"
        "/etc/profiles/per-user/${config.home.username}/bin"
        "/run/current-system/sw/bin"
        "/run/wrappers/bin"
        "/usr/local/bin"
        "/usr/bin"
        "/bin"
      ];
      description = ''
        Directories searched after the packages, so the agent finds the
        user's own programs (on NixOS, `hyprctl` lives here). Directories that
        do not exist are harmless. `/run/wrappers/bin` holds `sudo` on NixOS:
        it is on the PATH so that a privileged command the agent runs itself
        fails with a clear message because of NoNewPrivileges, rather than for
        want of a program.
      '';
    };

    vault = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/home/alice/Documents/Notes";
      description = ''
        An Obsidian vault the agent may read and write without confirmation.
        Sets `instructions.vault`. The vault is named in the agent's
        instructions but is not one of its workspace directories, so an
        AGENTS.md in the vault is not loaded as instructions.
      '';
    };

    obsidian = {
      enable = mkOption {
        type = types.bool;
        default = cfg.vault != null;
        defaultText = literalExpression "config.programs.hey-computer.vault != null";
        description = ''
          The obsidian-skills (obsidian-cli, obsidian-markdown,
          obsidian-bases, json-canvas) and the `obsidian` shim on the agent's
          PATH. The shim drives the running Obsidian application through its
          command-line client, `eval` and `delete` included.
        '';
      };

      package = mkOption {
        type = types.package;
        default = pkgs.obsidian;
        defaultText = literalExpression "pkgs.obsidian";
        description = "The Obsidian package whose `bin/obsidian-cli` the shim runs.";
      };
    };

    skills = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Give the agent the bundled `hyprland-desktop` skill, and the
          Obsidian skills when {option}`obsidian.enable` is set.
        '';
      };

      extra = mkOption {
        type = types.listOf (types.either types.path types.str);
        default = [];
        description = ''
          More skill directories: one skill (a directory with a `SKILL.md`)
          or a directory of skills each.
        '';
      };
    };

    viewer = {
      package = mkOption {
        type = types.nullOr types.package;
        default = pkgs.marker;
        defaultText = literalExpression "pkgs.marker";
        description = "Installed for the viewer command; null installs nothing.";
      };

      command = mkOption {
        type = types.listOf types.str;
        default = [
          "${pkgs.coreutils}/bin/env"
          # Marker is a unique GApplication: with a session bus, a second
          # answer would land in the first window as an editor tab. Without
          # one, every answer gets its own preview window.
          "DBUS_SESSION_BUS_ADDRESS=disabled:"
          (lib.getExe' cfg.viewer.package "marker")
          # The window's Wayland app id, for a window rule.
          "--name=hey-computer.answer"
          "--preview"
          "{file}"
        ];
        defaultText = literalExpression ''
          ["''${pkgs.coreutils}/bin/env" "DBUS_SESSION_BUS_ADDRESS=disabled:"
           "''${cfg.viewer.package}/bin/marker" "--name=hey-computer.answer" "--preview" "{file}"]
        '';
        description = ''
          Opens a long answer: program and arguments, `{file}` is the
          Markdown file. It is started in a systemd scope of its own, so the
          window outlives a restart of the daemon. Empty shows the file's path
          in a notification instead. With the default, the window's app id is
          `hey-computer.answer`.
        '';
      };
    };

    service.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Run the daemon as a systemd user service bound to the graphical session.";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [cfg.package] ++ lib.optional (cfg.viewer.package != null) cfg.viewer.package;

    programs.hey-computer.settings = {
      instructions = {
        vault = mkIf (cfg.vault != null) (mkDefault cfg.vault);
        skills = mkIf (skillDirs != []) skillDirs;
      };
      present.viewer = mkIf (cfg.viewer.command != []) (mkDefault (viewerLauncher ++ cfg.viewer.command));
    };

    xdg.configFile."hey-computer/config.toml".source = configFile;

    systemd.user.services.hey-computer = mkIf cfg.service.enable {
      Unit = {
        Description = "hey-computer voice assistant";
        After = ["graphical-session.target"];
        PartOf = ["graphical-session.target"];
        X-Restart-Triggers = ["${configFile}"];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 2;
        /*
        The agent runs every command without confirmation. With no_new_privs
        set, a setuid program (sudo, su, pkexec, …) that the agent runs itself
        cannot raise its privileges, however the command is spelled. It is
        not a sandbox: a command the agent has the systemd user manager or
        the compositor start runs without the flag (see the README's
        permission model). The deny list only gives the model a clean
        refusal.
        */
        NoNewPrivileges = true;
        Environment = ["PATH=${agentPath}"];
      };
      Install.WantedBy = ["graphical-session.target"];
    };
  };
}
