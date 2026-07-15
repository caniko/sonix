{
  description = "Declarative GoXLR Utility configuration and PipeWire/OBS audio orchestration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
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
