{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = {
    self,
    flake-utils,
    naersk,
    nixpkgs,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = (import nixpkgs) {
          inherit system;
        };

        naersk' = pkgs.callPackage naersk {};

        commonBuildArgs = {
          src = self;
          nativeBuildInputs = with pkgs; [
            pkg-config
            seatd.dev
            clang
            libclang
            libdrm.dev
          ];
          buildInputs = with pkgs; [
            seatd
            vulkan-loader
            libdrm
            libgbm
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig:${pkgs.libdrm.dev}/lib/pkgconfig:${pkgs.libgbm}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib:${pkgs.libdrm}/lib:${pkgs.libgbm}/lib";
          LIBRARY_PATH = "${pkgs.libgbm}/lib";
          RUSTFLAGS = "-L ${pkgs.libgbm}/lib -L ${pkgs.libdrm}/lib";
        };
      in {
        packages.default = naersk'.buildPackage commonBuildArgs;

        checks.default = naersk'.buildPackage (commonBuildArgs
          // {
            mode = "test";
            cargoTestOptions = x: (x
              ++ [
                "--workspace"
              ]);
          });

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rust-analyzer
            rustfmt
            openssl
            pkg-config
            lldb
            seatd.dev
            clang
            libclang
            libdrm.dev
          ];
          buildInputs = with pkgs; [
            seatd
            vulkan-loader
            vulkan-validation-layers
            libdrm
            libgbm
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig:${pkgs.libdrm.dev}/lib/pkgconfig:${pkgs.libgbm}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib:${pkgs.libdrm}/lib:${pkgs.libgbm}/lib";
          LIBRARY_PATH = "${pkgs.libgbm}/lib";
          RUSTFLAGS = "-L ${pkgs.libgbm}/lib -L ${pkgs.libdrm}/lib";
          VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
          RUST_LOG = "debug";
        };
      }
    );
}
