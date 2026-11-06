{
  description = "Sonix generic PipeWire echo/noise processing with GoXLR routing";

  inputs = {
    harbor-rs.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=05cc4f162b55fa904b687db1821e2463fa813e50";
    nixpkgs.follows = "harbor-rs/nixpkgs";
    rust-overlay.follows = "harbor-rs/rust-overlay";
    crane.follows = "harbor-rs/crane";
    flake-utils.url = "github:numtide/flake-utils";
    # Test the actual Home Manager module, not a parallel set of artifact values.
    home-manager = {
      url = "github:caniko/home-manager/e1bb806f34a39483ca5f4b153aef6652e4a6adf3";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    harbor-rs,
    nixpkgs,
    flake-utils,
    rust-overlay,
    home-manager,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = harbor-rs.lib.mkToolchain {
        inherit pkgs;
        toolchainProfile = "nightly";
      };
      inherit (toolchain) craneLib;
      src = pkgs.lib.cleanSourceWith {
        src = pkgs.lib.cleanSource ./.;
        filter = path: type:
          pkgs.lib.cleanSourceFilter path type
          && !(pkgs.lib.hasInfix "/graphify-out" (toString path));
      };
      spaBindgenHeader = pkgs.writeText "spa-bindgen.h" ''
        #include <stdint.h>

        static const uint32_t SPA_ID_INVALID = 0xffffffffu;
        static const uint32_t PW_ID_ANY = 0xffffffffu;
      '';
      commonArgs = {
        inherit src;
        strictDeps = true;
        nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.libclang];
        buildInputs = [pkgs.pipewire.dev];
        PKG_CONFIG_PATH = "${pkgs.pipewire.dev}/lib/pkgconfig";
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include -include ${spaBindgenHeader}";
      };
      buildCache = harbor-rs.lib.mkBuildCachePolicy {
        inherit pkgs;
        sccachePackage = harbor-rs.packages.${system}.sccache;
        cacheRoot = "/build/sccache";
        namespaceScope = "canix-rust";
        namespaceGeneration = 5;
      };
      cargoArtifacts = buildCache.withRustCache {
        package = craneLib.buildDepsOnly commonArgs;
      };
      package = buildCache.withRustCache {
        package = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--locked --bins";
          });
      };
      formatter = pkgs.writeShellApplication {
        name = "treefmt";
        runtimeInputs = [pkgs.treefmt pkgs.alejandra toolchain.rustToolchain];
        text = ''
          exec treefmt --config-file ${./treefmt.toml} "$@"
        '';
      };
    in {
      packages =
        {
          default = package;
          sonix = package;
          goxlr-nexus = package;
          goxlr-config = package;
        }
        // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
          # Kept separate from flake checks: this gate needs a KVM-enabled runner.
          goxlr-systemd-vm = import ./tests/goxlr-systemd-recovery.nix {
            inherit (pkgs) lib;
            inherit pkgs;
          };
        };
      apps =
        {
          default = flake-utils.lib.mkApp {drv = package;};
          sonix = flake-utils.lib.mkApp {drv = package;};
          goxlr-nexus = flake-utils.lib.mkApp {drv = package;};
          goxlr-config = {
            type = "app";
            program = "${package}/bin/goxlr-config";
          };
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Simit prebuild publication on trunk (secret ATTIC_TOKEN). Linux
          # outputs only: the package canix consumes, pushed to the public
          # canix cache.
          push-flake-inputs = harbor-rs.lib.mkAtticPush {
            inherit pkgs;
            flake = ".";
            paths = [package];
            adapter = harbor-rs.lib.mkAdapter {
              attic = {
                endpoint = "https://attic.candee.baby";
                cache = "canix";
                tokenEnvVar = "ATTIC_TOKEN";
              };
            };
          };
        };
      devShells.default = craneLib.devShell {
        packages = with pkgs; [
          pipewire
          pulseaudio
          llvmPackages.libclang
          pkg-config
          jq
        ];
        PKG_CONFIG_PATH = "${pkgs.pipewire.dev}/lib/pkgconfig";
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include -include ${spaBindgenHeader}";
      };
      checks =
        {
          default = package;
          # Artifact contracts for the GoXLR routing units and WirePlumber rule.
          # Evaluation-only: these assert the invariants behind the SplitPCM
          # deadlock and the unowned-daemon outages, which need real hardware to
          # reproduce at runtime.
          module-contracts = let
            contracts = import ./tests/goxlr-artifact-contracts.nix {
              inherit (pkgs) lib;
              inherit pkgs;
            };
          in
            builtins.deepSeq contracts (pkgs.runCommand "sonix-module-contracts" {} ''
              touch "$out"
            '');
          processing = craneLib.cargoTest (commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--locked -p nexus-audio-processing --lib";
            });
          formatter =
            pkgs.runCommand "sonix-formatter-check" {
              nativeBuildInputs = [pkgs.python3];
            } ''
              python ${./tests/check_formatter.py} ${formatter}/bin/treefmt
              touch "$out"
            '';
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          goxlr-watchdog-contracts = import ./tests/goxlr-watchdog.nix {
            inherit (pkgs) lib;
            inherit pkgs;
          };
          goxlr-nixos-module-contracts = let
            contracts = import ./tests/goxlr-nixos-module-contracts.nix {
              inherit (pkgs) lib;
              inherit pkgs self;
            };
          in
            builtins.deepSeq contracts (pkgs.runCommand "sonix-goxlr-nixos-module-contracts" {} ''
              touch "$out"
            '');
          goxlr-home-manager-module-contracts = let
            contracts = import ./tests/goxlr-home-manager-contracts.nix {
              inherit (pkgs) lib;
              inherit pkgs self;
              homeManager = home-manager;
            };
          in
            builtins.deepSeq contracts (pkgs.runCommand "sonix-goxlr-home-manager-contracts" {} ''
              touch "$out"
            '');
        };
      inherit formatter;
    })
    // {
      goxlr-nexus = {
        inherit (self) packages;
        homeModules.default = self.homeModules.goxlr-nexus;
      };
      nixosModules.default = {
        config,
        lib,
        pkgs,
        ...
      }: {
        imports = [
          (import ./modules/nixos/sonix.nix {
            inherit config lib pkgs self;
          })
          (import ./modules/nixos.nix {
            inherit config lib pkgs self;
          })
        ];
      };
      nixosModules.sonix = import ./modules/nixos/sonix.nix;
      homeModules = {
        sonix = {
          config,
          lib,
          pkgs,
          ...
        }:
          import ./modules/home-manager/sonix.nix {
            inherit config lib pkgs self;
          };
        goxlr-nexus = {
          config,
          lib,
          pkgs,
          ...
        }: {
          imports = [
            (import ./modules/home-manager/sonix.nix {
              inherit config lib pkgs self;
            })
            (import ./modules/home-manager.nix {
              inherit config lib pkgs self;
            })
          ];
        };
        goxlr-utility = {
          config,
          lib,
          pkgs,
          ...
        }:
          import ./modules/home-manager/goxlr-utility.nix {
            inherit config lib pkgs self;
          };
        default = self.homeModules.sonix;
      };
      homeManagerModules = self.homeModules;
    };
}
