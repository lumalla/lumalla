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
            libinput.dev
            libxkbcommon.dev
            systemd.dev
            clang
            libclang
            libdrm.dev
          ];
          buildInputs = with pkgs; [
            seatd
            libinput
            libxkbcommon
            systemd
            vulkan-loader
            libdrm
            libgbm
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig:${pkgs.libinput.dev}/lib/pkgconfig:${pkgs.libxkbcommon.dev}/lib/pkgconfig:${pkgs.systemd.dev}/lib/pkgconfig:${pkgs.libdrm.dev}/lib/pkgconfig:${pkgs.libgbm}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib:${pkgs.libdrm}/lib:${pkgs.libgbm}/lib:${pkgs.libinput}/lib:${pkgs.libxkbcommon}/lib:${pkgs.systemd}/lib";
          # libgbm/libdrm for link search; force libvulkan into NEEDED/RUNPATH so
          # ash::Entry::load() works without the flake shell's LD_LIBRARY_PATH.
          LIBRARY_PATH = "${pkgs.libgbm}/lib:${pkgs.vulkan-loader}/lib";
          RUSTFLAGS = "-L ${pkgs.libgbm}/lib -L ${pkgs.libdrm}/lib -L ${pkgs.vulkan-loader}/lib -C link-arg=-Wl,--push-state,--no-as-needed -C link-arg=-lvulkan -C link-arg=-Wl,--pop-state";
        };

        cargoRuntimeInputs = with pkgs; [
          cargo
          rustc
          pkg-config
          clang
          libclang
        ];

        runLocal = pkgs.writeShellApplication {
          name = "lumalla-run-local";
          runtimeInputs = cargoRuntimeInputs;
          text = builtins.readFile ./run-local.sh;
        };

        cpuProfiling = pkgs.writeShellApplication {
          name = "lumalla-cpu-profiling";
          runtimeInputs = cargoRuntimeInputs ++ (with pkgs; [
            perf
            hotspot
          ]);
          text = builtins.readFile ./cpu-profiling.sh;
        };

        memProfiling = pkgs.writeShellApplication {
          name = "lumalla-mem-profiling";
          runtimeInputs = cargoRuntimeInputs ++ (with pkgs; [
            heaptrack
          ]);
          text = builtins.readFile ./mem-profiling.sh;
        };
      in {
        packages.default = naersk'.buildPackage commonBuildArgs;
        packages.run-local = runLocal;
        packages.cpu-profiling = cpuProfiling;
        packages.mem-profiling = memProfiling;

        apps.run-local = {
          type = "app";
          program = "${runLocal}/bin/lumalla-run-local";
        };

        apps.cpu-profiling = {
          type = "app";
          program = "${cpuProfiling}/bin/lumalla-cpu-profiling";
        };

        apps.mem-profiling = {
          type = "app";
          program = "${memProfiling}/bin/lumalla-mem-profiling";
        };

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
            libinput.dev
            libxkbcommon.dev
            systemd.dev
            clang
            libclang
            libdrm.dev
            perf
            hotspot
            heaptrack
            xwayland-satellite
            runLocal
            cpuProfiling
            memProfiling
          ];
          buildInputs = with pkgs; [
            seatd
            libinput
            libxkbcommon
            systemd
            vulkan-loader
            vulkan-validation-layers
            libdrm
            libgbm
          ];
          PKG_CONFIG_PATH = "${pkgs.seatd.dev}/lib/pkgconfig:${pkgs.libinput.dev}/lib/pkgconfig:${pkgs.libxkbcommon.dev}/lib/pkgconfig:${pkgs.systemd.dev}/lib/pkgconfig:${pkgs.libdrm.dev}/lib/pkgconfig:${pkgs.libgbm}/lib/pkgconfig";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib:${pkgs.libdrm}/lib:${pkgs.libgbm}/lib:${pkgs.libinput}/lib:${pkgs.libxkbcommon}/lib:${pkgs.systemd}/lib";
          LIBRARY_PATH = "${pkgs.libgbm}/lib:${pkgs.vulkan-loader}/lib";
          RUSTFLAGS = "-L ${pkgs.libgbm}/lib -L ${pkgs.libdrm}/lib -L ${pkgs.vulkan-loader}/lib -C link-arg=-Wl,--push-state,--no-as-needed -C link-arg=-lvulkan -C link-arg=-Wl,--pop-state";
          VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
          RUST_LOG = "debug";
        };
      }
    );
}
