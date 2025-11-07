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
          ];
          buildInputs = with pkgs; [
            seatd
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
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
          ];
          buildInputs = with pkgs; [
            seatd
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          RUST_LOG = "debug";
        };
      }
    );
}
