use clap::Parser;
use eyre::bail;
use natpmp::{AsyncUdpSocket, NatpmpAsync, Protocol, Response};
use std::error::Error;
use std::net::Ipv4Addr;
use tokio::net::TcpListener;

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

async fn redirect_tcp_traffic(external_port: u16, local_port: u16) -> eyre::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", external_port)).await?;
    let local_addr = ("127.0.0.1", local_port);

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
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", external_port)).await?;
    let local_addr = ("127.0.0.1", local_port);

    let mut buf = vec![0u8; 2048]; // Buffer for receiving packets

    loop {
        // Receive a packet from the external port
        let (len, src) = socket.recv_from(&mut buf).await?;
        println!("Received {} bytes from external client: {}", len, src);

        // Create a new UDP socket for communicating with the local service
        let local_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let Args {
        gateway,
        local_port,
        external_port,
        lease_time,
        refresh_interval,
    } = Args::parse();

    let client = natpmp::new_tokio_natpmp_with(gateway).await?;

    // Initial port mapping
    let external_ports = map_ports(&client, local_port, external_port, lease_time).await?;
    println!("Mapped external ports: {:?}", external_ports);

    // Periodic port refresh
    tokio::spawn({
        async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(refresh_interval)).await;
                match map_ports(&client, local_port, external_port, lease_time).await {
                    Ok(_) => println!("Port mapping refreshed successfully"),
                    Err(e) => eprintln!("Failed to refresh port mapping: {}", e),
                }
            }
        }
    });

    // Run TCP and UDP redirection concurrently
    tokio::try_join!(
        redirect_tcp_traffic(external_ports.tcp, local_port),
        redirect_udp_traffic(external_ports.udp, local_port),
    )?;

    Ok(())
}
