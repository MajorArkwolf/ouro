use clap::Parser;
use eyre::{bail, WrapErr};
use natpmp::{AsyncUdpSocket, NatpmpAsync, Protocol, Response};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// VPN Gateway IP (typically 10.2.0.1 for many VPNs)
    #[arg(long, default_value = "10.2.0.1")]
    gateway: Ipv4Addr,

    /// Systemd service name (e.g., "slskd")
    #[arg(long)]
    service: String,

    /// Port environment variable name (e.g., "SLSKD_SLSK_LISTEN_PORT")
    #[arg(long)]
    port_env: String,

    /// Refresh interval in seconds (default: 45)
    #[arg(long, default_value_t = 45)]
    refresh_interval: u64,

    /// VPN interface name
    #[arg(long, default_value = "proton0")]
    vpn_interface: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Ports {
    udp: u16,
    tcp: u16,
}

#[derive(Debug)]
struct ServiceManager {
    service_name: String,
    port_env: String,
    current_ports: Option<Ports>,
}

impl ServiceManager {
    fn new(service_name: String, port_env: String) -> Self {
        Self {
            service_name,
            port_env,
            current_ports: None,
        }
    }

    async fn verify_service(&self) -> eyre::Result<()> {
        let status = Command::new("systemctl")
            .args(["status", &format!("{}.service", self.service_name)])
            .output()
            .await
            .wrap_err("Failed to check service status")?;
            
        if !status.status.success() {
            bail!("Service {} not found", self.service_name);
        }
        Ok(())
    }

    async fn stop(&mut self) -> eyre::Result<()> {
        info!("Stopping service {}...", self.service_name);
        Command::new("systemctl")
            .args(["stop", &format!("{}.service", self.service_name)])
            .status()
            .await
            .wrap_err("Failed to stop service")?;
        Ok(())
    }

    async fn start(&mut self, ports: Ports) -> eyre::Result<()> {
        info!("Starting service {} with ports TCP={}, UDP={}", self.service_name, ports.tcp, ports.udp);
        
        let override_dir = format!("/run/systemd/system/{}.service.d", self.service_name);
        fs::create_dir_all(&override_dir).await?;

        let override_content = format!(
            "[Service]\nEnvironment={}={}\n", 
            self.port_env, 
            ports.tcp
        );
        
        fs::write(
            format!("{}/override.conf", override_dir),
            override_content
        ).await?;

        Command::new("systemctl")
            .args(["daemon-reload"])
            .status()
            .await?;

        Command::new("systemctl")
            .args(["start", &format!("{}.service", self.service_name)])
            .status()
            .await?;

        self.current_ports = Some(ports);
        Ok(())
    }
}

async fn manage_firewall(interface: &str, new_ports: Option<&Ports>, old_ports: Option<&Ports>) -> eyre::Result<()> {
    info!("Managing firewall rules: new={:?}, old={:?}", new_ports, old_ports);

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
                .status()
                .await
                .wrap_err(format!("Failed to remove old {} rule", proto))?;
        }
    }

    // Add new rules
    if let Some(new) = new_ports {
        for (proto, port) in [("tcp", new.tcp), ("udp", new.udp)] {
            // Check if rule exists
            let check = Command::new("iptables")
                .args([
                    "-C", "INPUT",
                    "-p", proto,
                    "--dport", &port.to_string(),
                    "-j", "ACCEPT",
                    "-i", interface
                ])
                .status()
                .await
                .wrap_err(format!("Failed to check {} rule", proto))?;

            // Add rule if it doesn't exist
            if !check.success() {
                Command::new("iptables")
                    .args([
                        "-A", "INPUT",
                        "-p", proto,
                        "--dport", &port.to_string(),
                        "-j", "ACCEPT",
                        "-i", interface
                    ])
                    .status()
                    .await
                    .wrap_err(format!("Failed to add new {} rule", proto))?;
            }
        }
    }

    Ok(())
}

async fn run_service(
    args: Args,
    mut shutdown: broadcast::Receiver<()>,
) -> eyre::Result<()> {
    let client = natpmp::new_tokio_natpmp_with(args.gateway).await?;
    let mut service = ServiceManager::new(args.service, args.port_env);
    let mut interval = tokio::time::interval(Duration::from_secs(args.refresh_interval));

    // Verify service exists before starting
    service.verify_service().await?;

    // Initial port mapping and service start
    let ports = map_ports(&client, None).await?;
    manage_firewall(&args.vpn_interface, Some(&ports), None).await?;
    service.start(ports.clone()).await?;
    
    info!("Service started successfully with ports TCP={}, UDP={}", ports.tcp, ports.udp);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                match map_ports(&client, service.current_ports.as_ref().map(|p| p.tcp)).await {
                    Ok(new_ports) => {
                        if Some(&new_ports) != service.current_ports.as_ref() {
                            info!("Ports changed from {:?} to {:?}", service.current_ports, new_ports);
                            
                            service.stop().await?;
                            manage_firewall(
                                &args.vpn_interface,
                                Some(&new_ports),
                                service.current_ports.as_ref()
                            ).await?;
                            service.start(new_ports).await?;
                        }
                    }
                    Err(e) => {
                        error!("Port mapping failed: {}", e);
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                }
            }
            _ = shutdown.recv() => {
                info!("Shutting down...");
                service.stop().await?;
                if let Some(ports) = &service.current_ports {
                    manage_firewall(&args.vpn_interface, None, Some(ports)).await?;
                }
                break;
            }
        }
    }
    Ok(())
}

async fn map_ports<S>(
    client: &NatpmpAsync<S>,
    port: Option<u16>,
) -> eyre::Result<Ports>
where
    S: AsyncUdpSocket,
{
    let local_port = 1;  // Use port 1 as we're only interested in getting an external port
    let requested_port = port.unwrap_or(0);  // Use 0 to request any available port

    // Send UDP port mapping request
    client
        .send_port_mapping_request(Protocol::UDP, local_port, requested_port, 60)
        .await?;

    // Await and process UDP response
    let udp_response = client.read_response_or_retry().await?;
    let udp_mapped_port = match udp_response {
        Response::UDP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected UDP response type"),
    };

    // Send TCP port mapping request
    client
        .send_port_mapping_request(Protocol::TCP, local_port, requested_port, 60)
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

    run_service(args, shutdown_rx).await
}
