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
      in {
        packages.default = naersk'.buildPackage {
          src = self;
        };

        checks.default = naersk'.buildPackage {
          src = self;
          mode = "test";
          cargoTestOptions = x: (x
            ++ [
              "--workspace"
            ]);
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rust-analyzer
            rustfmt
            openssl
            pkg-config
          ];
          RUST_LOG = "debug";
        };
      }
    );
}
