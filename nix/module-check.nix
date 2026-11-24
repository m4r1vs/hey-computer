# checks.module: the Home Manager module in a minimal configuration. An
# option typo or a broken default fails here, and the generated configuration
# and service are run through the program itself.
{
  pkgs,
  home-manager,
  module,
}: let
  inherit (pkgs) lib;

  # Stand-ins for the unfree agent, Obsidian, and openhue packages.
  stubAgent = pkgs.writeShellScriptBin "agy" ''echo "agy stub: $*"'';
  stubObsidian = pkgs.writeShellScriptBin "obsidian-cli" ''echo "obsidian-cli stub: $*"'';
  stubOpenhue = pkgs.writeShellScriptBin "openhue" ''echo "openhue stub: $*"'';

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
        max_turn_secs = 180;
      };
      conversation.follow_up_secs = 60;
      instructions.extra = "Prefer metric units.";
      context = {
        time = true;
        desktop = true;
      };
    };
  };

  # A user's own viewer replaces the module's instead of being appended to
  # it, and without a vault there are no Obsidian skills and no shim.
  custom = evaluate {
    settings.present.viewer = ["my-viewer" "{file}"];
  };

  bare = evaluate {
    skills.enable = false;
    memory.file = null;
  };

  memorySample = pkgs.writeText "memory.md" "Prefer concise answers.\n";
  withMemory = evaluate {
    memory.file = toString memorySample;
  };

  settingsOf = config: config.programs.hey-computer.settings;
  service = full.systemd.user.services.hey-computer.Service;

  evaluated = assert lib.assertMsg service.NoNewPrivileges "the service must set NoNewPrivileges";
  assert lib.assertMsg (lib.hasSuffix "/bin/systemd-run" (lib.head (settingsOf full).present.viewer))
  "the viewer must start in a scope of its own";
  assert lib.assertMsg (lib.elem "--name=hey-computer.answer" (settingsOf full).present.viewer)
  "the default viewer sets the app id";
  assert lib.assertMsg (lib.length (settingsOf full).instructions.skills == 6)
  "six skills with a vault and openhue: hyprland-desktop, four from obsidian-skills, and openhue";
  assert lib.assertMsg ((settingsOf full).instructions.memory_file == "/home/alice/.config/hey-computer/memory.md")
  "the module defaults instructions.memory_file to xdg.configHome";
  assert lib.assertMsg ((settingsOf custom).present.viewer == ["my-viewer" "{file}"])
  "a user's viewer replaces the default";
  assert lib.assertMsg (lib.length (settingsOf custom).instructions.skills == 1)
  "without a vault or openhue only the bundled hyprland-desktop skill";
  assert lib.assertMsg (!((settingsOf bare).instructions or {} ? skills))
  "skills.enable = false links no skills";
  assert lib.assertMsg (!((settingsOf bare).instructions or {} ? memory_file))
  "memory.file = null omits instructions.memory_file";
  assert lib.assertMsg (!(builtins.tryEval (evaluate {vualt = "/tmp";}).programs.hey-computer.settings).success)
  "a misspelt option must fail"; true;

  configFile = full.xdg.configFile."hey-computer/config.toml".source;
  bareConfig = bare.xdg.configFile."hey-computer/config.toml".source;
  memoryConfig = withMemory.xdg.configFile."hey-computer/config.toml".source;
  unit = full.xdg.configFile."systemd/user/hey-computer.service".source;
  package = full.programs.hey-computer.package;
in
  assert evaluated;
    pkgs.runCommand "hey-computer-module-check" {} ''
      set -euo pipefail
      hc=${lib.getExe package}

      $hc --config ${configFile} debug config > /dev/null
      $hc --config ${bareConfig} debug config > /dev/null
      $hc --config ${memoryConfig} debug config > /dev/null

      # The agent's directory as the daemon prepares it.
      export XDG_STATE_HOME=$TMPDIR/state
      $hc --config ${configFile} debug prepare
      skills=$XDG_STATE_HOME/hey-computer/agy/config/skills
      for skill in hyprland-desktop obsidian-cli obsidian-markdown obsidian-bases json-canvas openhue; do
        test -e "$skills/$skill/SKILL.md" || { echo "missing skill $skill" >&2; exit 1; }
      done
      grep -q "Alice's Notes" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "Prefer metric units." $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "report the failure plainly" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md
      grep -q "untrusted_desktop_context" $XDG_STATE_HOME/hey-computer/workspace/AGENTS.md

      # Frozen-snapshot memory loading and opt-in ambient context.
      $hc --config ${memoryConfig} debug instructions | grep -q "Prefer concise answers."
      $hc --config ${bareConfig} debug instructions | grep -qv "## User memory"
      $hc --config ${bareConfig} debug instructions | grep -qv "untrusted_desktop_context"

      # The service.
      cat ${unit}
      grep -qx 'NoNewPrivileges=true' ${unit}
      grep -qx 'Restart=on-failure' ${unit}
      grep -qx 'WantedBy=graphical-session.target' ${unit}
      grep -q "^ExecStart=$hc daemon" ${unit}
      agent_path=$(sed -n 's/^Environment=PATH=//p' ${unit})
      test -n "$agent_path"

      # The agent's PATH: the shim first, then openhue, agy, the extras and the basics.
      PATH=$agent_path
      test "$(obsidian help)" = "obsidian-cli stub: help"
      test "$(openhue version)" = "openhue stub: version"
      test "$(agy --version)" = "agy stub: --version"
      for tool in hello jq notify-send playerctl wpctl bash rg curl; do
        command -v $tool > /dev/null || { echo "$tool is not on the agent's PATH" >&2; exit 1; }
      done
      touch $out
    ''
