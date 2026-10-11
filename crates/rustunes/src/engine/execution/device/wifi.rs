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

/// One usbmuxd pairing record: the phone's UDID (the file name) and secrets.
struct Record {
    udid: String,
    pairing: PairingFile,
}

type Keys = Vec<(String, Vec<u8>, String)>;
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

/// Once a locked phone has decided to sleep (its idle timer runs out about
/// 1.4 s after a wake), a new assertion no longer stops it: about 5 s later it
/// announces SleepyTime and closes every connection. A phone that announces
/// it right after being held is left to sleep, woken, and held again.
const SLEEP_DECIDED: Duration = Duration::from_secs(6);
const FALLING_ASLEEP: Duration = Duration::from_secs(3);
const HOLD_ATTEMPTS: usize = 3;

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
