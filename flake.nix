{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs: inputs.flake-parts.lib.mkFlake { inherit inputs; } {
    systems = [
      "x86_64-linux"
      "aarch64-linux"
      "x86_64-darwin"
      "aarch64-darwin"
    ];

    debug = true;

    perSystem = { config, self', inputs', pkgs, lib, system, ... }: {
      _module.args.pkgs = import inputs.nixpkgs {
        inherit system;
        overlays = [
          inputs.rust-overlay.overlays.default
        ];
      };

      packages = {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "ouro";
          version = "0.1.4";

          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./src
              ./Cargo.toml
              ./Cargo.lock
            ];
          };

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
        };

        ouro = self'.packages.default;
      };

      devShells = {
        default = pkgs.mkShell rec {
          buildInputs = with pkgs; [
            (rust-bin.stable.latest.default.override (oldAttrs: {
              extensions = (oldAttrs.extensions or []) ++ [
                "rust-analyzer"
              ];
            }))
            pkg-config
            clang
          ];

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath buildInputs}"
          '';
        };
      };
    };
  };
}
