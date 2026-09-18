config="${XDG_CONFIG_HOME:-$HOME/.config}/lumalla/init.lua"
if [ ! -f "$config" ]; then
  echo "error: lumalla config not found at $config" >&2
  exit 1
fi

export XDG_CURRENT_DESKTOP=lumalla
export XDG_SESSION_DESKTOP=lumalla
export XDG_SESSION_TYPE=wayland

# Portals require graphical-session.target (RefuseManualStart). NixOS
# exposes nixos-fake-graphical-session.target to pull it in from TTY sessions.
# WAYLAND_DISPLAY is pushed later from init.lua once the compositor is up.
systemctl --user start nixos-fake-graphical-session.target
systemctl --user import-environment \
  XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE
dbus-update-activation-environment --systemd \
  XDG_CURRENT_DESKTOP XDG_SESSION_DESKTOP XDG_SESSION_TYPE

lumalla -- lumalla-config --config "$config" --repl
status=$?
systemctl --user stop nixos-fake-graphical-session.target || true
exit "$status"
