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
      ++ lib.optionals cfg.recall.enable ["${cfg.package}/share/hey-computer/skills/recall"]
      ++ lib.optionals cfg.tmux.enable ["${cfg.package}/share/hey-computer/skills/tmux"]
      ++ lib.optionals cfg.schedule.enable ["${cfg.package}/share/hey-computer/skills/schedule"]
      ++ lib.optionals cfg.obsidian.enable obsidianSkillDirs
      ++ lib.optionals cfg.openhue.enable ["${cfg.package}/share/hey-computer/skills/openhue"]
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
    xdg-utils
  ];

  agentPath = lib.concatStringsSep ":" (
    [
      (lib.makeBinPath (
        lib.optional cfg.obsidian.enable obsidianShim
        ++ lib.optional cfg.openhue.enable cfg.openhue.package
        ++ lib.optional cfg.tmux.enable cfg.tmux.package
        ++ lib.optional (cfg.agent.package != null) cfg.agent.package
        ++ [cfg.package]
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

  mkPiperScript = lang: model: rate:
    pkgs.writeShellScript "hey-computer-speak-${lang}" ''
      set -euo pipefail
      ${lib.getExe' cfg.speak.package "piper"} --model ${lib.escapeShellArg (toString model)} --output-raw \
        | ${lib.getExe' pkgs.pipewire "pw-play"} --raw --rate ${toString rate} --channels 1 --format s16 -
    '';

  speakCommandEn =
    if cfg.speak.commandEn != []
    then cfg.speak.commandEn
    else
      lib.optionals (cfg.speak.package != null && cfg.speak.voiceEn != null) [
        "${mkPiperScript "en" cfg.speak.voiceEn cfg.speak.sampleRateEn}"
      ];

  speakCommandDe =
    if cfg.speak.commandDe != []
    then cfg.speak.commandDe
    else
      lib.optionals (cfg.speak.package != null && cfg.speak.voiceDe != null) [
        "${mkPiperScript "de" cfg.speak.voiceDe cfg.speak.sampleRateDe}"
      ];

  # Translate programs.hey-computer.mcp.servers into settings.mcp.<name>,
  # routing `{ file = "/abs/path"; }` entries into env_files / header_files so
  # secrets never land in config.toml or /nix/store. Tolerates explicit `null`
  # fields emitted by Home Manager's `programs.mcp.servers.*` submodule.
  translateMcpKv = attrs: let
    entries =
      if builtins.isAttrs attrs
      then attrs
      else {};
  in {
    inline = lib.filterAttrs (_: v: builtins.isString v) entries;
    files = lib.mapAttrs (_: v: toString v.file) (
      lib.filterAttrs (
        _: v:
          builtins.isAttrs v
          && v ? file
          && (builtins.isString v.file || builtins.isPath v.file)
      )
      entries
    );
  };

  translatedMcpServers =
    lib.mapAttrs (
      _: server: let
        envSplit = translateMcpKv (server.env or null);
        headerSplit = translateMcpKv (server.headers or null);
        command = server.command or null;
        args = server.args or null;
        url = server.url or null;
        transport = server.transport or null;
        disabled = server.disabled or null;
        enabledOpt = server.enabled or null;
        icon = server.icon or null;
        toolIcons = server.toolIcons or (server.tool_icons or null);
        enabled =
          if builtins.isBool disabled
          then !disabled
          else if builtins.isBool enabledOpt
          then enabledOpt
          else true;
      in
        {inherit enabled;}
        // lib.optionalAttrs (builtins.isString command && command != "") {
          inherit command;
        }
        // lib.optionalAttrs (builtins.isList args && args != []) {
          inherit args;
        }
        // lib.optionalAttrs (envSplit.inline != {}) {
          env = envSplit.inline;
        }
        // lib.optionalAttrs (envSplit.files != {}) {
          env_files = envSplit.files;
        }
        // lib.optionalAttrs (builtins.isString url && url != "") {
          inherit url;
        }
        // lib.optionalAttrs (builtins.isString transport && transport != "") {
          inherit transport;
        }
        // lib.optionalAttrs (headerSplit.inline != {}) {
          headers = headerSplit.inline;
        }
        // lib.optionalAttrs (headerSplit.files != {}) {
          header_files = headerSplit.files;
        }
        // lib.optionalAttrs (builtins.isString icon) {
          inherit icon;
        }
        // lib.optionalAttrs (builtins.isAttrs toolIcons && toolIcons != {}) {
          tool_icons = toolIcons;
        }
    )
    cfg.mcp.servers;

  # Refused at build time when the program would refuse it at start.
  checkedConfig = settings: let
    toml = tomlFormat.generate "hey-computer-config.toml" settings;
  in
    pkgs.runCommand "hey-computer-config.toml" {} ''
      ${lib.getExe cfg.package} --config ${toml} debug config > /dev/null
      cp ${toml} $out
    '';

  configFile = checkedConfig cfg.settings;

  bridgeSettings =
    {
      read_only = true;
      state_dir = cfg.messengerBridge.stateDir;
    }
    // cfg.messengerBridge.settings;

  bridgeConfigFile = tomlFormat.generate "hey-computer-bridge.toml" bridgeSettings;
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
          agent = {
            inactivity_timeout_secs = 60;
            max_turn_secs = 300;
            max_tool_calls = 20;
          };
          conversation.follow_up_secs = 120;
          permissions.deny = ["sudo" "su" "doas" "pkexec" "run0" "shutdown"];
          instructions.extra = "Prefer metric units. Answer in kilometres and degrees Celsius.";
          context = {
            time = true;
            desktop = true;
          };
          present.max_chars = 300;
        }
      '';
      description = ''
        Contents of {file}`~/.config/hey-computer/config.toml`, see the
        README for every key. The module fills in `instructions.vault`,
        `instructions.memory_file`, `instructions.skills`, `present.viewer` and
        `present.clipboard` from its own options. The file is checked with the
        package's own binary when it is built, so an unknown key fails the
        build.
      '';
    };

    agent = {
      package = mkOption {
        type = types.nullOr types.package;
        default = pkgs.antigravity-cli or null;
        defaultText = literalExpression "pkgs.antigravity-cli or null";
        description = ''
          The package that provides the agent command (`agy` by default), put
          on the agent's PATH. Null expects it elsewhere on the PATH, or an
          absolute path in `settings.agent.command`.
        '';
      };

      waitForKeyring = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Wait for the Freedesktop Secret Service (`org.freedesktop.secrets`
          default collection) to unlock before warm-starting the ACP agent
          (`settings.agent.wait_for_keyring`). Degrades cleanly when no Secret
          Service is present on the session bus.
        '';
      };
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
        Directories searched after the packages, so both the daemon (for
        `context.desktop`) and the agent find the user's own programs (on
        NixOS, `hyprctl` lives here). Directories that do not exist are
        harmless. `/run/wrappers/bin` holds `sudo` on NixOS: it is on the PATH
        so that a privileged command the agent runs itself fails with a clear
        message because of NoNewPrivileges, rather than for want of a program.
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

    memory = {
      file = mkOption {
        type = types.nullOr types.str;
        default = "${config.xdg.configHome}/hey-computer/memory.md";
        defaultText = literalExpression ''"''${config.xdg.configHome}/hey-computer/memory.md"'';
        description = ''
          Optional user-owned Markdown memory file loaded into `AGENTS.md` when
          the agent starts (and refreshed automatically once idle outside active
          conversations if edited on disk). Sets `instructions.memory_file`. A
          missing or empty file is ignored; null disables memory loading. Keep
          this file outside the vault.
        '';
      };

      extraFiles = mkOption {
        type = types.listOf (types.either types.path types.str);
        default = [];
        example = literalExpression ''["''${config.xdg.stateHome}/hey-computer/memory.md"]'';
        description = ''
          Additional Markdown memory files (outside the vault) appended after
          {option}`memory.file` within `instructions.memory_max_chars` — for
          example pairing a read-only `agenix` bootstrap file with a mutable
          state-directory memory file. Sets `instructions.memory_files`.
        '';
      };
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

    openhue = {
      enable = mkEnableOption ''
        Philips Hue control via openhue-cli: puts `openhue` on the agent's PATH
        and links the bundled `openhue` skill when {option}`skills.enable` is set
      '';

      package = mkOption {
        type = types.package;
        default = pkgs.openhue-cli;
        defaultText = literalExpression "pkgs.openhue-cli";
        description = "The openhue-cli package placed on the agent's PATH.";
      };
    };

    recall.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Link the bundled `recall` skill (when {option}`skills.enable` is set)
        so the agent can search past turns via `hey-computer recall` and open
        past conversations or schedules in the viewer via `hey-computer view --detach`.
      '';
    };

    tmux = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Put `tmux` on the daemon/agent PATH (used by `context.desktop` for
          live tmux ambient context) and link the bundled `tmux` skill
          when {option}`skills.enable` is set.
        '';
      };

      package = mkOption {
        type = types.package;
        default = pkgs.tmux;
        defaultText = literalExpression "pkgs.tmux";
        description = "The tmux package placed on the daemon/agent PATH.";
      };
    };

    schedule.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Link the bundled `schedule` skill (when {option}`skills.enable` is set)
        so the agent can create and manage jobs via `hey-computer schedule`.
      '';
    };

    mcp = {
      servers = mkOption {
        type = types.attrsOf (types.attrsOf types.anything);
        default = {};
        example = literalExpression ''
          {
            nixos.command = lib.getExe pkgs.mcp-nixos;
            slack = {
              command = lib.getExe pkgs.slack-mcp-server;
              args = ["--transport" "stdio"];
              env.SLACK_MCP_XOXC_TOKEN.file = "/home/alice/.local/state/slack-mcp/xoxc";
            };
          }
        '';
        description = ''
          MCP servers written to `<agent_dir>/config/mcp_config.json` (mode 0600)
          and gated per turn by `LanePolicy` (using each profile's `mcp_servers`
          allowlist). Accepts the same `env` / `headers` `{ file = "/abs/path"; }`
          form as Home Manager's `programs.mcp.servers` and lowers secret file
          entries to `settings.mcp.<name>.env_files` / `header_files` so
          credentials never enter the Nix store.
        '';
      };

      oauthTokensFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = literalExpression ''"''${config.home.homeDirectory}/.gemini/antigravity-cli/mcp_oauth_tokens.json"'';
        description = ''
          Optional path to an existing `mcp_oauth_tokens.json` (for example from
          an interactive `agy` installation) symlinked into the agent's private
          directory before startup so OAuth-authenticated remote MCP servers
          (such as Linear) share tokens without a second sign-in. Sets
          `settings.agent.mcp_oauth_tokens_file`; a missing file is skipped
          quietly.
        '';
      };
    };

    skills = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Give the agent the bundled `hyprland-desktop` skill, the bundled
          `recall` skill when {option}`recall.enable` is set, the bundled
          `tmux` skill when {option}`tmux.enable` is set, the bundled
          `schedule` skill when {option}`schedule.enable` is set, the Obsidian
          skills when {option}`obsidian.enable` is set, and the bundled
          `openhue` skill when {option}`openhue.enable` is set.
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
        default = null;
        defaultText = literalExpression "null";
        description = ''
          Optional external viewer package installed into `home.packages`. Null
          uses the built-in GTK4 + WebKitGTK 6.0 `hey-computer-viewer` shipped
          in {option}`package`.
        '';
      };

      command = mkOption {
        type = types.listOf types.str;
        default =
          if cfg.viewer.package == null
          then [
            (lib.getExe' cfg.package "hey-computer-viewer")
            "--turn-id"
            "{id}"
            "{file}"
          ]
          else [
            (lib.getExe cfg.viewer.package)
            "{file}"
          ];
        defaultText = literalExpression ''
          ["''${cfg.package}/bin/hey-computer-viewer" "--turn-id" "{id}" "{file}"]
        '';
        description = ''
          Opens a long or rich answer: program and arguments (`{file}` is the
          HTML-disarmed Markdown file and `{id}` is the SQLite turn ID). Started
          in a systemd user scope of its own so the window outlives daemon
          restarts. Empty shows the file's path in a notification instead. With
          the built-in viewer, the Wayland app id is `hey-computer.answer`.
        '';
      };
    };

    speak = {
      enable = mkEnableOption "optional spoken voice answers and audio cues (`present.speak`)";

      defaultOn = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Initial spoken-answer state when no runtime toggle file exists yet at
          `$XDG_STATE_HOME/hey-computer/speak_enabled`. Toggle anytime via
          `hey-computer speak toggle` (Waybar click or voice command).
        '';
      };

      voiceOnly = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Speak responses only when the request arrived via voice (`--voice` /
          `source = "voice"`), keeping typed and CLI requests text-only.
        '';
      };

      package = mkOption {
        type = types.nullOr types.package;
        default = pkgs.piper-tts;
        defaultText = literalExpression "pkgs.piper-tts";
        description = "TTS engine package (`piper`) used when `voiceEn` or `voiceDe` is set.";
      };

      voiceEn = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        example = "/var/lib/piper/en_US-lessac-medium.onnx";
        description = "Path to an English Piper `.onnx` model for auto-generated `command_en`.";
      };

      voiceDe = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        example = "/var/lib/piper/de_DE-thorsten-high.onnx";
        description = "Path to a German Piper `.onnx` model for auto-generated `command_de`.";
      };

      sampleRateEn = mkOption {
        type = types.ints.positive;
        default = 22050;
        description = "Raw PCM sample rate in Hz for `voiceEn` playback via `pw-play`.";
      };

      sampleRateDe = mkOption {
        type = types.ints.positive;
        default = 22050;
        description = "Raw PCM sample rate in Hz for `voiceDe` playback via `pw-play`.";
      };

      command = mkOption {
        type = types.listOf types.str;
        default = [];
        description = ''
          Fallback TTS command fed one sentence on stdin (`{lang}` replaced by
          `en` or `de`).
        '';
      };

      commandEn = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Explicit English TTS command override (fed one sentence on stdin).";
      };

      commandDe = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Explicit German TTS command override (fed one sentence on stdin).";
      };

      mediaDuck = mkOption {
        type = types.enum ["none" "pause" "lower"];
        default = "pause";
        description = "Pause playing MPRIS media (`pause`), lower sink volume (`lower`), or do nothing (`none`) while speaking.";
      };

      earcons = {
        start = mkOption {
          type = types.listOf types.str;
          default = [];
          description = "Optional audio cue command played when a voice turn begins.";
        };
        done = mkOption {
          type = types.listOf types.str;
          default = [];
          description = "Optional audio cue command played when spoken playback completes.";
        };
        cancel = mkOption {
          type = types.listOf types.str;
          default = [];
          description = "Optional audio cue command played when speech or a turn is cancelled.";
        };
      };
    };

    toolIcons = {
      enable = mkEnableOption "sequential Nerd Font tool and MCP server icons in notification titles (`present.tool_icons.enabled`)";

      maxIcons = mkOption {
        type = types.ints.between 1 16;
        default = 8;
        description = "Maximum distinct tool/MCP icons appended to a notification title before `…`.";
      };

      kinds = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Overrides for built-in tool kinds (`execute`, `read`, `edit`, `search`, `fetch`, `mcp_ui`, `vault`); `\"\"` suppresses.";
      };

      commands = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Overrides for `execute` command basenames (e.g. `gh`, `openhue`); `\"\"` suppresses.";
      };
    };

    notifications = {
      enable = mkEnableOption "reactive Hold-and-Enrich incoming desktop notification drafting (`[notifications]`)";

      profile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional profile name for restricted notification drafting turns.";
      };

      maxHoldSecs = mkOption {
        type = types.number;
        default = 60;
        description = "Maximum seconds SwayNC holds a matched notification waiting for enrichment (1..=300).";
      };

      maxTurnSecs = mkOption {
        type = types.number;
        default = 45;
        description = "Hard wall-clock cap per restricted notification drafting turn in seconds.";
      };

      maxToolCalls = mkOption {
        type = types.ints.unsigned;
        default = 0;
        description = "Maximum tool calls per restricted notification drafting turn (`0` = notification text only).";
      };

      draftTtlSecs = mkOption {
        type = types.number;
        default = 43200;
        description = "TTL in seconds for persisted `draft_checkpoints` rows.";
      };

      drop2faCodes = mkOption {
        type = types.bool;
        default = true;
        description = "Suppress incoming notifications containing 2FA/OTP verification codes.";
      };

      respectDnd = mkOption {
        type = types.bool;
        default = true;
        description = "Drop incoming notifications when Do Not Disturb / inhibitor is active.";
      };

      quietStart = mkOption {
        type = types.str;
        default = "";
        description = "Optional local quiet-hours start (`HH:MM`).";
      };

      quietEnd = mkOption {
        type = types.str;
        default = "";
        description = "Optional local quiet-hours end (`HH:MM`).";
      };

      sendersFile = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = "Optional private JSON/TOML file outside `/nix/store` and the vault holding sender allow/deny lists.";
      };

      workdayFile = mkOption {
        type = types.nullOr (types.either types.path types.str);
        default = null;
        description = "Optional workday status JSON file written by `hey-computer-proactive workday`.";
      };

      apps = mkOption {
        type = types.attrsOf (types.attrsOf types.anything);
        default = {};
        description = "Per-channel / app allowlist rules lowered to `settings.notifications.apps`.";
      };
    };

    messengerBridge = {
      enable = mkEnableOption "strictly read-only WhatsApp & Signal messenger bridge (`hey-computer-messenger-bridge`)";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.messenger-bridge;
        defaultText = literalExpression "hey-computer.packages.\${system}.messenger-bridge";
        description = "The `hey-computer-messenger-bridge` package wrapping `baileys-reader` and `signal-cli`.";
      };

      settings = mkOption {
        type = types.submodule {freeformType = tomlFormat.type;};
        default = {};
        description = "Contents of {file}`~/.config/hey-computer/bridge.toml` (`read_only = true` is always enforced).";
      };

      stateDir = mkOption {
        type = types.str;
        default = "${config.xdg.stateHome}/hey-computer-bridge";
        defaultText = literalExpression ''"''${config.xdg.stateHome}/hey-computer-bridge"'';
        description = "Private `0700` state directory holding WhatsApp/Signal linked-device credentials.";
      };

      extraBindReadOnlyPaths = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Additional read-only host paths (such as secret files for `allow_from_file` / `account_file`) bound into the bridge service sandbox.";
      };

      preferOverNotifications = mkOption {
        type = types.bool;
        default = true;
        description = "Allow bridge-originated DM fallback drafts when no recent SwayNC `NotifyHold` handled the message.";
      };

      draftProfile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional profile for bridge-originated DM drafts.";
      };

      selfChat = {
        enable = mkOption {
          type = types.bool;
          default = true;
          description = "Accept verified phone self-chat requests (`settings.messenger.self_chat_enabled`).";
        };

        lane = mkOption {
          type = types.enum ["background" "interactive"];
          default = "background";
          description = "Lane for verified phone self-chat requests (`settings.messenger.self_chat_lane`).";
        };

        profile = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = "Optional profile for verified phone self-chat requests.";
        };
      };

      cooldownSecs = mkOption {
        type = types.number;
        default = 60;
        description = "Minimum seconds between bridge-originated DM drafts per chat.";
      };

      quietStart = mkOption {
        type = types.str;
        default = "";
        description = "Optional local quiet-hours start (`HH:MM`) for messenger DM drafts.";
      };

      quietEnd = mkOption {
        type = types.str;
        default = "";
        description = "Optional local quiet-hours end (`HH:MM`) for messenger DM drafts.";
      };

      service.enable = mkOption {
        type = types.bool;
        default = true;
        description = "Run `hey-computer-messenger-bridge` as a hardened systemd user service.";
      };
    };

    service.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Run the daemon as a systemd user service bound to the graphical session.";
    };
  };

  config = mkIf cfg.enable {
    home.packages =
      [cfg.package]
      ++ lib.optional (cfg.viewer.package != null) cfg.viewer.package
      ++ lib.optional cfg.messengerBridge.enable cfg.messengerBridge.package;

    programs.hey-computer.settings = {
      agent = {
        wait_for_keyring = mkDefault cfg.agent.waitForKeyring;
        mcp_oauth_tokens_file = mkIf (cfg.mcp.oauthTokensFile != null) (mkDefault cfg.mcp.oauthTokensFile);
      };
      mcp = mkIf (translatedMcpServers != {}) (lib.mapAttrs (_: mkDefault) translatedMcpServers);
      instructions = {
        vault = mkIf (cfg.vault != null) (mkDefault cfg.vault);
        memory_file = mkIf (cfg.memory.file != null) (mkDefault cfg.memory.file);
        memory_files = mkIf (cfg.memory.extraFiles != []) (mkDefault (map toString cfg.memory.extraFiles));
        skills = mkIf (skillDirs != []) skillDirs;
      };
      present = {
        viewer = mkIf (cfg.viewer.command != []) (mkDefault (viewerLauncher ++ cfg.viewer.command));
        clipboard = mkDefault [(lib.getExe' pkgs.wl-clipboard "wl-copy")];
        tool_icons = mkIf (cfg.toolIcons.enable || cfg.toolIcons.kinds != {} || cfg.toolIcons.commands != {}) {
          enabled = mkDefault cfg.toolIcons.enable;
          max_icons = mkDefault cfg.toolIcons.maxIcons;
          kinds = mkIf (cfg.toolIcons.kinds != {}) (mkDefault cfg.toolIcons.kinds);
          commands = mkIf (cfg.toolIcons.commands != {}) (mkDefault cfg.toolIcons.commands);
        };
        speak = mkIf cfg.speak.enable {
          enabled = mkDefault cfg.speak.defaultOn;
          voice_only = mkDefault cfg.speak.voiceOnly;
          media_duck = mkDefault cfg.speak.mediaDuck;
          command = mkIf (cfg.speak.command != []) (mkDefault cfg.speak.command);
          command_en = mkIf (speakCommandEn != []) (mkDefault speakCommandEn);
          command_de = mkIf (speakCommandDe != []) (mkDefault speakCommandDe);
          earcon_start = mkIf (cfg.speak.earcons.start != []) (mkDefault cfg.speak.earcons.start);
          earcon_done = mkIf (cfg.speak.earcons.done != []) (mkDefault cfg.speak.earcons.done);
          earcon_cancel = mkIf (cfg.speak.earcons.cancel != []) (mkDefault cfg.speak.earcons.cancel);
        };
      };
      notifications = mkIf cfg.notifications.enable {
        enabled = mkDefault true;
        profile = mkIf (cfg.notifications.profile != null) (mkDefault cfg.notifications.profile);
        max_hold_secs = mkDefault cfg.notifications.maxHoldSecs;
        max_turn_secs = mkDefault cfg.notifications.maxTurnSecs;
        max_tool_calls = mkDefault cfg.notifications.maxToolCalls;
        draft_ttl_secs = mkDefault cfg.notifications.draftTtlSecs;
        drop_2fa_codes = mkDefault cfg.notifications.drop2faCodes;
        respect_dnd = mkDefault cfg.notifications.respectDnd;
        quiet_start = mkIf (cfg.notifications.quietStart != "") (mkDefault cfg.notifications.quietStart);
        quiet_end = mkIf (cfg.notifications.quietEnd != "") (mkDefault cfg.notifications.quietEnd);
        senders_file = mkIf (cfg.notifications.sendersFile != null) (mkDefault (toString cfg.notifications.sendersFile));
        workday_file = mkIf (cfg.notifications.workdayFile != null) (mkDefault (toString cfg.notifications.workdayFile));
        apps = mkIf (cfg.notifications.apps != {}) (lib.mapAttrs (_: mkDefault) cfg.notifications.apps);
      };
      messenger = mkIf cfg.messengerBridge.enable {
        enabled = mkDefault true;
        prefer_over_notifications = mkDefault cfg.messengerBridge.preferOverNotifications;
        draft_profile = mkIf (cfg.messengerBridge.draftProfile != null) (mkDefault cfg.messengerBridge.draftProfile);
        self_chat_enabled = mkDefault cfg.messengerBridge.selfChat.enable;
        self_chat_lane = mkDefault cfg.messengerBridge.selfChat.lane;
        self_chat_profile = mkIf (cfg.messengerBridge.selfChat.profile != null) (mkDefault cfg.messengerBridge.selfChat.profile);
        cooldown_secs = mkDefault cfg.messengerBridge.cooldownSecs;
        quiet_start = mkIf (cfg.messengerBridge.quietStart != "") (mkDefault cfg.messengerBridge.quietStart);
        quiet_end = mkIf (cfg.messengerBridge.quietEnd != "") (mkDefault cfg.messengerBridge.quietEnd);
      };
    };

    xdg.configFile."hey-computer/config.toml".source = configFile;
    xdg.configFile."hey-computer/bridge.toml" = mkIf cfg.messengerBridge.enable {
      source = bridgeConfigFile;
    };

    home.activation.heyComputerMessengerBridgeStateDir = mkIf cfg.messengerBridge.enable (
      lib.hm.dag.entryAfter ["writeBoundary"] ''
        run install -d -m 700 ${lib.escapeShellArg cfg.messengerBridge.stateDir}
      ''
    );

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

    systemd.user.services.hey-computer-messenger-bridge = mkIf (cfg.messengerBridge.enable && cfg.messengerBridge.service.enable) {
      Unit = {
        Description = "hey-computer strictly read-only messenger bridge (WhatsApp & Signal)";
        After = ["graphical-session.target" "network-online.target"];
        PartOf = ["graphical-session.target"];
        X-Restart-Triggers = ["${bridgeConfigFile}"];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.messengerBridge.package} --config ${bridgeConfigFile} daemon";
        Restart = "on-failure";
        RestartSec = 5;
        UMask = "0077";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = "tmpfs";
        BindPaths = ["-${cfg.messengerBridge.stateDir}" "%t"];
        BindReadOnlyPaths = mkIf (cfg.messengerBridge.extraBindReadOnlyPaths != []) (
          map (p: "-${p}") cfg.messengerBridge.extraBindReadOnlyPaths
        );
      };
      Install.WantedBy = ["graphical-session.target"];
    };
  };
}
