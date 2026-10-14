//! Discovery over USB (usbmuxd) or Wi-Fi (Bonjour and a direct TCP
//! connection). Selection is fixed before the first provider lookup; pairing
//! records are read, never created or repaired by this module.
mod lock;
mod wifi;

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
/// Explicit local discovery configuration: USB unless Wi-Fi is chosen.
/// Environment resolution belongs to the caller; no lookup occurs until a
/// scoped operation requests a provider.
#[derive(Clone, Default)]
pub struct Config {
    selected: Option<String>,
    socket: Option<OsString>,
    wifi: Option<WifiConfig>,
    locks: Option<PathBuf>,
}
#[derive(Clone, PartialEq)]
struct WifiConfig {
    address: Option<IpAddr>,
    pairing_records: PathBuf,
    remembered: Option<PathBuf>,
}
impl Config {
    pub(crate) fn selected_device(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    /// The global options that reach the phone as this configuration does,
    /// for a command printed for the user to run next: ` --wifi` or
    /// ` --wifi-address IP`, and nothing over USB. `--device` is left out so
    /// that the phone's identifier is never printed; with several phones
    /// attached, the printed command then asks for it.
    pub(crate) fn command_options(&self) -> String {
        match &self.wifi {
            None => String::new(),
            Some(WifiConfig {
                address: Some(address),
                ..
            }) => format!(" --wifi-address {address}"),
            Some(_) => " --wifi".into(),
        }
    }
    pub fn new(selected: Option<String>, socket: Option<OsString>) -> Result<Self> {
        if let Some(value) = &selected {
            valid_identifier(value)?;
        }
        // Offline commands never use the transport socket; address()
        // validates it immediately before the first connection.
        Ok(Self {
            selected,
            socket,
            wifi: None,
            locks: None,
        })
    }
    /// Reach the phone over Wi-Fi instead of USB: find it by Bonjour, or at
    /// `address`, and secure sessions with the pairing record usbmuxd saved
    /// when it was paired over USB. No usbmuxd connection is made. Each
    /// phone's last address is kept in `remembered`, so it can be woken there.
    pub fn over_wifi(mut self, address: Option<IpAddr>, remembered: Option<PathBuf>) -> Self {
        self.wifi = Some(WifiConfig {
            address,
            pairing_records: wifi::PAIRING_RECORDS.into(),
            remembered,
        });
        self
    }
    /// Lock each phone a run reaches with a file in `directory`, so that no
    /// other run uses it at the same time (see `lock`).
    pub fn locked_in(mut self, directory: Option<PathBuf>) -> Self {
        self.locks = directory;
        self
    }
}
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
struct Scope {
    selected: Option<String>,
    socket: Option<OsString>,
    wifi: Option<WifiConfig>,
    /// The phone found over Wi-Fi, reused by later lookups in this scope and
    /// in nested scopes of the same configuration. Its heartbeat stops when
    /// the outermost of them ends.
    found: Arc<Mutex<Option<wifi::Found>>>,
    /// The latest complete index snapshot captured in this scope, shared the
    /// same way.
    snapshot: Arc<Mutex<Option<PathBuf>>>,
    /// Where each phone this scope reaches is locked, if anywhere.
    locks: Option<PathBuf>,
    observer: Box<dyn Fn(Event) -> Result<()> + Send + Sync>,
}
tokio::task_local! { static CURRENT: Scope; }

/// Bind discovery to a caller's explicit configuration for this async task.
#[cfg(test)]
pub async fn scope<F: std::future::Future>(config: Config, future: F) -> F::Output {
    scope_with_observer(config, |_| Ok(()), future).await
}
pub async fn scope_with_observer<F: std::future::Future>(
    config: Config,
    observer: impl Fn(Event) -> Result<()> + Send + Sync + 'static,
    future: F,
) -> F::Output {
    let mut selected = config.selected.clone();
    let mut found = Arc::default();
    let mut snapshot = Arc::default();
    // An engine nested in the same configuration keeps the phone its caller
    // already found and holds awake (discovering it again repeats the wake,
    // Lockdown confirmation and hold), and its latest snapshot.
    if let Ok(Some((outer_selected, outer_found, outer_snapshot))) =
        CURRENT.try_with(|outer| outer.shared(&config))
    {
        selected = outer_selected;
        found = outer_found;
        snapshot = outer_snapshot;
    }
    CURRENT
        .scope(
            Scope {
                selected,
                socket: config.socket,
                wifi: config.wifi,
                found,
                snapshot,
                locks: config.locks,
                observer: Box::new(observer),
            },
            future,
        )
        .await
}
type Shared = (
    Option<String>,
    Arc<Mutex<Option<wifi::Found>>>,
    Arc<Mutex<Option<PathBuf>>>,
);
impl Scope {
    /// What a nested scope of `config` inherits: only a scope of the same
    /// transport whose selection the nested one does not contradict.
    fn shared(&self, config: &Config) -> Option<Shared> {
        if self.wifi != config.wifi || self.socket != config.socket {
            return None;
        }
        if config.selected.is_some() && self.selected != config.selected {
            return None;
        }
        Some((
            self.selected.clone(),
            Arc::clone(&self.found),
            Arc::clone(&self.snapshot),
        ))
    }
}
/// Record the latest complete index snapshot taken in this scope.
pub(crate) fn remember_snapshot(path: &Path) {
    let _ = CURRENT.try_with(|scope| {
        if let Ok(mut slot) = scope.snapshot.lock() {
            *slot = Some(path.to_owned());
        }
    });
}
/// The latest complete index snapshot taken in this scope, if any.
pub(crate) fn remembered_snapshot() -> Option<PathBuf> {
    CURRENT
        .try_with(|scope| scope.snapshot.lock().ok().and_then(|slot| slot.clone()))
        .ok()
        .flatten()
}

fn address() -> Result<UsbmuxdAddr> {
    CURRENT
        .try_with(|scope| socket_address(scope.socket.clone()))
        .context("device configuration scope is required")?
}
fn selection() -> Result<Option<String>> {
    CURRENT
        .try_with(|scope| scope.selected.clone())
        .context("device configuration scope is required")
}
/// Lock the phone `udid` for the rest of this process, where the scope's
/// configuration says.
fn lock_phone(udid: &str) -> Result<()> {
    let locks = CURRENT
        .try_with(|scope| scope.locks.clone())
        .context("device configuration scope is required")?;
    locks.map_or(Ok(()), |directory| lock::take(&directory, udid))
}
fn emit(event: Event) -> Result<()> {
    CURRENT
        .try_with(|scope| (scope.observer)(event))
        .context("device configuration scope is required")?
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
fn usb_only(devices: Vec<UsbmuxdDevice>) -> Result<Vec<UsbmuxdDevice>> {
    ensure!(
        devices.len() <= MAX_DEVICES,
        "usbmuxd device list exceeds supported bound"
    );
    let mut usb: Vec<_> = devices
        .into_iter()
        .filter(|d| d.connection_type == Connection::Usb)
        .collect();
    let mut identifiers = BTreeSet::new();
    let mut handles = BTreeSet::new();
    for device in &usb {
        valid_identifier(&device.udid)?;
        ensure!(
            identifiers.insert(device.udid.as_str()) && handles.insert(device.device_id),
            "usbmuxd returned ambiguous USB device identities"
        );
    }
    usb.sort_by(|a, b| a.udid.cmp(&b.udid));
    Ok(usb)
}
/// Whether nothing serves the usbmuxd socket: there is no socket file, or a
/// stopped daemon left one behind.
fn stopped(error: &IdeviceError) -> bool {
    matches!(error, IdeviceError::Socket(error) if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    ))
}
async fn enumerate(address: &UsbmuxdAddr) -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = address.connect(0).await.map_err(|error| {
        let context = if stopped(&error) {
            "connecting to usbmuxd, which is not running; plug the iPhone in (on most distributions that starts usbmuxd) or install usbmuxd, then run again"
        } else {
            "connecting to usbmuxd"
        };
        anyhow::Error::new(error).context(context)
    })?;
    let devices = mux.get_devices().await.context("listing usbmuxd devices")?;
    let listed = devices.len();
    let usb = usb_only(devices)?;
    // Every phone operation lists the devices again; say only what changed.
    let counts = (listed, usb.len());
    let last = LISTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(counts);
    if last != Some(counts) {
        tracing::debug!("usbmuxd lists {listed} device(s), {} over USB", usb.len());
    }
    Ok(usb)
}
/// What usbmuxd listed last in this run: devices, and those over USB.
static LISTED: Mutex<Option<(usize, usize)>> = Mutex::new(None);
/// ListDevices only: no Lockdown session and no pairing-record access. The
/// dependency decodes its response before our entry-count validation; this is
/// not a claim that upstream usbmuxd frame allocation is bounded here.
pub async fn usb_devices() -> Result<Vec<UsbmuxdDevice>> {
    let address = address()?;
    enumerate(&address).await
}
fn select_device(mut devices: Vec<UsbmuxdDevice>, selected: Option<&str>) -> Result<UsbmuxdDevice> {
    if let Some(selected) = selected {
        let index = devices
            .iter()
            .position(|d| d.udid == selected)
            .context("selected device is not connected over USB; inspect device list")?;
        Ok(devices.swap_remove(index))
    } else {
        ensure!(!devices.is_empty(), "no iPhone connected to Linux over USB");
        ensure!(
            devices.len() == 1,
            "multiple USB Apple devices connected; select the intended device with --device UDID"
        );
        Ok(devices.remove(0))
    }
}
/// The selected phone over the configured transport.
pub async fn provider() -> Result<DeviceProvider> {
    let provider = match wifi_config()? {
        None => usb_provider().await,
        Some(config) => wifi_provider(&config).await,
    }?;
    crate::engine::state::audio_index::cache::phone_selected(&provider.udid);
    Ok(provider)
}
async fn usb_provider() -> Result<DeviceProvider> {
    let selected = selection()?;
    let address = address()?;
    let device = select_device(enumerate(&address).await?, selected.as_deref())?;
    emit(Event::DeviceDetected(Transport::Usb))?;
    lock_phone(&device.udid)?;
    let provider = device.to_provider(address, LABEL);
    // Never print the pairing file: it contains private keys and host identifiers.
    let pairing = provider
        .get_pairing_file()
        .await
        .context("existing pairing record unavailable; user pairing/trust action is required")?;
    emit(Event::PairingRecordAvailable)?;
    if !DESCRIBED.swap(true, Ordering::Relaxed) {
        let described = async {
            let mut lockdown = LockdownClient::connect(&provider).await?;
            lockdown.start_session(&pairing).await?;
            describe(&mut lockdown).await;
            Ok::<_, IdeviceError>(())
        };
        match tokio::time::timeout(DESCRIBE, described).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!("Phone model and iOS version not read: {error}"),
            Err(_) => tracing::debug!("Phone model and iOS version not read within {DESCRIBE:?}"),
        }
    }
    Ok(DeviceProvider::usb(provider))
}
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
async fn wifi_provider(config: &WifiConfig) -> Result<DeviceProvider> {
    if let Some(provider) = found()? {
        return Ok(provider);
    }
    let found = wifi::find(config, selection()?.as_deref()).await?;
    emit(Event::DeviceDetected(found.provider.transport()))?;
    emit(Event::PairingRecordAvailable)?;
    let provider = found.provider.clone();
    CURRENT
        .try_with(|scope| -> Result<()> {
            *scope
                .found
                .lock()
                .map_err(|_| anyhow::anyhow!("Wi-Fi device lock poisoned"))? = Some(found);
            Ok(())
        })
        .context("device configuration scope is required")??;
    Ok(provider)
}
fn wifi_config() -> Result<Option<WifiConfig>> {
    CURRENT
        .try_with(|scope| scope.wifi.clone())
        .context("device configuration scope is required")
}
fn found() -> Result<Option<DeviceProvider>> {
    CURRENT
        .try_with(|scope| {
            Ok(scope
                .found
                .lock()
                .map_err(|_| anyhow::anyhow!("Wi-Fi device lock poisoned"))?
                .as_ref()
                .map(|found| found.provider.clone()))
        })
        .context("device configuration scope is required")?
}
/// Whether the Wi-Fi phone found in this scope announced that it was going to
/// sleep, which closes every connection it had open.
pub fn phone_slept() -> bool {
    CURRENT
        .try_with(|scope| {
            scope
                .found
                .lock()
                .is_ok_and(|found| found.as_ref().is_some_and(wifi::Found::slept))
        })
        .unwrap_or(false)
}

/// A phone listed by `device list`, without a Lockdown session.
pub struct Listed {
    pub udid: String,
    pub transport: Transport,
}
/// USB devices from usbmuxd, or paired phones answering Bonjour over Wi-Fi.
pub async fn devices() -> Result<Vec<Listed>> {
    match wifi_config()? {
        None => Ok(usb_devices()
            .await?
            .into_iter()
            .map(|device| Listed {
                udid: device.udid,
                transport: Transport::Usb,
            })
            .collect()),
        Some(config) => wifi::list(&config).await,
    }
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
    Wifi(wifi::Phone),
}
impl DeviceProvider {
    pub fn usb(provider: UsbmuxdProvider) -> Self {
        Self {
            udid: provider.udid.clone(),
            route: Route::Usb(provider),
        }
    }
    pub fn transport(&self) -> Transport {
        match &self.route {
            Route::Usb(_) => Transport::Usb,
            Route::Wifi(phone) => Transport::Wifi(phone.address()),
        }
    }
}
type Connecting<T> = Pin<Box<dyn Future<Output = Result<T, IdeviceError>> + Send>>;
impl IdeviceProvider for DeviceProvider {
    fn connect(&self, port: u16) -> Connecting<Idevice> {
        match &self.route {
            Route::Usb(provider) => provider.connect(port),
            Route::Wifi(phone) => phone.connect(port),
        }
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn get_pairing_file(&self) -> Connecting<PairingFile> {
        match &self.route {
            Route::Usb(provider) => provider.get_pairing_file(),
            Route::Wifi(phone) => phone.pairing_file(),
        }
    }
}

#[cfg(test)]
mod tests;
