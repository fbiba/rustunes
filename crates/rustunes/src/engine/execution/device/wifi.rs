//! Wi-Fi discovery. A paired phone advertises `_apple-mobdev2._tcp` over
//! Bonjour and accepts Lockdown on TCP 62078; sessions use the pairing record
//! usbmuxd saved when the phone was paired and trusted over USB. Services are
//! reached on the TCP ports Lockdown names, exactly as usbmuxd forwards them.
//!
//! A sleeping phone keeps answering ARP but ignores Bonjour and new TCP
//! connections. A unicast datagram to the Wake-on-LAN port wakes it within
//! about a second, so the address each phone last answered at is remembered.
//! A locked phone then stays awake only while this host answers its
//! heartbeat and holds a power assertion.
use super::{
    Connecting, DeviceProvider, LABEL, Listed, MAX_DEVICES, Route, Transport, WifiConfig,
    valid_identifier,
};
use anyhow::{Context, Result, bail, ensure};
use idevice::{
    HeartbeatError, Idevice, IdeviceError, IdeviceService, heartbeat::HeartbeatClient,
    lockdown::LockdownClient, pairing_file::PairingFile, provider::IdeviceProvider,
};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent, TxtProperties};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Where usbmuxd keeps pairing records on Linux, one `<UDID>.plist` each.
pub(super) const PAIRING_RECORDS: &str = "/var/lib/lockdown";
const SERVICE: &str = "_apple-mobdev2._tcp.local.";
const LOCKDOWN_PORT: u16 = 62078;
/// The phone's radio wakes its system for unicast datagrams to the echo (7)
/// or discard (9) port, whatever they carry; other ports and broadcasts do not.
const WAKE_PORT: u16 = 9;
const MAX_RECORD_BYTES: u64 = 1 << 20;
/// How long to wait for Bonjour answers. A selected phone ends the wait as
/// soon as it resolves; otherwise every answering phone is collected.
const BROWSE: Duration = Duration::from_secs(4);
/// A locked phone can take several seconds to wake its radio for a new
/// connection.
const CONNECT: Duration = Duration::from_secs(15);
const WAKE_REPEAT: Duration = Duration::from_secs(1);

/// One phone on the network: its address and the pairing record that opens
/// sessions with it.
#[derive(Clone)]
pub(super) struct Phone {
    address: IpAddr,
    scope_id: u32,
    pairing: Arc<PairingFile>,
}
// The pairing record holds private keys; never print it.
impl fmt::Debug for Phone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Phone")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}
impl Phone {
    pub(super) fn address(&self) -> IpAddr {
        self.address
    }
    pub(super) fn connect(&self, port: u16) -> Connecting<Idevice> {
        let address = socket_address(self.address, self.scope_id, port);
        let mac = self.pairing.wifi_mac_address.clone();
        Box::pin(async move {
            // A dozing phone drops SYNs; TCP retries them once it is awake. A
            // phone still falling asleep ignores a wake, so it is repeated.
            let connecting = tokio::net::TcpStream::connect(address);
            tokio::pin!(connecting);
            let deadline = tokio::time::Instant::now() + CONNECT;
            let stream = loop {
                wake(address, &mac);
                let next = deadline.min(tokio::time::Instant::now() + WAKE_REPEAT);
                if let Ok(connected) = tokio::time::timeout_at(next, &mut connecting).await {
                    break connected?;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("no answer from the phone at {address} over Wi-Fi"),
                    )
                    .into());
                }
            };
            // Sessions exchange many small request/reply frames.
            stream.set_nodelay(true)?;
            Ok(Idevice::new(Box::new(stream), LABEL))
        })
    }
    pub(super) fn pairing_file(&self) -> Connecting<PairingFile> {
        let pairing = PairingFile::clone(&self.pairing);
        Box::pin(async move { Ok(pairing) })
    }
}

fn socket_address(address: IpAddr, scope_id: u32, port: u16) -> SocketAddr {
    match address {
        IpAddr::V4(_) => SocketAddr::new(address, port),
        IpAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(v6, port, 0, scope_id)),
    }
}

/// Send a Wake-on-LAN magic packet for `mac` to the phone's own address. Any
/// failure is left to the connection that follows.
fn wake(address: SocketAddr, mac: &str) {
    let mut target = address;
    target.set_port(WAKE_PORT);
    let local = match target {
        SocketAddr::V4(_) => SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
        SocketAddr::V6(_) => SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
    };
    if let Ok(socket) = UdpSocket::bind(local) {
        let _ = socket.send_to(&magic_packet(mac), target);
    }
}
fn magic_packet(mac: &str) -> Vec<u8> {
    let octets: Vec<u8> = mac
        .split(':')
        .filter_map(|octet| u8::from_str_radix(octet, 16).ok())
        .collect();
    let mut packet = vec![0xff; 6];
    if octets.len() == 6 {
        for _ in 0..16 {
            packet.extend(&octets);
        }
    }
    packet
}

/// The last address each phone answered at, one file per UDID. Failing to
/// read or write it never fails a run.
struct Remembered(Option<PathBuf>);
impl Remembered {
    fn get(&self, udid: &str) -> Option<IpAddr> {
        let path = self.0.as_ref()?.join(udid);
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }
    /// A sleeping phone ignores Bonjour until woken where it last answered.
    fn wake(&self, records: &[Record]) {
        for record in records {
            if let Some(address) = self.get(&record.udid) {
                wake(
                    socket_address(address, 0, WAKE_PORT),
                    &record.pairing.wifi_mac_address,
                );
            }
        }
    }
    fn set(&self, udid: &str, address: IpAddr) {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let Some(directory) = &self.0 else {
            return;
        };
        if self.get(udid) == Some(address) {
            return;
        }
        let write = || -> std::io::Result<()> {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            let temporary = directory.join(format!(".{udid}.tmp"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            std::io::Write::write_all(&mut file, format!("{address}\n").as_bytes())?;
            std::fs::rename(temporary, directory.join(udid))
        };
        if let Err(error) = write() {
            tracing::debug!("The phone's Wi-Fi address was not remembered: {error}");
        }
    }
}

/// The phone found in this scope, with its heartbeat and power assertion.
pub(super) struct Found {
    pub(super) provider: DeviceProvider,
    heartbeat: Heartbeat,
    _keep_awake: KeepAwake,
}
impl Found {
    pub(super) fn slept(&self) -> bool {
        self.heartbeat.slept.load(Ordering::Relaxed)
    }
}

/// Find the selected (or only) paired phone, lock it for this run, confirm
/// its identity in a paired Lockdown session, start answering its heartbeat
/// and keep it awake.
pub(super) async fn find(config: &WifiConfig, selected: Option<&str>) -> Result<Found> {
    let records = records(&config.pairing_records, selected)?;
    tracing::debug!(
        "{} pairing record(s) in {}",
        records.len(),
        config.pairing_records.display()
    );
    let remembered = Remembered(config.remembered.clone());
    let (record, address, scope_id) = match config.address {
        Some(address) => (record_at(address, records).await?, address, 0),
        None => {
            remembered.wake(&records);
            let mut found = browse(&records, selected).await?;
            if found.is_empty()
                && let [record] = records.as_slice()
                && let Some(address) = remembered.get(&record.udid)
            {
                // Still asleep or not announcing: try where it last answered.
                tracing::debug!(
                    "No Bonjour answer; trying {address}, where the phone last answered"
                );
                found.insert(record.udid.clone(), (address, 0));
            }
            ensure!(
                !found.is_empty(),
                "no paired iPhone answered over Wi-Fi; wake or unlock it on this network, or pass --wifi-address. If it is awake and still does not answer, it may no longer trust this computer: pair and trust it over USB, then run again"
            );
            ensure!(
                found.len() == 1,
                "multiple paired iPhones found over Wi-Fi; select the intended device with --device UDID"
            );
            let (udid, (address, scope_id)) = found.pop_first().expect("one phone");
            let record = records
                .into_iter()
                .find(|record| record.udid == udid)
                .expect("found phones come from the records");
            (record, address, scope_id)
        }
    };
    let provider = DeviceProvider {
        udid: record.udid,
        route: Route::Wifi(Phone {
            address,
            scope_id,
            pairing: Arc::new(record.pairing),
        }),
    };
    super::lock_phone(&provider.udid)?;
    confirm(&provider).await?;
    remembered.set(&provider.udid, address);
    let (heartbeat, keep_awake) = held_awake(&provider).await;
    Ok(Found {
        provider,
        heartbeat,
        _keep_awake: keep_awake,
    })
}

/// Paired phones answering Bonjour, without opening a session.
pub(super) async fn list(config: &WifiConfig) -> Result<Vec<Listed>> {
    let records = records(&config.pairing_records, None)?;
    Remembered(config.remembered.clone()).wake(&records);
    Ok(browse(&records, None)
        .await?
        .into_iter()
        .map(|(udid, (address, _))| Listed {
            udid,
            transport: Transport::Wifi(address),
        })
        .collect())
}

/// One usbmuxd pairing record: the phone's UDID (the file name) and secrets.
struct Record {
    udid: String,
    pairing: PairingFile,
}

fn records(directory: &Path, selected: Option<&str>) -> Result<Vec<Record>> {
    let unreadable = || {
        format!(
            "reading pairing records in {}; pair and trust the phone over USB first",
            directory.display()
        )
    };
    let mut records = Vec::new();
    for entry in std::fs::read_dir(directory).with_context(unreadable)? {
        let path = entry.with_context(unreadable)?.path();
        let Some(udid) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".plist"))
        else {
            continue;
        };
        if udid == "SystemConfiguration"
            || valid_identifier(udid).is_err()
            || selected.is_some_and(|selected| selected != udid)
        {
            continue;
        }
        ensure!(
            records.len() < MAX_DEVICES,
            "too many pairing records in {}",
            directory.display()
        );
        let size = path.metadata().with_context(unreadable)?.len();
        ensure!(
            size <= MAX_RECORD_BYTES,
            "pairing record {} is too large",
            path.display()
        );
        let bytes = std::fs::read(&path).with_context(unreadable)?;
        // Never print the record or its parse details: they hold private keys.
        let pairing = PairingFile::from_bytes(&bytes)
            .map_err(|_| anyhow::anyhow!("pairing record {} is unreadable", path.display()))?;
        records.push(Record {
            udid: udid.to_owned(),
            pairing,
        });
    }
    records.sort_by(|a, b| a.udid.cmp(&b.udid));
    match selected {
        Some(selected) => ensure!(
            !records.is_empty(),
            "no pairing record for {selected} in {}; pair and trust it over USB first",
            directory.display()
        ),
        None => ensure!(
            !records.is_empty(),
            "no pairing records in {}; pair and trust the phone over USB first",
            directory.display()
        ),
    }
    Ok(records)
}

/// The record for the phone at an explicit address: the only (or selected)
/// record, or the one naming the UDID the phone reports before a session.
async fn record_at(address: IpAddr, mut records: Vec<Record>) -> Result<Record> {
    if records.len() == 1 {
        return Ok(records.remove(0));
    }
    let probe = Phone {
        address,
        scope_id: 0,
        pairing: Arc::new(records[0].pairing.clone()),
    };
    let mut lockdown = LockdownClient::new(
        probe
            .connect(LOCKDOWN_PORT)
            .await
            .with_context(|| format!("connecting to Lockdown at {address}"))?,
    );
    let udid = lockdown
        .get_value(Some("UniqueDeviceID"), None)
        .await
        .ok()
        .and_then(|value| value.as_string().map(str::to_owned));
    match udid.and_then(|udid| records.iter().position(|record| record.udid == udid)) {
        Some(index) => Ok(records.swap_remove(index)),
        None => bail!(
            "several pairing records and the phone at {address} did not name itself; select it with --device UDID"
        ),
    }
}

type Keys = Vec<(String, Vec<u8>, String)>;

/// Bonjour answers matched to pairing records, by UDID. iOS 26.4 and later
/// publish an identifier and auth tags derived from the record's HostID;
/// earlier versions name the service after the phone's Wi-Fi MAC address.
async fn browse(
    records: &[Record],
    selected: Option<&str>,
) -> Result<BTreeMap<String, (IpAddr, u32)>> {
    let keys: Keys = records
        .iter()
        .map(|record| {
            (
                record.udid.clone(),
                record.pairing.host_id.as_bytes().to_vec(),
                record.pairing.wifi_mac_address.clone(),
            )
        })
        .collect();
    let selected = selected.map(str::to_owned);
    tokio::task::spawn_blocking(move || browse_blocking(&keys, selected.as_deref()))
        .await
        .context("Bonjour discovery task failed")?
}
fn browse_blocking(keys: &Keys, selected: Option<&str>) -> Result<BTreeMap<String, (IpAddr, u32)>> {
    let daemon = ServiceDaemon::new().context("starting Bonjour discovery")?;
    let events = daemon.browse(SERVICE).context("browsing Bonjour")?;
    let deadline = std::time::Instant::now() + BROWSE;
    let mut found = BTreeMap::new();
    let mut answers = 0;
    while let Ok(event) = events.recv_deadline(deadline) {
        let ServiceEvent::ServiceResolved(service) = event else {
            continue;
        };
        answers += 1;
        let txt = service.get_properties();
        let udid = matching(service.get_fullname(), txt, keys);
        let (Some(udid), Some(address)) = (udid, preferred(service.get_addresses())) else {
            if udid.is_some() {
                tracing::debug!("A paired phone answered Bonjour without an address");
            } else if let Some(Some(_)) = txt.get_property_val("identifier") {
                // Other Apple devices answer too, and so does a paired phone
                // that no longer trusts this computer: its tags match none.
                tracing::debug!(
                    "A Bonjour answer with an identifier and {} auth tag(s) matched no pairing record",
                    auth_tags(txt).len()
                );
            } else {
                tracing::debug!(
                    "A Bonjour answer named by a Wi-Fi address matched no pairing record"
                );
            }
            continue;
        };
        tracing::debug!("A paired phone answered Bonjour at {}", address.0);
        found.insert(udid.to_owned(), address);
        // Only paired phones can match, so once each has answered (or the
        // selected one has) nothing else can turn up.
        if selected == Some(udid) || found.len() == keys.len() {
            break;
        }
    }
    let _ = daemon.shutdown();
    tracing::debug!(
        "{answers} Bonjour answer(s), {} from paired phones",
        found.len()
    );
    Ok(found)
}
/// The tags of the hosts a phone trusts, from iOS 26.4.
fn auth_tags(txt: &TxtProperties) -> Vec<&[u8]> {
    txt.iter()
        .filter(|property| property.key() == "authTag" || property.key().starts_with("authTag#"))
        .filter_map(|property| property.val())
        .collect()
}
fn matching<'a>(fullname: &str, txt: &TxtProperties, keys: &'a Keys) -> Option<&'a str> {
    let tags = auth_tags(txt);
    if let Some(Some(identifier)) = txt.get_property_val("identifier")
        && let Some((udid, ..)) = keys
            .iter()
            .find(|(_, host_id, _)| idevice::mdns::txt_record_matches(host_id, identifier, &tags))
    {
        return Some(udid);
    }
    let (mac, _) = fullname.split_once('@')?;
    keys.iter()
        .find(|(.., wifi_mac)| wifi_mac.eq_ignore_ascii_case(mac))
        .map(|(udid, ..)| udid.as_str())
}
/// IPv4 first; an IPv6 address keeps its interface for link-local use.
fn preferred(addresses: &HashSet<ScopedIp>) -> Option<(IpAddr, u32)> {
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| addresses.iter().next())
        .map(|address| match address {
            ScopedIp::V6(v6) => (IpAddr::V6(*v6.addr()), v6.scope_id().index),
            other => (other.to_ip_addr(), 0),
        })
}

/// A paired session proves the phone accepts this host's record; the UDID it
/// reports inside that session must be the one the record is filed under.
async fn confirm(provider: &DeviceProvider) -> Result<()> {
    let Transport::Wifi(address) = provider.transport() else {
        unreachable!("Wi-Fi providers only");
    };
    let mut lockdown = LockdownClient::connect(provider)
        .await
        .with_context(|| format!("connecting to Lockdown at {address} over Wi-Fi"))?;
    lockdown
        .start_session(&provider.get_pairing_file().await?)
        .await
        .context(
            "the phone refused this computer's pairing record over Wi-Fi; plug it in over USB, unlock it and tap Trust, then run again",
        )?;
    let udid = lockdown
        .get_value(Some("UniqueDeviceID"), None)
        .await
        .context("reading the phone's identifier over Wi-Fi")?;
    ensure!(
        udid.as_string() == Some(provider.udid.as_str()),
        "the phone at {address} is not the device its pairing record names"
    );
    super::describe(&mut lockdown).await;
    Ok(())
}

/// Once a locked phone has decided to sleep (its idle timer runs out about
/// 1.4 s after a wake), a new assertion no longer stops it: about 5 s later it
/// announces SleepyTime and closes every connection. A phone that announces
/// it right after being held is left to sleep, woken, and held again.
const SLEEP_DECIDED: Duration = Duration::from_secs(6);
const FALLING_ASLEEP: Duration = Duration::from_secs(3);
const HOLD_ATTEMPTS: usize = 3;
async fn held_awake(provider: &DeviceProvider) -> (Heartbeat, KeepAwake) {
    let mut attempt = 1;
    loop {
        let heartbeat = Heartbeat::start(provider).await;
        let keep_awake = KeepAwake::start(provider).await;
        if attempt == HOLD_ATTEMPTS || !heartbeat.sleeps_within(SLEEP_DECIDED).await {
            return (heartbeat, keep_awake);
        }
        attempt += 1;
        tracing::debug!(
            "The phone was falling asleep as it was held; holding it again, attempt {attempt} of {HOLD_ATTEMPTS}"
        );
        drop((heartbeat, keep_awake));
        tokio::time::sleep(FALLING_ASLEEP).await;
    }
}

/// Answers the phone's heartbeat (Marco, then Polo) while it lives, as
/// Apple's hosts and usbmuxd-compatible muxers do for Wi-Fi devices. Over
/// Wi-Fi the phone closes `com.apple.atc` and `assertion_agent` connections
/// from a host without one. The phone announces sleep on it (SleepyTime) and
/// then closes every service connection; otherwise the connection lapses
/// quietly.
struct Heartbeat {
    task: Option<tokio::task::JoinHandle<()>>,
    slept: Arc<AtomicBool>,
}
impl Heartbeat {
    /// Connects before returning, so later connections find it held.
    async fn start(provider: &DeviceProvider) -> Self {
        let slept = Arc::new(AtomicBool::new(false));
        let task = match HeartbeatClient::connect(provider).await {
            Ok(client) => Some(tokio::spawn(heartbeat(client, Arc::clone(&slept)))),
            Err(error) => {
                tracing::warn!(
                    "The phone's heartbeat did not start, so it may close connections: {error}"
                );
                None
            }
        };
        Self { task, slept }
    }
    /// Whether the phone announces sleep within `window`. A heartbeat that
    /// could not connect, or ended otherwise, never reports it.
    async fn sleeps_within(&self, window: Duration) -> bool {
        let Some(task) = &self.task else {
            return false;
        };
        let deadline = tokio::time::Instant::now() + window;
        while !task.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.slept.load(Ordering::Relaxed)
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
async fn heartbeat(mut client: HeartbeatClient, slept: Arc<AtomicBool>) {
    let mut interval = 10;
    loop {
        match client.get_marco(interval + 5).await {
            Ok(next) => interval = next,
            Err(IdeviceError::Heartbeat(HeartbeatError::SleepyTime)) => {
                tracing::info!("The phone announced that it is going to sleep");
                slept.store(true, Ordering::Relaxed);
                return;
            }
            Err(error) => {
                tracing::debug!("The phone's heartbeat ended: {error}");
                return;
            }
        }
        if let Err(error) = client.send_polo().await {
            tracing::debug!("The phone's heartbeat ended: {error}");
            return;
        }
    }
}

/// A Wi-Fi sync power assertion, as Apple's host holds during Wi-Fi sync
/// (`AMDevicePowerAssertionCreate` with `AMDPowerAssertionTypeWirelessSync`).
/// Without it a locked phone sleeps about two seconds after waking and closes
/// every connection; with it (and the heartbeat answered) it stays awake. The
/// timeout is short and renewed, so a host that dies releases the phone
/// within a minute. The agent answers one command per connection (a second
/// closes it), so each renewal is a new connection, and the previous one is
/// closed only once its successor holds.
struct KeepAwake(Option<tokio::task::JoinHandle<()>>);
const ASSERTION_TIMEOUT: Duration = Duration::from_secs(60);
const ASSERTION_RENEWAL: Duration = Duration::from_secs(20);
const ASSERTION_REPLY: Duration = Duration::from_secs(10);
const MAX_ASSERTION_REPLY: usize = 64 << 10;
impl KeepAwake {
    /// Best effort: a phone that refuses keeps the old behaviour, and the
    /// heartbeat still reports when it sleeps.
    async fn start(provider: &DeviceProvider) -> Self {
        let Some(mut held) = assertion(provider).await else {
            tracing::warn!(
                "The phone did not take a Wi-Fi sync power assertion, so a locked phone may sleep"
            );
            return Self(None);
        };
        let provider = provider.clone();
        Self(Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(ASSERTION_RENEWAL).await;
                match assertion(&provider).await {
                    Some(next) => drop(std::mem::replace(&mut held, next)),
                    None => tracing::debug!(
                        "The Wi-Fi sync power assertion was not renewed; the last one holds until it lapses"
                    ),
                }
            }
        })))
    }
}
async fn assertion(provider: &DeviceProvider) -> Option<Idevice> {
    let AssertionAgent(mut agent) = AssertionAgent::connect(provider).await.ok()?;
    assert(&mut agent).await.then_some(agent)
}
impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

struct AssertionAgent(Idevice);
impl IdeviceService for AssertionAgent {
    fn service_name() -> std::borrow::Cow<'static, str> {
        "com.apple.mobile.assertion_agent".into()
    }
    async fn from_stream(idevice: Idevice) -> Result<Self, IdeviceError> {
        Ok(Self(idevice))
    }
}

/// Creates the assertion and waits for the phone's reply.
async fn assert(agent: &mut Idevice) -> bool {
    let exchange = async {
        agent.send_raw(&frame(&assertion_request())).await.ok()?;
        let header: [u8; 4] = agent.read_raw(4).await.ok()?.try_into().ok()?;
        let length = u32::from_be_bytes(header) as usize;
        if length > MAX_ASSERTION_REPLY {
            return None;
        }
        plist::from_bytes::<plist::Value>(&agent.read_raw(length).await.ok()?).ok()
    };
    matches!(
        tokio::time::timeout(ASSERTION_REPLY, exchange).await,
        Ok(Some(reply)) if accepted(&reply)
    )
}
fn assertion_request() -> plist::Value {
    let mut request = plist::Dictionary::new();
    request.insert("CommandKey".into(), "CommandCreateAssertion".into());
    request.insert(
        "AssertionTypeKey".into(),
        "AMDPowerAssertionTypeWirelessSync".into(),
    );
    request.insert("AssertionNameKey".into(), LABEL.into());
    request.insert(
        "AssertionTimeoutKey".into(),
        plist::Value::Real(ASSERTION_TIMEOUT.as_secs_f64()),
    );
    request.insert("AssertionDetailKey".into(), "Wi-Fi command".into());
    plist::Value::Dictionary(request)
}
/// Lockdown service framing: a big-endian length, then an XML plist.
fn frame(value: &plist::Value) -> Vec<u8> {
    let mut body = Vec::new();
    value
        .to_writer_xml(&mut body)
        .expect("a plist of strings and a real serializes");
    let mut framed = (body.len() as u32).to_be_bytes().to_vec();
    framed.extend(body);
    framed
}
/// The phone answers an empty dictionary; errors come back as `Error`.
fn accepted(reply: &plist::Value) -> bool {
    reply
        .as_dictionary()
        .is_some_and(|reply| !reply.contains_key("Error"))
}

#[cfg(test)]
mod tests;
