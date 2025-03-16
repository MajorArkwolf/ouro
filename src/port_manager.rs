use std::time::Duration;

use eyre::{Context, bail};
use natpmp::{AsyncUdpSocket, NatpmpAsync, Protocol, Response};
use tokio::time::timeout;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Ports {
    pub udp: u16,
    pub tcp: u16,
}

pub const MAX_RETRIES: u32 = 3;
pub const RETRY_DELAY: Duration = Duration::from_secs(60);
pub const TIMEOUT: Duration = Duration::from_secs(45);
pub const LOCAL_PORT: u16 = 0; // Use port 0 to bind to any available local port
pub const REQUESTED_PORT: u16 = 1; // Request port 1, NAT-PMP will assign a port
pub const PORT_MAPPING_LIFETIME: u32 = 60; // 60 second lifetime

pub async fn map_ports<S>(client: &NatpmpAsync<S>) -> eyre::Result<Ports>
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

pub async fn try_map_ports<S>(client: &NatpmpAsync<S>) -> eyre::Result<Ports>
where
    S: AsyncUdpSocket,
{
    debug!("Requesting UDP port mapping...");
    timeout(
        TIMEOUT,
        client.send_port_mapping_request(
            Protocol::UDP,
            LOCAL_PORT,
            REQUESTED_PORT,
            PORT_MAPPING_LIFETIME,
        ),
    )
    .await
    .wrap_err("UDP mapping request timeout")?
    .wrap_err("Failed to send UDP mapping request")?;

    let udp_response = timeout(TIMEOUT, client.read_response_or_retry())
        .await
        .wrap_err("UDP mapping response timeout")?
        .wrap_err("Failed to receive UDP mapping response")?;

    let udp_mapped_port = match udp_response {
        Response::UDP(mapping) => mapping.public_port(),
        _ => bail!("Unexpected UDP response type"),
    };
    debug!("Got UDP port: {}", udp_mapped_port);

    debug!("Requesting TCP port mapping...");
    timeout(
        TIMEOUT,
        client.send_port_mapping_request(
            Protocol::TCP,
            LOCAL_PORT,
            REQUESTED_PORT,
            PORT_MAPPING_LIFETIME,
        ),
    )
    .await
    .wrap_err("TCP mapping request timeout")?
    .wrap_err("Failed to send TCP mapping request")?;

    let tcp_response = timeout(TIMEOUT, client.read_response_or_retry())
        .await
        .wrap_err("TCP mapping response timeout")?
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
