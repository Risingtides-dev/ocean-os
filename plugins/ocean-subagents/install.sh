#!/bin/sh
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
config_root=${OCEAN_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/ocean-rs}
plugin_dir=${OCEAN_PLUGINS_DIR:-$config_root/plugins}/ocean-subagents
agents_root=${OCEAN_AGENTS_DIR:-$config_root/agents}
agent_dir=$agents_root/ocean-subagent-worker

case "${1:-install}" in
  install)
    mkdir -p "$plugin_dir" "$agent_dir"
    install -m 0644 "$here/plugin.toml" "$plugin_dir/plugin.toml"
    install -m 0755 "$here/ocean-subagents.py" "$plugin_dir/ocean-subagents.py"
    install -m 0644 "$here/agent/ocean-subagent-worker/agent.toml" "$agent_dir/agent.toml"
    install -m 0644 "$here/agent/ocean-subagent-worker/instructions.md" "$agent_dir/instructions.md"
    printf 'installed ocean-subagents plugin: %s\n' "$plugin_dir"
    printf 'installed fixed worker agent: %s\n' "$agent_dir"
    printf 'restart ocean-daemon to load the plugin\n'
    ;;
  uninstall)
    rm -f "$plugin_dir/plugin.toml" "$plugin_dir/ocean-subagents.py"
    rmdir "$plugin_dir" 2>/dev/null || true
    rm -f "$agent_dir/agent.toml" "$agent_dir/instructions.md"
    rmdir "$agent_dir" 2>/dev/null || true
    printf 'uninstalled ocean-subagents plugin and fixed worker agent\n'
    ;;
  *)
    printf 'usage: %s [install|uninstall]\n' "$0" >&2
    exit 2
    ;;
esac
