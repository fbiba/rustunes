//! Wi-Fi discovery. A paired phone advertises `_apple-mobdev2._tcp` over
//! Bonjour and accepts Lockdown on TCP 62078; sessions use the pairing record
//! usbmuxd saved when the phone was paired and trusted over USB. Services are
//! reached on the TCP ports Lockdown names, exactly as usbmuxd forwards them.
//!
//! A sleeping phone ignores Bonjour and new TCP connections, and answers
//! broadcast ARP late or not at all. Two datagrams wake it within seconds: a
//! magic packet for the Wi-Fi MAC address it uses now, multicast to Bonjour's
//! group as Apple's hosts send it, and any datagram to the Wake-on-LAN port at
//! its address. Its Bonjour name starts with that MAC, so the MAC and the
//! address each phone last answered with are remembered. A locked phone then
//! stays awake only while this host answers its heartbeat and holds a power
//! assertion.
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
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Where usbmuxd keeps pairing records on Linux, one `<UDID>.plist` each.
pub(super) const PAIRING_RECORDS: &str = "/var/lib/lockdown";
const SERVICE: &str = "_apple-mobdev2._tcp.local.";
const LOCKDOWN_PORT: u16 = 62078;
/// The phone's radio wakes its system for unicast datagrams to the echo (7)
/// or discard (9) port, whatever they carry, and for a magic packet for the
/// Wi-Fi MAC it uses now, multicast to `WAKE_GROUP` on the discard port.
/// Subnet broadcasts, other ports and other MACs do not wake it.
const WAKE_PORT: u16 = 9;
/// Bonjour's IPv4 group, where Apple's hosts multicast the wake.
const WAKE_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// Apple's Bonjour sends a burst of magic packets with each of the first
/// three queries of a resolve that asks it to wake the phone, so the bursts
/// overlap.
const WAKE_BURSTS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(1100),
    Duration::from_millis(4100),
];
const BURST_PACKETS: u32 = 18;
const BURST_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RECORD_BYTES: u64 = 1 << 20;
/// How long to wait for Bonjour answers. A selected phone ends the wait as
/// soon as it resolves; otherwise every answering phone is collected.
const BROWSE: Duration = Duration::from_secs(4);
/// The wait once a wake was sent: the last burst leaves about 6 s in, and a
/// woken phone announces itself a second or two after the packet that woke
/// it.
const WOKEN_BROWSE: Duration = Duration::from_secs(10);
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
/// Six 0xff bytes, then the MAC sixteen times: 102 bytes. An unreadable MAC
/// leaves the six 0xff bytes, as any datagram to the wake port at the phone's
/// address wakes it.
fn magic_packet(mac: &str) -> Vec<u8> {
    let mut packet = vec![0xff; 6];
    if let Some(octets) = mac_octets(mac) {
        for _ in 0..16 {
            packet.extend(octets);
        }
    }
    packet
}
fn mac_octets(mac: &str) -> Option<[u8; 6]> {
    let mut octets = [0; 6];
    let mut texts = mac.split(':');
    for octet in &mut octets {
        let text = texts.next()?;
        if !(1..=2).contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        *octet = u8::from_str_radix(text, 16).ok()?;
    }
    texts.next().is_none().then_some(octets)
}
/// A MAC written as six hex octets separated by `:`, in lowercase.
fn normal_mac(mac: &str) -> Option<String> {
    let octets = mac_octets(mac)?.map(|octet| format!("{octet:02x}"));
    Some(octets.join(":"))
}
/// The Wi-Fi MAC a phone's Bonjour instance name starts with:
/// `<MAC>@<IPv6>`, with `-supportsRP-<n>` after it on newer phones. It is the
/// MAC the phone uses on this network now, the one its wake answers to.
fn name_mac(fullname: &str) -> Option<String> {
    let (mac, _) = fullname.split_once('@')?;
    normal_mac(mac)
}

/// What this host learned of each phone the last time it answered, in two
/// files per UDID: `<UDID>` holds its address and `<UDID>.mac` the Wi-Fi MAC
/// its Bonjour name carried. Older builds read the first as an address, so it
/// holds nothing else. Failing to read or write them never fails a run.
struct Remembered(Option<PathBuf>);
impl Remembered {
    fn get(&self, udid: &str) -> Option<IpAddr> {
        self.read(udid)?.parse().ok()
    }
    fn mac(&self, udid: &str) -> Option<String> {
        normal_mac(&self.read(&mac_file(udid))?)
    }
    fn read(&self, name: &str) -> Option<String> {
        let path = self.0.as_ref()?.join(name);
        Some(std::fs::read_to_string(path).ok()?.trim().to_owned())
    }
    /// What there is to wake these phones by.
    fn known(&self, records: &[Record]) -> Wake {
        let mut known = Wake::default();
        for record in records {
            let mac = self.mac(&record.udid);
            if let Some(address) = self.get(&record.udid) {
                let payload = mac.as_deref().unwrap_or(&record.pairing.wifi_mac_address);
                known
                    .addresses
                    .push((record.udid.clone(), address, payload.to_owned()));
            }
            known.macs.extend(mac);
        }
        tracing::debug!(
            "{} last address(es) and {} Wi-Fi MAC(s) remembered to wake the phone(s) by",
            known.addresses.len(),
            known.macs.len()
        );
        known
    }
    fn set(&self, udid: &str, address: IpAddr) {
        if self.get(udid) != Some(address) {
            self.write(udid, &address.to_string(), "Wi-Fi address");
        }
    }
    fn set_mac(&self, udid: &str, mac: &str) {
        if self.mac(udid).as_deref() != Some(mac) {
            self.write(&mac_file(udid), mac, "Wi-Fi MAC");
        }
    }
    /// Replaces the file whole, so a reader never sees half a value.
    fn write(&self, name: &str, value: &str, what: &str) {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let Some(directory) = &self.0 else {
            return;
        };
        let write = || -> std::io::Result<()> {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
            let temporary = directory.join(format!(".{name}.tmp"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            std::io::Write::write_all(&mut file, format!("{value}\n").as_bytes())?;
            std::fs::rename(temporary, directory.join(name))
        };
        if let Err(error) = write() {
            tracing::debug!("The phone's {what} was not remembered: {error}");
        }
    }
}
fn mac_file(udid: &str) -> String {
    format!("{udid}.mac")
}

/// What this host remembers to wake the paired phones by.
#[derive(Default)]
struct Wake {
    /// Each phone's last address, with the MAC its magic packet carries
    /// there: the one its Bonjour name last carried, or the record's
    /// hardware MAC.
    addresses: Vec<(String, IpAddr, String)>,
    /// The MACs their Bonjour names last carried, for the multicast wake.
    macs: Vec<String>,
}
impl Wake {
    /// A unicast datagram to each phone's last address, now.
    fn unicast(&self) {
        for (_, address, mac) in &self.addresses {
            wake(socket_address(*address, 0, WAKE_PORT), mac);
        }
    }
    /// How long to browse: long enough for a woken phone to announce itself
    /// when a wake was sent.
    fn window(&self) -> Duration {
        if self.addresses.is_empty() && self.macs.is_empty() {
            BROWSE
        } else {
            WOKEN_BROWSE
        }
    }
}

/// When each multicast wake leaves, from the start of the browse: Apple's
/// three bursts, merged in order.
fn wake_schedule() -> Vec<Duration> {
    let mut schedule: Vec<Duration> = WAKE_BURSTS
        .iter()
        .flat_map(|&start| (0..BURST_PACKETS).map(move |packet| start + BURST_INTERVAL * packet))
        .collect();
    schedule.sort();
    schedule
}
/// Apple's wake, which needs no address: a magic packet for each remembered
/// MAC, multicast on every interface on Apple's schedule, until the schedule
/// ends or the browse does and drops the sender of `stop`. Failures are only
/// logged.
fn multicast_wake(macs: &[String], stop: &mpsc::Receiver<()>) {
    let start = Instant::now();
    if macs.is_empty() {
        return;
    }
    let sockets: Vec<(String, UdpSocket)> = multicast_interfaces()
        .into_iter()
        .filter_map(|interface| match wake_socket(&interface) {
            Ok(socket) => Some((interface.name, socket)),
            Err(error) => {
                tracing::debug!("No multicast wake on {}: {error}", interface.name);
                None
            }
        })
        .collect();
    tracing::debug!(
        "Multicast wake for {} phone(s) on {} interface(s): {}",
        macs.len(),
        sockets.len(),
        sockets
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if sockets.is_empty() {
        return;
    }
    let packets: Vec<Vec<u8>> = macs.iter().map(|mac| magic_packet(mac)).collect();
    let mut failed = HashSet::new();
    for at in wake_schedule() {
        let wait = (start + at).saturating_duration_since(Instant::now());
        if !matches!(
            stop.recv_timeout(wait),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            return;
        }
        for (name, socket) in &sockets {
            for packet in &packets {
                if let Err(error) = socket.send_to(packet, (WAKE_GROUP, WAKE_PORT))
                    && failed.insert(name.as_str())
                {
                    tracing::debug!("The multicast wake failed on {name}: {error}");
                }
            }
        }
    }
}

/// An interface the multicast wake leaves by.
struct Interface {
    name: String,
    index: u32,
    address: Ipv4Addr,
}
/// Every interface that is up, not loopback or point-to-point and can
/// multicast, with an IPv4 address, once each. The phone's network need not
/// hold the default route: a VPN may.
fn multicast_interfaces() -> Vec<Interface> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `list` is a valid place for the list's head; it is freed below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        tracing::debug!(
            "The network interfaces could not be listed: {}",
            std::io::Error::last_os_error()
        );
        return Vec::new();
    }
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut next = list;
    while !next.is_null() {
        // SAFETY: a node of the list getifaddrs returned, freed only below.
        let entry = unsafe { &*next };
        next = entry.ifa_next;
        if !wakes_through(entry.ifa_flags) || entry.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: a non-null address of a live node.
        if i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: an `AF_INET` address is a `sockaddr_in`.
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        // SAFETY: a live node's name is a NUL-terminated string.
        let (name, index) = unsafe {
            (
                std::ffi::CStr::from_ptr(entry.ifa_name),
                libc::if_nametoindex(entry.ifa_name),
            )
        };
        if index != 0 && !interfaces.iter().any(|known| known.index == index) {
            interfaces.push(Interface {
                name: name.to_string_lossy().into_owned(),
                index,
                address: Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()),
            });
        }
    }
    // SAFETY: the list getifaddrs returned, freed once; nothing borrowed from
    // it is used after this.
    unsafe { libc::freeifaddrs(list) };
    interfaces
}
/// Up, not loopback or point-to-point, and able to multicast. A link-local
/// multicast cannot wake a phone across a routed tunnel, and would show its
/// MAC to the tunnel's far end.
fn wakes_through(flags: libc::c_uint) -> bool {
    let flags = flags as libc::c_int;
    flags & libc::IFF_UP != 0
        && flags & libc::IFF_LOOPBACK == 0
        && flags & libc::IFF_POINTOPOINT == 0
        && flags & libc::IFF_MULTICAST != 0
}
/// A socket whose multicasts leave by `interface` and stay on its link.
fn wake_socket(interface: &Interface) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind((interface.address, 0))?;
    socket.set_multicast_ttl_v4(1)?;
    let request = libc::ip_mreqn {
        imr_multiaddr: libc::in_addr { s_addr: 0 },
        imr_address: libc::in_addr {
            s_addr: u32::from_ne_bytes(interface.address.octets()),
        },
        imr_ifindex: interface.index as libc::c_int,
    };
    // SAFETY: a live socket, and an option of the type and size the kernel
    // reads for `IP_MULTICAST_IF`.
    let status = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const request).cast(),
            std::mem::size_of_val(&request) as libc::socklen_t,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(socket)
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
    let (record, answer, unanswered) = match config.address {
        Some(address) => (
            record_at(address, records).await?,
            Answer::at(address),
            None,
        ),
        None => {
            let wake = remembered.known(&records);
            wake.unicast();
            let found = browse(&records, selected, &wake).await?;
            // A phone tried at its last address after no Bonjour answer may
            // not answer there either.
            let unanswered = found.is_empty().then(|| no_answer(&wake));
            let (udid, answer) = choose(found, &wake)?;
            let record = records
                .into_iter()
                .find(|record| record.udid == udid)
                .expect("chosen phones come from the records");
            (record, answer, unanswered)
        }
    };
    let provider = DeviceProvider {
        udid: record.udid,
        route: Route::Wifi(Phone {
            address: answer.address,
            scope_id: answer.scope_id,
            pairing: Arc::new(record.pairing),
        }),
    };
    super::lock_phone(&provider.udid)?;
    confirm(&provider, unanswered).await?;
    remembered.set(&provider.udid, answer.address);
    // A name without a MAC leaves the last one remembered.
    if let Some(mac) = &answer.mac {
        remembered.set_mac(&provider.udid, mac);
    }
    let (heartbeat, keep_awake) = held_awake(&provider).await;
    Ok(Found {
        provider,
        heartbeat,
        _keep_awake: keep_awake,
    })
}

/// The phone to open: the one paired phone that answered Bonjour or, when
/// none did, the one with a last address to try. A paired session then
/// proves which phone is there.
fn choose(mut found: BTreeMap<String, Answer>, wake: &Wake) -> Result<(String, Answer)> {
    if found.is_empty() {
        return match wake.addresses.as_slice() {
            [(udid, address, _)] => {
                // Still asleep or not announcing: try where it last answered.
                tracing::debug!(
                    "No Bonjour answer; trying {address}, where the phone last answered"
                );
                Ok((udid.clone(), Answer::at(*address)))
            }
            [] => bail!(no_answer(wake)),
            _ => bail!(
                "no paired iPhone answered over Wi-Fi, and several have a last address to try; select one with --device UDID"
            ),
        };
    }
    ensure!(
        found.len() == 1,
        "multiple paired iPhones found over Wi-Fi; select the intended device with --device UDID"
    );
    Ok(found.pop_first().expect("one phone"))
}
/// Why no phone answered: what was tried, and how rustunes learns the phone
/// again.
fn no_answer(wake: &Wake) -> String {
    const UNTRUSTED: &str = "If it is awake and still does not answer, it may no longer trust this computer: pair and trust it over USB, then run again";
    let tried = match (wake.addresses.is_empty(), wake.macs.is_empty()) {
        (true, true) => {
            return format!(
                "no paired iPhone answered over Wi-Fi, and rustunes has no saved address or Wi-Fi identity to wake a locked one by; unlock the phone on this network and run again, so rustunes learns them, or pass --wifi-address. {UNTRUSTED}"
            );
        }
        (false, true) => "at its last address",
        (true, false) => "by its last Wi-Fi identity",
        (false, false) => "at its last address and by its last Wi-Fi identity",
    };
    format!(
        "no paired iPhone answered over Wi-Fi after rustunes tried to wake it {tried}; it may have a new address or Wi-Fi identity since it last answered. Unlock it once on this network and run again, so rustunes learns them, or pass --wifi-address. {UNTRUSTED}"
    )
}

/// Paired phones answering Bonjour, woken as `find` wakes them, without
/// opening a session.
pub(super) async fn list(config: &WifiConfig) -> Result<Vec<Listed>> {
    let records = records(&config.pairing_records, None)?;
    let wake = Remembered(config.remembered.clone()).known(&records);
    wake.unicast();
    Ok(browse(&records, None, &wake)
        .await?
        .into_iter()
        .map(|(udid, answer)| Listed {
            udid,
            transport: Transport::Wifi(answer.address),
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

/// A paired phone's Bonjour answer.
#[derive(Debug, PartialEq)]
struct Answer {
    address: IpAddr,
    scope_id: u32,
    /// The Wi-Fi MAC its instance name starts with, if it does.
    mac: Option<String>,
}
impl Answer {
    /// A phone reached at an address without a Bonjour answer.
    fn at(address: IpAddr) -> Self {
        Self {
            address,
            scope_id: 0,
            mac: None,
        }
    }
}

/// Bonjour answers matched to pairing records, by UDID. iOS 26.4 and later
/// publish an identifier and auth tags derived from the record's HostID;
/// earlier versions name the service after the phone's Wi-Fi MAC address.
/// Apple's multicast wake runs alongside for the remembered MACs.
async fn browse(
    records: &[Record],
    selected: Option<&str>,
    wake: &Wake,
) -> Result<BTreeMap<String, Answer>> {
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
    let window = wake.window();
    let macs = wake.macs.clone();
    tokio::task::spawn_blocking(move || browse_blocking(&keys, selected.as_deref(), window, &macs))
        .await
        .context("Bonjour discovery task failed")?
}
fn browse_blocking(
    keys: &Keys,
    selected: Option<&str>,
    window: Duration,
    macs: &[String],
) -> Result<BTreeMap<String, Answer>> {
    let daemon = ServiceDaemon::new().context("starting Bonjour discovery")?;
    let events = daemon.browse(SERVICE).context("browsing Bonjour")?;
    tracing::debug!("Browsing Bonjour for up to {} s", window.as_secs());
    let deadline = Instant::now() + window;
    let mut found = BTreeMap::new();
    let mut answers = 0;
    let (stop, stopped) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || multicast_wake(macs, &stopped));
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
                    // Other Apple devices answer too, and so does a paired
                    // phone that no longer trusts this computer: its tags
                    // match none.
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
            found.insert(
                udid.to_owned(),
                Answer {
                    address: address.0,
                    scope_id: address.1,
                    mac: name_mac(service.get_fullname()),
                },
            );
            // Only paired phones can match, so once each has answered (or
            // the selected one has) nothing else can turn up.
            if selected == Some(udid) || found.len() == keys.len() {
                break;
            }
        }
        // The wake ends with the browse.
        drop(stop);
    });
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
/// `unanswered` explains a phone that does not answer at all.
async fn confirm(provider: &DeviceProvider, unanswered: Option<String>) -> Result<()> {
    let Transport::Wifi(address) = provider.transport() else {
        unreachable!("Wi-Fi providers only");
    };
    let connected = LockdownClient::connect(provider)
        .await
        .with_context(|| format!("connecting to Lockdown at {address} over Wi-Fi"));
    let mut lockdown = match unanswered {
        Some(unanswered) => connected.context(unanswered)?,
        None => connected?,
    };
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
