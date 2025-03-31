{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} ({config, ...}: let
      flakePartsConfig = config;
    in {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      debug = true;

      flake = {
        nixosModules = {
          ouro = {
            config,
            lib,
            pkgs,
            ...
          }: {
            options = let
              inherit (lib) types;
            in {
              vpnNamespaces = lib.mkOption {
                type = types.attrsOf (types.submodule (vpnNamespaceModule @ {name, ...}: {
                  options = {
                    ouro = lib.mkOption {
                      default = {};
                      type = types.submodule (ouroSubmodule: {
                        options = {
                          enable = lib.mkEnableOption "ouro module subsystem for vpnNamespaces";
                          gateway = lib.mkOption {
                            type = types.str;
                            default = "10.2.0.1";
                            description = ''
                              Gateway IP Address of VPN Subnet.
                              Use `sudo ip netns exec ${name} bash -c "ip a"` to find out.
                              For Proton, default is `10.2.0.1`
                            '';
                          };
                          interface = lib.mkOption {
                            type = types.str;
                            default = "${name}0";
                            description = ''
                              Interface of the VPN connection in the confinement.
                              Use `sudo ip netns exec ${name} bash -c "ip a"` to find out.
                              Usually should be `${name}0`, but do double-check it.
                            '';
                          };
                          slskd = lib.mkOption {
                            default = {};
                            type = types.submodule {
                              options = {
                                enable = lib.mkEnableOption "Soulseek support";
                              };
                            };
                          };
                        };
                      });
                    };
                  };
                }));
              };
            };
            config = {
              # ids.uids.ouro = 0;
              # ids.gids.ouro = 0;
              # users.groups.ouro = {
              #   group = config.ids.gids.ouro;
              # };
              # users.users.ouro = {
              #   group = "ouro";
              #   uid = config.ids.uids.ouro;
              #   description = "Ouro user";
              #   home = "/var/lib/ouro";
              # };
              systemd.services = lib.pipe config.vpnNamespaces [
                # name = { ..., ouro = { STUFF }, ... }
                (lib.mapAttrs (vpnNamespace: vpnNamespaceConfig: vpnNamespaceConfig.ouro))
                # name = { STUFF }
                (lib.filterAttrs (vpnNamespace: ouroConfig: ouroConfig.enable))
                # name = { enable = true, STUFF }
                (lib.mapAttrs' (vpnNamespace: ouroConfig:
                  lib.nameValuePair "ouro-${vpnNamespace}" {
                    description = ''
                      Ouro stuff for ${vpnNamespace}, TODO
                    '';
                    wantedBy = ["multi-user.target"];
                    after = [ "slskd.service" "${vpnNamespace}.service" ];
                    environment = {
                      "RUST_LOG" = "debug";
                    };
                    serviceConfig = let
                      inherit
                        (flakePartsConfig.allSystems.${pkgs.hostPlatform.system}.packages)
                        ouro
                        ;
                      inherit
                        (ouroConfig)
                        gateway
                        interface
                        ;
                    in {
                      # TODO(74k1): more systemd module stuff
                      # User = "ouro";
                      # Group = "ouro";
                      ExecStart = ''
                        ${lib.getExe' pkgs.iproute2 "ip"} netns exec ${vpnNamespace} ${lib.getExe pkgs.bashNonInteractive} -c "${lib.getExe ouro} --gateway ${gateway} --vpn-interface ${interface} slskd"
                      '';
                      Restart = "always";
                    };
                    vpnConfinement = {
                      enable = true;
                      inherit vpnNamespace;
                    };
                  }))
              ];
            };
          };
          default = config.flake.nixosModules.ouro;
        };
      };

      perSystem = {
        config,
        self',
        inputs',
        pkgs,
        lib,
        system,
        ...
      }: {
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

            meta = {
              mainProgram = "ouro";
            };
          };

          ouro = self'.packages.default;
        };

        devShells = {
          default = pkgs.mkShell rec {
            buildInputs = with pkgs; [
              (rust-bin.stable.latest.default.override (oldAttrs: {
                extensions =
                  (oldAttrs.extensions or [])
                  ++ [
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
    });
}
