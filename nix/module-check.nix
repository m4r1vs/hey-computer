# checks.module: the Home Manager module in a minimal configuration. An
# option typo or a broken default fails here, and the generated configuration
# and service are run through the program itself.
{
  pkgs,
  home-manager,
  module,
}: let
  inherit (pkgs) lib;

  # Stand-ins for the unfree agent, Obsidian, openhue, piper, and bridge packages.
  stubAgent = pkgs.writeShellScriptBin "agy" ''echo "agy stub: $*"'';
  stubObsidian = pkgs.writeShellScriptBin "obsidian-cli" ''echo "obsidian-cli stub: $*"'';
  stubOpenhue = pkgs.writeShellScriptBin "openhue" ''echo "openhue stub: $*"'';
  stubPiper = pkgs.writeShellScriptBin "piper" ''cat >/dev/null'';
  stubBridge = pkgs.writeShellScriptBin "hey-computer-bridge" ''echo "bridge stub: $*"'';

  evaluate = settings:
    (home-manager.lib.homeManagerConfiguration {
      inherit pkgs;
      modules = [
        module
        {
          home = {
            username = "alice";
            homeDirectory = "/home/alice";
            stateVersion = "25.11";
          };
          programs.hey-computer =
            lib.recursiveUpdate {
              enable = true;
              agent.package = stubAgent;
              obsidian.package = stubObsidian;
              openhue.package = stubOpenhue;
            }
            settings;
        }
      ];
    }).config;

  full = evaluate {
    vault = "/home/alice/Alice's Notes";
    openhue.enable = true;
    extraPackages = [pkgs.hello];
    settings = {
      agent = {
        max_tool_calls = 15;
        max_repeat_tool_calls = 3;
        max_turn_secs = 180;
      };
      profiles.deep = {
        model = "gemini-4-neon";
        thought_level = "high";
        max_tool_calls = 40;
        mcp_servers = ["demo"];
        thinking_label = "Pondering Hard…";
      };
      profiles.lean = {
        thought_level = "low";
        mcp_servers = [];
      };
      routing = {
        enabled = true;
        fast_profile = "lean";
        deep_profile = "deep";
      };
      scheduler = {
        enabled = true;
        min_interval_secs = 60;
        script_timeout_secs = 30;
        max_jobs = 64;
      };
      tasks = {
        max_concurrent = 1;
        max_queued = 16;
        profile = "deep";
      };
      history = {
        retention_days = 90;
        max_entries = 5000;
        suppressed_days = 7;
      };
      conversation.follow_up_secs = 60;
      instructions.extra = "Prefer metric units.";
      context = {
        time = true;
        desktop = true;
      };
    };
    mcp = {
      oauthTokensFile = "/home/alice/.gemini/antigravity-cli/mcp_oauth_tokens.json";
      servers = {
        demo = {
          command = "/bin/true";
          args = ["--stdio"];
          icon = "";
          toolIcons = {
            "*gmail*" = "";
          };
          env = {
            PLAIN = "ok";
            SECRET.file = "/run/secrets/nonexistent-in-sandbox";
          };
        };
        # Mirrors Home Manager's `programs.mcp.servers.*` submodule shape with explicit nulls.
        hm_like = {
          command = "/bin/true";
          args = null;
          env = null;
          url = null;
          headers = null;
          transport = null;
          disabled = null;
          enabled = true;
        };
      };
    };
    toolIcons = {
      enable = true;
      maxIcons = 10;
    };
    notifications = {
      enable = true;
      triageProfile = "lean";
      triageTimeoutSecs = 3.5;
      profile = "deep";
      allowedMcpTools = ["demo/get_events"];
      maxConcurrent = 4;
      maxQueued = 16;
      maxHoldSecs = 60;
      maxTurnSecs = 45;
      maxToolCalls = 0;
      quietStart = "22:00";
      quietEnd = "08:00";
      sendersFile = "/home/alice/.local/state/hey-computer/senders.json";
      workdayFile = "/home/alice/.local/state/hey-computer/workday.json";
      apps = {
        signal = {
          match_apps = ["signal"];
          allow_senders = ["Bob"];
        };
      };
    };
    messengerBridge = {
      enable = true;
      package = stubBridge;
      draftProfile = "lean";
      selfChat = {
        enable = true;
        lane = "background";
        profile = "deep";
      };
      extraBindReadOnlyPaths = ["/run/secrets/signal-account"];
      settings = {
        whatsapp.enabled = true;
      };
    };
  };

  # A user's own viewer (with viewer.package = null) replaces the module's
  # without evaluating lib.getExe' on null, and extra settings.mcp entries
  # merge alongside programs.hey-computer.mcp.servers.
  custom = evaluate {
    viewer.package = null;
    mcp.servers.nixos.command = "/bin/true";
    settings = {
      mcp.extra_srv = {
        enabled = true;
        command = "/bin/true";
      };
      present = {
        viewer = ["my-viewer" "{file}"];
        clipboard = ["my-clip"];
      };
    };
  };

  bare = evaluate {
    skills.enable = false;
    memory.file = null;
  };

  memorySample = pkgs.writeText "memory.md" "Prefer concise answers.\n";
  memoryExtra = pkgs.writeText "memory-extra.md" "Local state note.\n";
  withMemory = evaluate {
    memory = {
      file = toString memorySample;
      extraFiles = [memoryExtra];
    };
    speak = {
      enable = true;
      defaultOn = false;
      voiceOnly = true;
      package = stubPiper;
      voiceEn = "/var/lib/piper/en.onnx";
      voiceDe = "/var/lib/piper/de.onnx";
      mediaDuck = "pause";
    };
    settings = {
      agent.mcp_servers = [];
      scheduler.jobs.daily-check = {
        schedule = "daily 08:00";
        prompt = "Check calendar.";
        suppress_token = "HEARTBEAT_OK";
      };
    };
  };

  settingsOf = config: config.programs.hey-computer.settings;
  service = full.systemd.user.services.hey-computer.Service;
  bridgeService = full.systemd.user.services.hey-computer-messenger-bridge.Service;

  evaluated = assert lib.assertMsg service.NoNewPrivileges "the service must set NoNewPrivileges";
  assert lib.assertMsg ((settingsOf full).agent.wait_for_keyring == true)
  "agent.waitForKeyring defaults to true";
  assert lib.assertMsg ((settingsOf full).present.tool_icons.enabled == true)
  "toolIcons.enable forwards to present.tool_icons.enabled";
  assert lib.assertMsg ((settingsOf full).mcp.demo.icon == "" && (settingsOf full).mcp.demo.tool_icons."*gmail*" == "")
  "mcp.servers.<name>.{icon,toolIcons} forward into settings.mcp";
  assert lib.assertMsg ((settingsOf full).notifications.enabled == true)
  "notifications.enable forwards to settings.notifications";
  assert lib.assertMsg ((settingsOf full).notifications.triage_profile == "lean" && (settingsOf full).notifications.allowed_mcp_tools == ["demo/get_events"])
  "notifications.triageProfile and allowedMcpTools forward to settings.notifications";
  assert lib.assertMsg ((settingsOf full).notifications.max_concurrent == 4 && (settingsOf full).notifications.max_queued == 16)
  "notifications.maxConcurrent and maxQueued forward to settings.notifications";
  assert lib.assertMsg ((settingsOf full).messenger.enabled == true)
  "messengerBridge.enable forwards to settings.messenger";
  assert lib.assertMsg (
    bridgeService.UMask
    == "0077"
    && bridgeService.NoNewPrivileges
    && bridgeService.PrivateTmp
    && bridgeService.ProtectSystem == "strict"
    && bridgeService.ProtectHome == "tmpfs"
  )
  "messenger bridge service must enable systemd hardening";
  assert lib.assertMsg (lib.hasSuffix "/bin/systemd-run" (lib.head (settingsOf full).present.viewer))
  "the viewer must start in a scope of its own";
  assert lib.assertMsg (lib.any (lib.hasSuffix "/bin/hey-computer-viewer") (settingsOf full).present.viewer)
  "the default viewer uses hey-computer-viewer";
  assert lib.assertMsg (lib.elem "{id}" (settingsOf full).present.viewer)
  "the default viewer passes the turn id placeholder";
  assert lib.assertMsg ((settingsOf withMemory).present.speak.voice_only == true)
  "speak defaults to voice_only = true";
  assert lib.assertMsg (lib.length (settingsOf full).instructions.skills == 10)
  "ten skills with a vault, openhue, and messengerBridge: hyprland-desktop, recall, tmux, schedule, four from obsidian-skills, openhue, and messenger";
  assert lib.assertMsg ((settingsOf full).instructions.memory_file == "/home/alice/.config/hey-computer/memory.md")
  "the module defaults instructions.memory_file to xdg.configHome";
  assert lib.assertMsg (lib.hasSuffix "/bin/wl-copy" (lib.head (settingsOf full).present.clipboard))
  "the module defaults present.clipboard to wl-copy";
  assert lib.assertMsg ((settingsOf custom).present.viewer == ["my-viewer" "{file}"])
  "a user's viewer replaces the default";
  assert lib.assertMsg ((settingsOf custom).mcp ? nixos && (settingsOf custom).mcp ? extra_srv)
  "settings.mcp entries merge alongside programs.hey-computer.mcp.servers";
  assert lib.assertMsg ((settingsOf custom).present.clipboard == ["my-clip"])
  "a user's clipboard command replaces the default";
  assert lib.assertMsg (lib.length (settingsOf custom).instructions.skills == 4)
  "without a vault or openhue only the bundled hyprland-desktop, recall, tmux, and schedule skills";
  assert lib.assertMsg (!((settingsOf bare).instructions or {} ? skills))
  "skills.enable = false links no skills";
  assert lib.assertMsg (!((settingsOf bare).instructions or {} ? memory_file))
  "memory.file = null omits instructions.memory_file";
  assert lib.assertMsg (!(builtins.tryEval (evaluate {vualt = "/tmp";}).programs.hey-computer.settings).success)
  "a misspelt option must fail"; true;

  configFile = full.xdg.configFile."hey-computer/config.toml".source;
  bridgeConfigFile = full.xdg.configFile."hey-computer/bridge.toml".source;
  bareConfig = bare.xdg.configFile."hey-computer/config.toml".source;
  memoryConfig = withMemory.xdg.configFile."hey-computer/config.toml".source;
  unit = full.xdg.configFile."systemd/user/hey-computer.service".source;
  bridgeUnit = full.xdg.configFile."systemd/user/hey-computer-messenger-bridge.service".source;
  package = full.programs.hey-computer.package;
in
  assert evaluated;
    pkgs.runCommand "hey-computer-module-check" {} ''
      set -euo pipefail
      hc=${lib.getExe package}
      test -x "${package}/bin/hey-computer-viewer" || { echo "missing hey-computer-viewer binary" >&2; exit 1; }
      test -f "${package}/share/hey-computer/assets/viewer.css" || { echo "missing viewer.css asset" >&2; exit 1; }

      $hc --config ${configFile} debug config > /dev/null
      $hc --config ${bareConfig} debug config > /dev/null
      $hc --config ${memoryConfig} debug config > /dev/null

      # The agent's directory as the daemon prepares it.
      export XDG_STATE_HOME=$TMPDIR/state
      $hc --config ${configFile} debug prepare
      skills=$XDG_STATE_HOME/hey-computer/agy/config/skills
      for skill in hyprland-desktop recall tmux schedule obsidian-cli obsidian-markdown obsidian-bases json-canvas openhue messenger; do
        test -e "$skills/$skill/SKILL.md" || { echo "missing skill $skill" >&2; exit 1; }
      done
      test -d "$XDG_STATE_HOME/hey-computer/artifacts" || { echo "missing artifacts dir" >&2; exit 1; }
      test -f "$XDG_STATE_HOME/hey-computer/restricted-workspace/AGENTS.md" || { echo "missing restricted-workspace AGENTS.md" >&2; exit 1; }
      grep -q "Alice's Notes" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "Prefer metric units." $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "report the failure plainly" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "untrusted_desktop_context" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md

      # Frozen-snapshot memory loading (including extraFiles) and opt-in ambient context.
      $hc --config ${memoryConfig} debug instructions | grep -q "Prefer concise answers."
      $hc --config ${memoryConfig} debug instructions | grep -q "Local state note."
      $hc --config ${bareConfig} debug instructions | grep -qv "## User memory"
      $hc --config ${bareConfig} debug instructions | grep -qv "untrusted_desktop_context"

      # The service.
      cat ${unit}
      grep -qx 'NoNewPrivileges=true' ${unit}
      grep -qx 'Restart=on-failure' ${unit}
      grep -qx 'WantedBy=graphical-session.target' ${unit}
      grep -q "^ExecStart=$hc daemon" ${unit}
      grep -qx 'read_only = true' ${bridgeConfigFile}
      grep -qx 'UMask=0077' ${bridgeUnit}
      grep -qx 'NoNewPrivileges=true' ${bridgeUnit}
      grep -qx 'PrivateTmp=true' ${bridgeUnit}
      grep -qx 'ProtectSystem=strict' ${bridgeUnit}
      grep -qx 'ProtectHome=tmpfs' ${bridgeUnit}
      grep -q 'BindPaths=' ${bridgeUnit}
      grep -q 'BindReadOnlyPaths=' ${bridgeUnit}
      agent_path=$(sed -n 's/^Environment=PATH=//p' ${unit})
      test -n "$agent_path"

      # The agent's PATH: the shim first, then openhue, agy, bridge, the extras and the basics.
      PATH=$agent_path
      test "$(obsidian help)" = "obsidian-cli stub: help"
      test "$(openhue version)" = "openhue stub: version"
      test "$(agy --version)" = "agy stub: --version"
      test "$(hey-computer-bridge status)" = "bridge stub: status"
      for tool in hey-computer hey-computer-bridge tmux hello jq notify-send playerctl wpctl bash rg curl; do
        command -v $tool > /dev/null || { echo "$tool is not on the agent's PATH" >&2; exit 1; }
      done
      touch $out
    ''
