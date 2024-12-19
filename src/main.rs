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

    /// Transmission RPC username
    #[arg(long)]
    rpc_user: Option<String>,

    /// Transmission RPC password
    #[arg(long)]
    rpc_pass: Option<String>,
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

        // First handle firewall
        manage_firewall(interface, Some(ports), old_ports).await?;

        // Then update service
        match self {
            Self::Slskd(_) => update_service(&ServiceType::Slskd, ports, None, None).await?,
            Self::Transmission(_) => update_service(&ServiceType::Transmission, ports, None, None).await?,
        }

        // Update current ports
        match self {
            Self::Slskd(m) => m.current_ports = Some(ports.clone()),
            Self::Transmission(m) => m.current_ports = Some(ports.clone()),
        }

        Ok(())
    }

    async fn stop(&mut self) -> eyre::Result<()> {
        match self {
            Self::Slskd(_) => cleanup_service(&ServiceType::Slskd).await?,
            Self::Transmission(_) => cleanup_service(&ServiceType::Transmission).await?,
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
}

#[derive(Debug)]
struct TransmissionManager {
    current_ports: Option<Ports>,
}

impl TransmissionManager {
    fn new() -> Self {
        Self { current_ports: None }
    }
}

async fn manage_firewall(interface: &str, new_ports: Option<&Ports>, old_ports: Option<&Ports>) -> eyre::Result<()> {
    // Skip if no changes needed
    if new_ports == old_ports {
        debug!("Firewall rules unchanged, skipping update");
        return Ok(());
    }

    info!("Updating firewall rules");

    // Remove old rules if they exist
    if let Some(old) = old_ports {
        for (proto, port) in [("tcp", old.tcp), ("udp", old.udp)] {
            Command::new("iptables")
                .args([
                    "-D", "INPUT",
                    "-p", proto,
                    "--dport", &port.to_string(),
                    "-j", "ACCEPT",
                    "-i", interface
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await?;
        }
    }

    // Add new rules
    if let Some(new) = new_ports {
        for (proto, port) in [("tcp", new.tcp), ("udp", new.udp)] {
            // Check if rule exists
            let exists = Command::new("iptables")
                .args([
                    "-C", "INPUT",
                    "-p", proto,
                    "--dport", &port.to_string(),
                    "-j", "ACCEPT",
                    "-i", interface
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await?
                .success();

            if !exists {
                Command::new("iptables")
                    .args([
                        "-A", "INPUT",
                        "-p", proto,
                        "--dport", &port.to_string(),
                        "-j", "ACCEPT",
                        "-i", interface
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await?;
                
                info!("Added firewall rule for {}/{}", proto, port);
            }
        }
    }

    Ok(())
}

async fn update_service(service: &ServiceType, ports: &Ports, rpc_user: Option<&str>, rpc_pass: Option<&str>) -> eyre::Result<()> {
    match service {
        ServiceType::Slskd => {
            info!("Stopping slskd service...");
            Command::new("systemctl")
                .args(["stop", "slskd.service"])
                .status()
                .await?;

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

            info!("Starting slskd with new configuration...");
            Command::new("systemctl")
                .args(["start", "slskd.service"])
                .status()
                .await?;
        },
        ServiceType::Transmission => {
            let mut cmd = Command::new("transmission-remote");
            if let (Some(user), Some(pass)) = (rpc_user, rpc_pass) {
                cmd.args(["--auth", &format!("{}:{}", user, pass)]);
            }
            cmd.args(["--port", &ports.tcp.to_string()])
                .status()
                .await?;
        }
    }
    Ok(())
}

async fn cleanup_service(service: &ServiceType) -> eyre::Result<()> {
    if let ServiceType::Slskd = service {
        info!("Stopping slskd service...");
        Command::new("systemctl")
            .args(["stop", "slskd.service"])
            .status()
            .await?;

        info!("Removing override configuration...");
        let override_dir = "/run/systemd/system/slskd.service.d";
        let _ = fs::remove_dir_all(override_dir).await;  // Ignore if doesn't exist
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
    info!("Requesting initial port mapping...");
    let initial_ports = map_ports(&client).await?;
    info!("Got initial ports: TCP={}, UDP={}", initial_ports.tcp, initial_ports.udp);
    
    manager.apply_ports(&args.vpn_interface, &initial_ports, None).await?;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Just refresh the NAT-PMP mapping without reconfiguring
                match map_ports(&client).await {
                    Ok(new_ports) => {
                        info!(
                            current_tcp = initial_ports.tcp,
                            current_udp = initial_ports.udp,
                            new_tcp = new_ports.tcp,
                            new_udp = new_ports.udp,
                            "Checking port mapping"
                        );
                        
                        if new_ports != initial_ports {
                            info!("Ports changed, reconfiguring service");
                            manager.apply_ports(&args.vpn_interface, &new_ports, manager.current_ports().as_ref()).await?;
                        } else {
                            debug!("Ports unchanged, keeping current configuration");
                        }
                    }
                    Err(e) => {
                        error!("Port mapping refresh failed: {}", e);
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
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
    debug!("Requesting UDP port mapping...");
    client
        .send_port_mapping_request(Protocol::UDP, LOCAL_PORT, LOCAL_PORT, PORT_MAPPING_LIFETIME)
        .await
        .wrap_err("Failed to send UDP mapping request")?;

    let udp_response = client.read_response_or_retry().await
        .wrap_err("Failed to receive UDP mapping response")?;
    
    let udp_mapped_port = match udp_response {
        Response::UDP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected UDP response type"),
    };
    debug!("Got UDP port: {}", udp_mapped_port);

    debug!("Requesting TCP port mapping...");
    client
        .send_port_mapping_request(Protocol::TCP, LOCAL_PORT, LOCAL_PORT, PORT_MAPPING_LIFETIME)
        .await
        .wrap_err("Failed to send TCP mapping request")?;

    let tcp_response = client.read_response_or_retry().await
        .wrap_err("Failed to receive TCP mapping response")?;
        
    let tcp_mapped_port = match tcp_response {
        Response::TCP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected TCP response type"),
    };
    debug!("Got TCP port: {}", tcp_mapped_port);

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