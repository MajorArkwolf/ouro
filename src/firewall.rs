use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info};

use crate::port_manager::Ports;

pub async fn manage_firewall(
    interface: &str,
    new_ports: Option<&Ports>,
    old_ports: Option<&Ports>,
) -> eyre::Result<()> {
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
                    "-D",
                    "INPUT",
                    "-p",
                    proto,
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                    "-i",
                    interface,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
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
                    "-C",
                    "INPUT",
                    "-p",
                    proto,
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                    "-i",
                    interface,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?
                .success();

            if !exists {
                Command::new("iptables")
                    .args([
                        "-A",
                        "INPUT",
                        "-p",
                        proto,
                        "--dport",
                        &port.to_string(),
                        "-j",
                        "ACCEPT",
                        "-i",
                        interface,
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await?;

                info!("Added firewall rule for {}/{}", proto, port);
            }
        }
    }

    Ok(())
}
