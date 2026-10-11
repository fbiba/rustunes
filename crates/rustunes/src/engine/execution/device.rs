//! Discovery over USB (usbmuxd) or Wi-Fi (Bonjour and a direct TCP
//! connection). Selection is fixed before the first provider lookup; pairing
//! records are read, never created or repaired by this module.
mod lock;

use anyhow::{Context, Result, ensure};
use idevice::{
    Idevice, IdeviceError, IdeviceService,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice},
};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    future::Future,
    net::IpAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MAX_DEVICES: usize = 256;
const LABEL: &str = "rustunes";
/// How the selected phone is reached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transport {
    Usb,
    Wifi(IpAddr),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    DeviceDetected(Transport),
    PairingRecordAvailable,
}
fn valid_identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "device identifier must contain 1..=128 ASCII letters, digits or hyphens"
    );
    Ok(())
}
fn socket_address(value: Option<OsString>) -> Result<UsbmuxdAddr> {
    match value {
        None => Ok(UsbmuxdAddr::default()),
        Some(path) => {
            let path = path
                .to_str()
                .context("USBMUXD_SOCKET_ADDRESS must be a UTF-8 Unix socket path")?;
            ensure!(
                Path::new(path).is_absolute() && !path.contains('\0'),
                "USBMUXD_SOCKET_ADDRESS must be an absolute local Unix socket path"
            );
            Ok(UsbmuxdAddr::UnixSocket(path.to_owned()))
        }
    }
}
/// Whether nothing serves the usbmuxd socket: there is no socket file, or a
/// stopped daemon left one behind.
fn stopped(error: &IdeviceError) -> bool {
    matches!(error, IdeviceError::Socket(error) if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ))
}
/// What usbmuxd listed last in this run: devices, and those over USB.
static LISTED: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// Whether this run has logged the phone's model and iOS version.
static DESCRIBED: AtomicBool = AtomicBool::new(false);
/// How long reading them over USB may take before the log goes without.
const DESCRIBE: Duration = Duration::from_secs(5);
/// Log the phone's model and iOS version, which a bug report needs, from a
/// paired Lockdown session.
async fn describe(lockdown: &mut LockdownClient) {
    let mut details = Vec::new();
    for key in ["ProductType", "ProductVersion", "BuildVersion"] {
        let value = lockdown.get_value(Some(key), None).await.ok();
        details.push(
            value
                .and_then(|value| value.as_string().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into()),
        );
    }
    tracing::info!("Phone: {}, iOS {} ({})", details[0], details[1], details[2]);
}

/// A phone listed by `device list`, without a Lockdown session.
pub struct Listed {
    pub udid: String,
    pub transport: Transport,
}

/// A paired phone and the transport that reaches it. Both transports secure
/// sessions with the same pairing record.
#[derive(Clone, Debug)]
pub struct DeviceProvider {
    pub udid: String,
    route: Route,
}
#[derive(Clone, Debug)]
enum Route {
    Usb(UsbmuxdProvider),
}
type Connecting<T> = Pin<Box<dyn Future<Output = Result<T, IdeviceError>> + Send>>;
