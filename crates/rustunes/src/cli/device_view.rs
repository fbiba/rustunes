//! List the USB transport inventory, or the paired phones answering Bonjour
//! over Wi-Fi, without opening a phone service. Being listed does not attest
//! to model or pairing/trust.
use crate::engine::execution::device::{Listed, Transport};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{future::Future, io::Write, time::Duration};
use tokio::time::{Instant, timeout_at};

#[derive(Serialize)]
struct Device<'a> {
    udid: &'a str,
    connection: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
}
#[derive(Serialize)]
struct Listing<'a> {
    schema: &'static str,
    devices: Vec<Device<'a>>,
    device_count: usize,
    selection_required: bool,
    pairing_checked: bool,
    phone_writes: u32,
}

pub async fn list(json: bool, out: &mut impl Write) -> Result<()> {
    list_with(
        json,
        out,
        Instant::now() + Duration::from_secs(15),
        crate::engine::execution::device::devices,
    )
    .await
}
async fn list_with<F, Fut>(
    json: bool,
    out: &mut impl Write,
    deadline: Instant,
    query: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<Listed>>>,
{
    ensure!(
        Instant::now() < deadline,
        "device listing deadline expired before discovery"
    );
    let devices = timeout_at(deadline, query())
        .await
        .context("timed out listing devices")??;
    ensure!(
        devices.len() <= 256,
        "device listing requires a bounded inventory"
    );
    let report = Listing {
        schema: "rustunes.devices",
        device_count: devices.len(),
        selection_required: devices.len() > 1,
        devices: devices
            .iter()
            .map(|d| Device {
                udid: &d.udid,
                connection: match d.transport {
                    Transport::Usb => "usb",
                    Transport::Wifi(_) => "wifi",
                },
                address: match d.transport {
                    Transport::Usb => None,
                    Transport::Wifi(address) => Some(address.to_string()),
                },
            })
            .collect(),
        pairing_checked: false,
        phone_writes: 0,
    };
    if json {
        serde_json::to_writer_pretty(&mut *out, &report)?;
        writeln!(out)?;
    } else if devices.is_empty() {
        writeln!(
            out,
            "No Apple devices connected over USB or answering over Wi-Fi."
        )?;
    } else {
        writeln!(out, "Devices: {}", devices.len())?;
        for device in &devices {
            match device.transport {
                Transport::Usb => writeln!(out, "  {} (USB)", terminal(&device.udid))?,
                Transport::Wifi(address) => {
                    writeln!(out, "  {} (Wi-Fi, {address})", terminal(&device.udid))?
                }
            }
        }
        if devices.len() > 1 {
            writeln!(out, "Select a device with --device UDID.")?;
        }
        writeln!(out, "Pairing status has not been checked.")?;
    }
    Ok(())
}
fn terminal(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}') {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
