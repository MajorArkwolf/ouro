# ouro 🐍🌀

A NAT-PMP port forwarding utility for VPN services that automatically renews port mappings.

## Overview

Ouro(boros) sends recurring NAT-PMP requests to your VPN gateway, automatically renewing port mappings even after timeouts. It's designed as an extension for [Maroka-chan's](https://github.com/Maroka-chan) [VPN-Confinement](https://github.com/Maroka-chan/VPN-Confinement) NixOS module.

This module solves the problem of port forwarding with VPN providers like ProtonVPN that [don't allow manual port selection](https://protonvpn.com/support/port-forwarding-manual-setup) by automatically requesting and configuring ports through NAT-PMP

> [!NOTE]
> Primary testing has been conducted with [ProtonVPN](https://protonvpn.com/), but the module should work with other VPN providers that support NAT-PMP.

## Supported Services

Ouro currently supports automatic port configuration for:

| Service | Implementation |
|---------|----------------|
| [Transmission](https://search.nixos.org/options?channel=unstable&from=0&size=50&sort=relevance&type=packages&query=services.transmission.) | Updates the public port using `transmission-remote` |
| [Slskd](https://search.nixos.org/options?channel=unstable&from=0&size=50&sort=relevance&type=packages&query=services.slskd.) | Modifies systemd configuration with new port and restarts the service |


## Installation / Usage

Add ouro to your flake.nix inputs:

```nix
inputs = {
  nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  vpn-confinement.url = "github:Maroka-chan/VPN-Confinement";
  ouro = {
    url = "github:reo101/ouro";
    inputs.nixpkgs.follows = "nixpkgs";
  };
};
```

In your existing VPN-Confinement configuration, import the ouro module and add the necessary configuration like so:

> [!IMPORTANT]
> You must enable either Transmission OR Slskd - enabling both simultaneously is not supported.

For Transmission:
```nix
imports = [
  inputs.ouro.nixosModules.default
];

vpnNamespaces = { 
  prtr = {
    enable = true;
    wireguardConfigFile = config.age.secrets."proton1_config".path;

    ouro = {
      enable = true;            # default = false
      gateway = "10.2.0.1";     # default = "10.2.0.1"
      interface = "prtr0";      # default = (vpnNamespace name above) + "0"

      # Configure Transmission support
      transmission = {
        enable = true;          # default = false

        # TODO
        # rpc_file = config.age.secrets."ouro_transmission_creds";

        RPC_USER="USER";        # required
        RPC_PASS="PASS";        # required
      };
    };
  };
};
```

For Slskd:

```nix
imports = [
  inputs.ouro.nixosModules.default
];

vpnNamespaces = { 
  prsl = {
    enable = true;
    wireguardConfigFile = config.age.secrets."proton2_config".path;

    ouro = {
      enable = true;            # default = false
      gateway = "10.2.0.1";     # default = "10.2.0.1"
      interface = "prsl0";      # default = (vpnNamespace name above) + "0"

      # Configure Slskd support
      slskd.enable = true;      # default = false
    };
  };
};
```

## How It Works

When ouro receives a new port from the NAT-PMP request:

- For Transmission: Uses `transmission-remote` to update the public-facing port
- For Slskd: Stops the service, adds `SLSKD_SLSK_LISTEN_PORT` environment variable with the new port, then restarts the service

## TODOs

- [ ] add option to allow loading Transmission RPC credentials from a file
- [ ] add assertion that only one of `slskd` and `transmission` are enabled
- [ ] add toggle for `RUST_LOG = debug`

## Contribution

Contributions are welcome! Feel free to:

- Submit [Pull Requests](https://github.com/reo101/ouro/pulls)
- Open [Issues](https://github.com/reo101/ouro/issues) for bugs or feature requests

We appreciate your suggestions and improvements to make ouro better.
