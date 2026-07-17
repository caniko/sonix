{
  description = "Declarative GoXLR Utility configuration and PipeWire/OBS audio orchestration";

  inputs = {
    rs-harbor.url = "git+https://codeberg.org/caniko/rs-harbor.git?ref=trunk&rev=9bfa8bdb0ecb22d7bc11448665f7fbaebae7a759";
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
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
      buildCache = rs-harbor.lib.mkBuildCachePolicy {
        inherit pkgs;
        buildPackageSet = pkgs.buildPackages;
        sccachePackage = pkgs.buildPackages.sccache;
        cacheRoot = null;
        namespaceScope = "canix-rust";
        namespaceGeneration = 5;
      };
      package = buildCache.withRustCache {
        package = pkgs.rustPlatform.buildRustPackage {
        pname = manifest.name;
        inherit (manifest) version;
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [pkgs.pkg-config];
        meta = {
          description = manifest.description;
          homepage = "https://codeberg.org/caniko/goxlr-nexus";
          mainProgram = "goxlr-nexus";
        };
        };
      };
    in {
      packages = {
        default = package;
        goxlr-nexus = package;
        goxlr-config = package;
      };
      apps = {
        default = flake-utils.lib.mkApp {drv = package;};
        goxlr-nexus = flake-utils.lib.mkApp {drv = package;};
        goxlr-config = {
          type = "app";
          program = "${package}/bin/goxlr-config";
        };
      };
      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          clippy
          rustc
          rustfmt
          pipewire
          pulseaudio
          jq
        ];
      };
      checks.default = package;
      formatter = pkgs.alejandra;
    })
    // {
      nixosModules.default = {
        config,
        lib,
        pkgs,
        ...
      }:
        import ./modules/nixos.nix {
          inherit config lib pkgs self;
        };
      homeModules = {
        default = {
          config,
          lib,
          pkgs,
          ...
        }:
          import ./modules/home-manager.nix {
            inherit config lib pkgs self;
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
      };
      homeManagerModules = self.homeModules;
    };
}
