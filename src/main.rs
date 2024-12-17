use clap::Parser;
use eyre::bail;
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

#[derive(Debug)]
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
    println!("Managing iptables rules for ports: new={}, old={:?}", new_port, old_port);

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

    if !new_status.success() {
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

async fn redirect_tcp_traffic(external_port: u16, local_port: u16) -> eyre::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, external_port)).await?;
    let local_addr = (Ipv4Addr::LOCALHOST, local_port);

    loop {
        let (mut inbound, _) = listener.accept().await?;

        let mut outbound = tokio::net::TcpStream::connect(local_addr).await?;

        tokio::spawn(async move {
            let (mut ri, mut wi) = inbound.split();
            let (mut ro, mut wo) = outbound.split();

            tokio::try_join!(
                tokio::io::copy(&mut ri, &mut wo),
                tokio::io::copy(&mut ro, &mut wi),
            )
            .ok();
        });
    }
}

async fn redirect_udp_traffic(external_port: u16, local_port: u16) -> eyre::Result<()> {
    // Bind a UDP socket to the external port
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, external_port)).await?;
    let local_addr = (Ipv4Addr::LOCALHOST, local_port);

    let mut buf = vec![0u8; 2048]; // Buffer for receiving packets

    loop {
        // Receive a packet from the external port
        let (len, src) = socket.recv_from(&mut buf).await?;
        println!("Received {} bytes from external client: {}", len, src);

        // Create a new UDP socket for communicating with the local service
        let local_socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        local_socket.connect(local_addr).await?;

        // Forward the packet to the local port
        local_socket.send(&buf[..len]).await?;
        println!(
            "Forwarded {} bytes to local service at {:?}",
            len, local_addr
        );

        // Receive a response from the local service
        let response_len = local_socket.recv(&mut buf).await?;
        println!(
            "Received {} bytes from local service, sending back to {}",
            response_len, src
        );

        // Send the response back to the original sender
        socket.send_to(&buf[..response_len], src).await?;
    }
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
    let mut port_history = None;
    let external_ports = Arc::new(tokio::sync::RwLock::new(Ports { tcp: 0, udp: 0 }));
    
    // Initial port mapping
    let ports = map_ports(&client, args.local_port, args.external_port, args.lease_time).await?;
    manage_iptables(&args.vpn_interface, ports.tcp, None).await?;
    *external_ports.write().await = ports;
    port_history = Some(ports.tcp);
    
    info!(
        local_port = args.local_port,
        external_tcp = ports.tcp,
        external_udp = ports.udp,
        "Initial port mapping successful"
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
                        if let Err(e) = manage_iptables(&args.vpn_interface, new_ports.tcp, port_history).await {
                            error!("Failed to update iptables: {}", e);
                        }
                        *external_ports.write().await = new_ports;
                        port_history = Some(new_ports.tcp);
                        info!("Port mapping refreshed successfully");
                    }
                    Err(e) => error!("Port mapping refresh failed: {}", e),
                }
            }
            _ = shutdown.recv() => {
                info!("Port mapping service shutting down");
                // Cleanup iptables rules
                if let Some(port) = port_history {
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
