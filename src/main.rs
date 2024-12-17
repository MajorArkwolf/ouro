use clap::Parser;
use eyre::{bail, WrapErr};  // Add WrapErr import
use natpmp::{AsyncUdpSocket, NatpmpAsync, Protocol, Response};
use std::net::Ipv4Addr;
use tokio::net::TcpListener;
use std::process::Command;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// VPN Gateway IP (typically 10.2.0.1 for many VPNs)
    #[arg(long, default_value = "10.2.0.1")]
    gateway: Ipv4Addr,

    /// Local service port to forward to
    #[arg(long)]
    local_port: u16,

    /// External port to map (optional, defaults to local_port)
    #[arg(long)]
    external_port: Option<u16>,

    /// Lease time in seconds (default: 60)
    #[arg(long, default_value_t = 60)]
    lease_time: u32,

    /// Refresh interval in seconds (default: 45)
    #[arg(long, default_value_t = 45)]
    refresh_interval: u64,

    /// VPN interface name
    #[arg(long, default_value = "proton0")]
    vpn_interface: String,
}

#[derive(Debug, Clone)]  // Add Clone derive
struct Ports {
    udp: u16,
    tcp: u16,
}

async fn map_ports<S>(
    client: &NatpmpAsync<S>,
    local_port: u16,
    external_port: Option<u16>,
    lease_time: u32,
) -> eyre::Result<Ports>
where
    S: AsyncUdpSocket,
{
    // Default to using local port if no external port specified
    let external_udp_port = external_port.unwrap_or(local_port);
    let external_tcp_port = external_port.unwrap_or(local_port);

    // Send UDP port mapping request
    client
        .send_port_mapping_request(Protocol::UDP, local_port, external_udp_port, lease_time)
        .await?;

    // Await and process UDP response
    let udp_response = client.read_response_or_retry().await?;
    let udp_mapped_port = match udp_response {
        Response::UDP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected UDP response type"),
    };

    // Send TCP port mapping request
    client
        .send_port_mapping_request(Protocol::TCP, local_port, external_tcp_port, lease_time)
        .await?;

    // Await and process TCP response
    let tcp_response = client.read_response_or_retry().await?;
    let tcp_mapped_port = match tcp_response {
        Response::TCP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected TCP response type"),
    };

    Ok(Ports {
        udp: udp_mapped_port,
        tcp: tcp_mapped_port,
    })
}

async fn manage_iptables(interface: &str, new_port: u16, old_port: Option<u16>) -> eyre::Result<()> {
    info!("Managing iptables rules for ports: new={}, old={:?}", new_port, old_port);

    // Add new rule if it doesn't exist
    let new_status = Command::new("sudo")
        .args([
            "iptables",
            "-C",
            "INPUT",
            "-p",
            "tcp",
            "--dport",
            &new_port.to_string(),
            "-j",
            "ACCEPT",
            "-i",
            interface,
        ])
        .status()?;

    if !new_status.success() {  // Fixed parentheses
        Command::new("sudo")
            .args([
                "iptables",
                "-A",
                "INPUT",
                "-p",
                "tcp",
                "--dport",
                &new_port.to_string(),
                "-j",
                "ACCEPT",
                "-i",
                interface,
            ])
            .status()?;
    }

    // Remove old rule if it exists
    if let Some(old_port) = old_port {
        Command::new("sudo")
            .args([
                "iptables",
                "-D",
                "INPUT",
                "-p",
                "tcp",
                "--dport",
                &old_port.to_string(),
                "-j",
                "ACCEPT",
                "-i",
                interface,
            ])
            .status()?;
    }

    Ok(())
}

struct PortForwarder {
    local_port: u16,
    external_ports: Arc<tokio::sync::RwLock<Ports>>,
    shutdown: broadcast::Receiver<()>,
}

impl PortForwarder {
    async fn run_tcp(&mut self) -> eyre::Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, self.external_ports.read().await.tcp)).await
            .wrap_err("Failed to bind TCP listener")?;
        
        info!(local_port = self.local_port, "TCP forwarder starting");
        
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((inbound, addr)) => {
                            info!("New TCP connection from {}", addr);
                            let local_port = self.local_port;
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_tcp_connection(inbound, local_port).await {
                                    error!("TCP connection error: {}", e);
                                }
                            });
                        }
                        Err(e) => warn!("TCP accept error: {}", e),
                    }
                }
                _ = self.shutdown.recv() => {
                    info!("TCP forwarder shutting down");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_tcp_connection(mut inbound: tokio::net::TcpStream, local_port: u16) -> eyre::Result<()> {
        let mut outbound = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, local_port))
            .await
            .wrap_err("Failed to connect to local service")?;

        let (mut ri, mut wi) = inbound.split();
        let (mut ro, mut wo) = outbound.split();

        tokio::select! {
            result = tokio::io::copy(&mut ri, &mut wo) => {
                if let Err(e) = result {
                    warn!("Forward stream error: {}", e);
                }
            }
            result = tokio::io::copy(&mut ro, &mut wi) => {
                if let Err(e) = result {
                    warn!("Reverse stream error: {}", e);
                }
            }
        }
        Ok(())
    }
}

async fn run_port_mapping_service(
    args: Args,
    shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    let client = natpmp::new_tokio_natpmp_with(args.gateway).await?;
    let external_ports = Arc::new(tokio::sync::RwLock::new(Ports { tcp: 0, udp: 0 }));
    let mut current_port = None;
    
    // Initial port mapping
    let ports = map_ports(&client, args.local_port, args.external_port, args.lease_time).await?;
    manage_iptables(&args.vpn_interface, ports.tcp, current_port).await?;  // Use current_port here
    let ports_clone = ports.clone();
    *external_ports.write().await = ports;
    current_port = Some(ports_clone.tcp);
    
    info!(
        local_port = args.local_port,
        external_tcp = ports_clone.tcp,
        external_udp = ports_clone.udp,
        "Initial port mapping successful. Previous port: {:?}", current_port  // Log the port change
    );

    let mut forwarder = PortForwarder {
        local_port: args.local_port,
        external_ports: external_ports.clone(),
        shutdown: shutdown.resubscribe(),
    };

    // Start the forwarder
    tokio::spawn(async move {
        if let Err(e) = forwarder.run_tcp().await {
            error!("TCP forwarder error: {}", e);
        }
    });

    // Port refresh loop
    let mut interval = tokio::time::interval(Duration::from_secs(args.refresh_interval));
    let mut shutdown = shutdown;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                match map_ports(&client, args.local_port, args.external_port, args.lease_time).await {
                    Ok(new_ports) => {
                        let old_port = current_port;  // Store for logging
                        if let Err(e) = manage_iptables(&args.vpn_interface, new_ports.tcp, current_port).await {
                            error!("Failed to update iptables: {}", e);
                        }
                        let new_ports_clone = new_ports.clone();
                        *external_ports.write().await = new_ports;
                        current_port = Some(new_ports_clone.tcp);
                        info!(
                            "Port mapping refreshed successfully. Old port: {:?}, New port: {}",
                            old_port,
                            new_ports_clone.tcp
                        );
                    }
                    Err(e) => error!("Port mapping refresh failed: {}", e),
                }
            }
            _ = shutdown.recv() => {
                info!("Port mapping service shutting down");
                if let Some(port) = current_port {
                    if let Err(e) = manage_iptables(&args.vpn_interface, 0, Some(port)).await {
                        error!("Failed to cleanup iptables: {}", e);
                    }
                }
                break;
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::try_parse()?;
    
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    
    // Set up Ctrl+C handler
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal");
            shutdown_tx_clone.send(()).ok();
        }
    });

    run_port_mapping_service(args, shutdown_rx).await
}
