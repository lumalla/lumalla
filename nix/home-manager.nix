self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.lumalla;
  system = pkgs.stdenv.hostPlatform.system;
  defaultPackage = self.packages.${system}.default;
  defaultReplPackage = self.packages.${system}.lumalla-repl;

  startLumalla = pkgs.writeShellApplication {
    name = "start_lumalla";
    runtimeInputs = [
      cfg.package
      pkgs.dbus
      pkgs.systemd
    ]
    ++ lib.optionals cfg.enableXwaylandSatellite [pkgs.xwayland-satellite];
    text = builtins.readFile ./start-lumalla.sh;
  };
in {
  options.programs.lumalla = {
    enable = lib.mkEnableOption "Lumalla window manager session helpers";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      defaultText = lib.literalExpression "inputs.lumalla.packages.\${system}.default";
      description = "Lumalla package (compositor + lumalla-config + lumalla-ui).";
    };

    replPackage = lib.mkOption {
      type = lib.types.package;
      default = defaultReplPackage;
      defaultText = lib.literalExpression "inputs.lumalla.packages.\${system}.lumalla-repl";
      description = "Package providing the lumalla-repl helper.";
    };

    configFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Lua config installed to xdg config as lumalla/init.lua.";
    };

    configText = lib.mkOption {
      type = lib.types.nullOr lib.types.lines;
      default = null;
      description = "Inline Lua config installed to xdg config as lumalla/init.lua.";
    };

    enableXwaylandSatellite = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install xwayland-satellite (spawn it from your Lua config if needed).";
    };

    enableRepl = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install the lumalla-repl helper on PATH.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !(cfg.configFile != null && cfg.configText != null);
        message = "programs.lumalla.configFile and programs.lumalla.configText are mutually exclusive.";
      }
    ];

    home.packages =
      [cfg.package startLumalla]
      ++ lib.optionals cfg.enableRepl [cfg.replPackage]
      ++ lib.optionals cfg.enableXwaylandSatellite [pkgs.xwayland-satellite];

    xdg.configFile = lib.mkMerge [
      (lib.mkIf (cfg.configFile != null) {
        "lumalla/init.lua".source = cfg.configFile;
      })
      (lib.mkIf (cfg.configText != null) {
        "lumalla/init.lua".text = cfg.configText;
      })
    ];
  };
}
