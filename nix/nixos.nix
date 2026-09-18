self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.lumalla;
in {
  options.programs.lumalla = {
    enable = lib.mkEnableOption "Lumalla system integration (seatd + xdg-desktop-portal)";
  };

  config = lib.mkIf cfg.enable {
    services.seatd.enable = true;

    xdg.portal = {
      enable = true;
      extraPortals = with pkgs; [
        xdg-desktop-portal-gnome
        xdg-desktop-portal-gtk
      ];
      config.lumalla = {
        default = [
          "gnome"
          "gtk"
        ];
        "org.freedesktop.impl.portal.ScreenCast" = ["gnome"];
        # GNOME Screenshot needs gnome-shell; prefer gtk for that interface.
        "org.freedesktop.impl.portal.Screenshot" = ["gtk"];
        "org.freedesktop.impl.portal.FileChooser" = ["gtk"];
      };
    };
  };
}
