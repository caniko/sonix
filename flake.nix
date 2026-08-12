{
  description = "Sonix generic PipeWire echo/noise processing with GoXLR routing";

  inputs = {
    rs-harbor.url = "git+https://codefloe.com/caniko/rs-harbor.git?ref=trunk&rev=7fa1c2104dab4e1dbaa1aaa6df84bba815aa282d";
    nixpkgs.follows = "rs-harbor/nixpkgs";
    rust-overlay.follows = "rs-harbor/rust-overlay";
    crane.follows = "rs-harbor/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    rs-harbor,
    nixpkgs,
    flake-utils,
    crane,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = rs-harbor.lib.mkToolchain { inherit pkgs; toolchainProfile = "nightly"; };
      craneLib = toolchain.craneLib;
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
      buildCache = rs-harbor.lib.mkBuildCachePolicy {
        inherit pkgs;
        sccachePackage = rs-harbor.packages.${system}.sccache;
        cacheRoot = null;
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
    in {
      packages = {
        default = package;
        sonix = package;
        goxlr-nexus = package;
        goxlr-config = package;
      };
      apps = {
        default = flake-utils.lib.mkApp {drv = package;};
        sonix = flake-utils.lib.mkApp {drv = package;};
        goxlr-nexus = flake-utils.lib.mkApp {drv = package;};
        goxlr-config = {
          type = "app";
          program = "${package}/bin/goxlr-config";
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
      checks.default = package;
      formatter = pkgs.alejandra;
    })
    // {
      goxlr-nexus = {
        packages = self.packages;
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
