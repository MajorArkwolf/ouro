use clap::Parser;
use eyre::{bail, WrapErr};
use natpmp::{AsyncUdpSocket, NatpmpAsync, Protocol, Response};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{error, info, debug};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// VPN Gateway IP (typically 10.2.0.1 for many VPNs)
    #[arg(long, default_value = "10.2.0.1")]
    gateway: Ipv4Addr,

    /// VPN interface name
    #[arg(long, default_value = "proton0")]
    vpn_interface: String,

    /// Service to manage (slskd or transmission)
    #[arg(long)]
    service: ServiceType,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum ServiceType {
    Slskd,
    Transmission,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Ports {
    udp: u16,
    tcp: u16,
}

#[derive(Debug)]
enum PortManager {
    Slskd(SlskdManager),
    Transmission(TransmissionManager),
}

impl PortManager {
    async fn apply_ports(&mut self, interface: &str, ports: &Ports, old_ports: Option<&Ports>) -> eyre::Result<()> {
        // Don't do anything if ports haven't changed
        if Some(ports) == old_ports {
            debug!("Ports unchanged, skipping update");
            return Ok(());
        }

        match self {
            Self::Slskd(m) => {
                manage_firewall(interface, Some(ports), old_ports).await?;
                m.update_service(ports).await?;
                m.current_ports = Some(ports.clone());
            }
            Self::Transmission(m) => {
                manage_firewall(interface, Some(ports), old_ports).await?;
                m.update_service(ports).await?;
                m.current_ports = Some(ports.clone());
            }
        }
        Ok(())
    }

    async fn stop(&mut self) -> eyre::Result<()> {
        match self {
            Self::Slskd(_) => {
                Command::new("systemctl")
                    .args(["stop", "slskd.service"])
                    .status()
                    .await?;
            }
            Self::Transmission(_) => {
                // Transmission doesn't need explicit stopping
            }
        }
        Ok(())
    }

    fn current_ports(&self) -> Option<Ports> {
        match self {
            Self::Slskd(m) => m.current_ports.clone(),
            Self::Transmission(m) => m.current_ports.clone(),
        }
    }
}

#[derive(Debug)]
struct SlskdManager {
    current_ports: Option<Ports>,
}

impl SlskdManager {
    fn new() -> Self {
        Self { current_ports: None }
    }

    async fn update_service(&mut self, ports: &Ports) -> eyre::Result<()> {
        info!("Configuring slskd with ports TCP={}, UDP={}", ports.tcp, ports.udp);
        
        let override_dir = "/run/systemd/system/slskd.service.d";
        fs::create_dir_all(override_dir).await?;

        fs::write(
            format!("{}/override.conf", override_dir),
            format!("[Service]\nEnvironment=SLSKD_SLSK_LISTEN_PORT={}\n", ports.tcp)
        ).await?;

        Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .await?;

        Command::new("systemctl")
            .args(["restart", "slskd.service"])
            .status()
            .await?;

        self.current_ports = Some(ports.clone());
        Ok(())
    }
}

#[derive(Debug)]
struct TransmissionManager {
    current_ports: Option<Ports>,
}

impl TransmissionManager {
    fn new() -> Self {
        Self { current_ports: None }
    }

    async fn update_service(&mut self, ports: &Ports) -> eyre::Result<()> {
        info!("Configuring transmission with ports TCP={}, UDP={}", ports.tcp, ports.udp);
        
        Command::new("transmission-remote")
            .args(["--port", &ports.tcp.to_string()])
            .status()
            .await?;
            
        Ok(())
    }
}

async fn manage_firewall(interface: &str, new_ports: Option<&Ports>, old_ports: Option<&Ports>) -> eyre::Result<()> {
    // Skip if no changes needed
    if new_ports == old_ports {
        return Ok(());
    }

    info!("Updating firewall rules");

    // Remove old rules if they exist - don't error if they don't
    if let Some(old) = old_ports {
        for (proto, port) in [("tcp", old.tcp), ("udp", old.udp)] {
            let _ = Command::new("iptables")
                .args([
                    "-D", "INPUT",
                    "-p", proto,
                    "--dport", &port.to_string(),
                    "-j", "ACCEPT",
                    "-i", interface
                ])
                .output()
                .await?;
        }
    }

    // Add new rules only if they don't exist
    if let Some(new) = new_ports {
        for (proto, port) in [("tcp", new.tcp), ("udp", new.udp)] {
            // Use output() instead of status() to suppress stderr
            let exists = Command::new("iptables")
                .args([
                    "-C", "INPUT",
                    "-p", proto,
                    "--dport", &port.to_string(),
                    "-j", "ACCEPT",
                    "-i", interface
                ])
                .output()
                .await?
                .status
                .success();

            if !exists {
                let result = Command::new("iptables")
                    .args([
                        "-A", "INPUT",
                        "-p", proto,
                        "--dport", &port.to_string(),
                        "-j", "ACCEPT",
                        "-i", interface
                    ])
                    .output()
                    .await?;
                
                if result.status.success() {
                    info!("Added firewall rule for {}/{}", proto, port);
                } else {
                    error!("Failed to add firewall rule for {}/{}", proto, port);
                }
            }
        }
    }

    Ok(())
}

async fn run_service(
    args: Args,
    mut manager: PortManager,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    let client = natpmp::new_tokio_natpmp_with(args.gateway).await?;
    let mut interval = tokio::time::interval(Duration::from_secs(45));

    // Initial port mapping
    let ports = map_ports(&client).await?;
    manager.apply_ports(&args.vpn_interface, &ports, None).await?;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Just refresh the NAT-PMP mapping, don't reconfigure unless ports change
                if let Err(e) = map_ports(&client).await {
                    error!("Port mapping refresh failed: {}", e);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
            _ = shutdown.recv() => {
                info!("Shutting down...");
                if let Some(ports) = manager.current_ports() {
                    manage_firewall(&args.vpn_interface, None, Some(&ports)).await?;
                }
                manager.stop().await?;
                break;
            }
        }
    }
    Ok(())
}

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(60);
const LOCAL_PORT: u16 = 1;  // Request port 1, NAT-PMP will assign random port
const PORT_MAPPING_LIFETIME: u32 = 60;  // 60 second lifetime, will be refreshed

async fn map_ports<S>(client: &NatpmpAsync<S>) -> eyre::Result<Ports>
where
    S: AsyncUdpSocket,
{
    let mut retry_count = 0;
    
    while retry_count < MAX_RETRIES {
        let result = try_map_ports(client).await;
        
        match result {
            Ok(ports) => return Ok(ports),
            Err(e) => {
                error!("Port mapping attempt {} failed: {}", retry_count + 1, e);
                if retry_count < MAX_RETRIES - 1 {
                    info!("Retrying in {} seconds...", RETRY_DELAY.as_secs());
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                retry_count += 1;
            }
        }
    }
    
    bail!("Failed to map ports after {} attempts", MAX_RETRIES)
}

async fn try_map_ports<S>(client: &NatpmpAsync<S>) -> eyre::Result<Ports>
where
    S: AsyncUdpSocket,
{
    // Send UDP port mapping request
    client
        .send_port_mapping_request(Protocol::UDP, LOCAL_PORT, LOCAL_PORT, PORT_MAPPING_LIFETIME)
        .await
        .wrap_err("Failed to send UDP mapping request")?;

    // Await and process UDP response
    let udp_response = client.read_response_or_retry().await
        .wrap_err("Failed to receive UDP mapping response")?;
    
    let udp_mapped_port = match udp_response {
        Response::UDP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected UDP response type"),
    };

    // Send TCP port mapping request
    client
        .send_port_mapping_request(Protocol::TCP, LOCAL_PORT, LOCAL_PORT, PORT_MAPPING_LIFETIME)
        .await
        .wrap_err("Failed to send TCP mapping request")?;

    // Await and process TCP response
    let tcp_response = client.read_response_or_retry().await
        .wrap_err("Failed to receive TCP mapping response")?;
        
    let tcp_mapped_port = match tcp_response {
        Response::TCP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected TCP response type"),
    };

    Ok(Ports {
        udp: udp_mapped_port,
        tcp: tcp_mapped_port,
    })
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    
    let manager = match args.service {
        ServiceType::Slskd => PortManager::Slskd(SlskdManager::new()),
        ServiceType::Transmission => PortManager::Transmission(TransmissionManager::new()),
    };
    
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal");
            shutdown_tx_clone.send(()).ok();
        }
    });

    run_service(args, manager, shutdown_rx).await
}