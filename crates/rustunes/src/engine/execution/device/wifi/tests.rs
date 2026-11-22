use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    sync::atomic::{AtomicU64, Ordering},
};

// Synthetic pairing: the tag is HMAC-SHA256(HKDF-SHA512(HOST_ID), IDENTIFIER)[..8].
const HOST_ID: &str = "8F1C2D3E-0000-4000-8000-000000000001";
const IDENTIFIER: &str = "11111111-2222-3333-4444-555555555555";
const AUTH_TAG: &str = "t+ALA8yR0gY=";
const MAC: &str = "0a:1b:2c:3d:4e:5f";

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "wifi-device-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn record(&self, udid: &str, host_id: &str) {
        let pem = |label: &str| {
            plist::Value::Data(
                format!("-----BEGIN {label}-----\nAAAA\n-----END {label}-----\n").into(),
            )
        };
        let mut record = plist::Dictionary::new();
        for key in ["DeviceCertificate", "HostCertificate", "RootCertificate"] {
            record.insert(key.into(), pem("CERTIFICATE"));
        }
        for key in ["HostPrivateKey", "RootPrivateKey"] {
            record.insert(key.into(), pem("RSA PRIVATE KEY"));
        }
        record.insert("SystemBUID".into(), "SYSTEM".into());
        record.insert("HostID".into(), host_id.into());
        record.insert("WiFiMACAddress".into(), MAC.into());
        plist::Value::Dictionary(record)
            .to_file_xml(self.0.join(format!("{udid}.plist")))
            .unwrap();
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn keys(records: &[Record]) -> Keys {
    records
        .iter()
        .map(|r| {
            (
                r.udid.clone(),
                r.pairing.host_id.as_bytes().to_vec(),
                r.pairing.wifi_mac_address.clone(),
            )
        })
        .collect()
}
/// TXT record data as Bonjour sends it: length-prefixed `key=value` strings.
fn txt(entries: &[&str]) -> TxtProperties {
    let mut rdata = Vec::new();
    for entry in entries {
        rdata.push(entry.len() as u8);
        rdata.extend(entry.as_bytes());
    }
    rdata.as_slice().into()
}
fn addresses(list: &[IpAddr]) -> HashSet<ScopedIp> {
    list.iter().map(|&a| ScopedIp::from(a)).collect()
}

#[test]
fn records_skip_host_configuration_and_follow_the_selection() {
    let temp = Temp::new();
    temp.record("00008140-AAAA", HOST_ID);
    temp.record("00008140-BBBB", "OTHER-HOST");
    fs::write(temp.0.join("SystemConfiguration.plist"), b"not a record").unwrap();
    fs::write(temp.0.join("notes.txt"), b"ignored").unwrap();
    let all = records(&temp.0, None).unwrap();
    assert_eq!(
        all.iter().map(|r| r.udid.as_str()).collect::<Vec<_>>(),
        ["00008140-AAAA", "00008140-BBBB"]
    );
    let one = records(&temp.0, Some("00008140-BBBB")).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].pairing.host_id, "OTHER-HOST");
    assert!(
        records(&temp.0, Some("00008140-CCCC"))
            .err()
            .unwrap()
            .to_string()
            .contains("no pairing record for 00008140-CCCC")
    );
    assert!(records(&temp.0.join("missing"), None).is_err());
}

#[test]
fn unreadable_record_fails_without_echoing_its_contents() {
    let temp = Temp::new();
    fs::write(temp.0.join("00008140-AAAA.plist"), b"HostPrivateKey secret").unwrap();
    let error = records(&temp.0, None).err().unwrap().to_string();
    assert!(error.contains("is unreadable"));
    assert!(!error.contains("secret"));
}

#[test]
fn auth_tags_name_the_record_whose_host_id_derived_them() {
    let temp = Temp::new();
    temp.record("00008140-AAAA", "OTHER-HOST");
    temp.record("00008140-BBBB", HOST_ID);
    let keys = keys(&records(&temp.0, None).unwrap());
    let identifier = format!("identifier={IDENTIFIER}");
    let advertised = txt(&[
        "authTag#1=AAECAwQFBgc=",
        &format!("authTag={AUTH_TAG}"),
        &identifier,
    ]);
    let name = "06:aa:bb:cc:0d:ee@fe80::1-supportsRP-26._apple-mobdev2._tcp.local.";
    assert_eq!(matching(name, &advertised, &keys), Some("00008140-BBBB"));
    let foreign = txt(&["authTag=AAECAwQFBgc=", &identifier]);
    assert_eq!(matching(name, &foreign, &keys), None);
}

#[test]
fn older_phones_match_by_the_wifi_address_in_their_service_name() {
    let temp = Temp::new();
    temp.record("00008140-AAAA", HOST_ID);
    let keys = keys(&records(&temp.0, None).unwrap());
    let hardware = "0A:1B:2C:3D:4E:5F@fe80::1._apple-mobdev2._tcp.local.";
    assert_eq!(matching(hardware, &txt(&[]), &keys), Some("00008140-AAAA"));
    let private = "06:aa:bb:cc:0d:ee@fe80::1._apple-mobdev2._tcp.local.";
    assert_eq!(matching(private, &txt(&[]), &keys), None);
}

#[test]
fn ipv4_is_preferred_over_link_local_ipv6() {
    let v4: IpAddr = "192.0.2.16".parse().unwrap();
    let v6: IpAddr = "fe80::4aa:bbff:fecc:dee".parse().unwrap();
    assert_eq!(preferred(&addresses(&[v6, v4])), Some((v4, 0)));
    assert_eq!(preferred(&addresses(&[v6])).unwrap().0, v6);
    assert_eq!(preferred(&addresses(&[])), None);
}

#[test]
fn magic_packet_repeats_the_hardware_address() {
    let packet = magic_packet(MAC);
    assert_eq!(packet.len(), 102);
    assert_eq!(packet[..6], [0xff; 6]);
    assert!(
        packet[6..]
            .chunks(6)
            .all(|chunk| chunk == [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f])
    );
    assert_eq!(magic_packet(&MAC.to_uppercase()), packet);
    // Any datagram to the wake port wakes the phone; a bad address still sends.
    assert_eq!(magic_packet("not a mac"), vec![0xff; 6]);
    assert_eq!(magic_packet("0a:1b:2c:3d:4e"), vec![0xff; 6]);
}

#[test]
fn the_wifi_mac_is_read_from_the_bonjour_instance_name() {
    let service = "._apple-mobdev2._tcp.local.";
    let named = |instance: &str| name_mac(&format!("{instance}{service}"));
    assert_eq!(
        named("0a:1b:2c:3d:4e:5f@fe80::81b:2cff:fe3d:4e5f"),
        Some(MAC.into())
    );
    assert_eq!(
        named("0a:1b:2c:3d:4e:5f@fe80::81b:2cff:fe3d:4e5f-supportsRP-26"),
        Some(MAC.into())
    );
    assert_eq!(named("0A:1B:2C:3D:4E:5F@fe80::1"), Some(MAC.into()));
    assert_eq!(named("a:1b:2c:3d:4e:5f@fe80::1"), Some(MAC.into()));
    for instance in [
        "iPhone",
        "@fe80::1",
        "0a:1b:2c:3d:4e@fe80::1",
        "0a:1b:2c:3d:4e:5f:60@fe80::1",
        "0a:1b:2c:3d:4e:5g@fe80::1",
        "0a:1b:2c:3d:4e:+f@fe80::1",
        "0a:1b:2c:3d:4e:5ff@fe80::1",
        "0a:1b:2c:3d::5f@fe80::1",
        "0a-1b-2c-3d-4e-5f@fe80::1",
        " 0a:1b:2c:3d:4e:5f@fe80::1",
    ] {
        assert_eq!(named(instance), None, "{instance}");
    }
}

#[test]
fn multicast_wake_follows_apples_three_overlapping_bursts() {
    let mut expected: Vec<Duration> = [100, 1100, 4100]
        .into_iter()
        .flat_map(|start| (0..18).map(move |packet| Duration::from_millis(start + 100 * packet)))
        .collect();
    expected.sort();
    let schedule = wake_schedule();
    assert_eq!(schedule, expected);
    assert_eq!(schedule.len(), 54);
    assert_eq!(schedule[0], Duration::from_millis(100));
    assert_eq!(schedule[53], Duration::from_millis(5800));
    assert!(schedule[53] < WOKEN_BROWSE);
}

#[test]
fn the_browse_waits_longer_only_after_a_wake() {
    let address: IpAddr = "192.0.2.16".parse().unwrap();
    assert_eq!(Wake::default().window(), BROWSE);
    let by_address = Wake {
        addresses: vec![("00008140-AAAA".into(), address, MAC.into())],
        macs: Vec::new(),
    };
    assert_eq!(by_address.window(), WOKEN_BROWSE);
    let by_mac = Wake {
        addresses: Vec::new(),
        macs: vec![MAC.into()],
    };
    assert_eq!(by_mac.window(), WOKEN_BROWSE);
    assert!(BROWSE < WOKEN_BROWSE);
}

#[test]
fn multicast_leaves_by_every_up_multicast_interface_but_loopback_and_tunnels() {
    let up = (libc::IFF_UP | libc::IFF_MULTICAST) as libc::c_uint;
    assert!(wakes_through(up));
    assert!(wakes_through(up | libc::IFF_RUNNING as libc::c_uint));
    assert!(wakes_through(up | libc::IFF_BROADCAST as libc::c_uint));
    assert!(!wakes_through(up | libc::IFF_LOOPBACK as libc::c_uint));
    assert!(!wakes_through(up | libc::IFF_POINTOPOINT as libc::c_uint));
    assert!(!wakes_through(libc::IFF_UP as libc::c_uint));
    assert!(!wakes_through(libc::IFF_MULTICAST as libc::c_uint));
    // Listing the interfaces sends nothing.
    let interfaces = multicast_interfaces();
    assert!(interfaces.iter().all(|i| !i.address.is_loopback()));
    let mut indexes: Vec<u32> = interfaces.iter().map(|i| i.index).collect();
    indexes.sort();
    indexes.dedup();
    assert_eq!(indexes.len(), interfaces.len());
}

#[test]
fn a_wake_socket_stays_on_its_link_and_names_its_interface() {
    // Loopback only, and nothing is sent.
    // SAFETY: a NUL-terminated name.
    let index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    assert_ne!(index, 0);
    let socket = wake_socket(&Interface {
        name: "lo".into(),
        index,
        address: Ipv4Addr::LOCALHOST,
    })
    .unwrap();
    assert_eq!(socket.multicast_ttl_v4().unwrap(), 1);
    let mut chosen = libc::in_addr { s_addr: 0 };
    let mut length = size_of::<libc::in_addr>() as libc::socklen_t;
    // SAFETY: a live socket, and a buffer of the length passed.
    let status = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw mut chosen).cast(),
            &mut length,
        )
    };
    assert_eq!(status, 0);
    assert_eq!(
        Ipv4Addr::from(chosen.s_addr.to_ne_bytes()),
        Ipv4Addr::LOCALHOST
    );
}

#[test]
fn remembered_addresses_are_private_and_never_fail_a_run() {
    let temp = Temp::new();
    let remembered = Remembered(Some(temp.0.join("wifi")));
    assert_eq!(remembered.get("00008140-AAAA"), None);
    let address: IpAddr = "192.0.2.16".parse().unwrap();
    remembered.set("00008140-AAAA", address);
    assert_eq!(remembered.get("00008140-AAAA"), Some(address));
    let directory = fs::metadata(temp.0.join("wifi")).unwrap();
    assert_eq!(directory.permissions().mode() & 0o777, 0o700);
    let file = fs::metadata(temp.0.join("wifi/00008140-AAAA")).unwrap();
    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    let moved: IpAddr = "192.0.2.20".parse().unwrap();
    remembered.set("00008140-AAAA", moved);
    assert_eq!(remembered.get("00008140-AAAA"), Some(moved));
    fs::write(temp.0.join("wifi/00008140-BBBB"), b"garbage").unwrap();
    assert_eq!(remembered.get("00008140-BBBB"), None);
    // Unwritable or absent storage is ignored.
    Remembered(Some(temp.0.join("wifi/00008140-AAAA/below-a-file"))).set("X", address);
    Remembered(None).set("X", address);
    assert_eq!(Remembered(None).get("X"), None);
}

#[test]
fn remembered_wifi_macs_sit_beside_the_address_without_touching_it() {
    use std::os::unix::fs::MetadataExt;
    let temp = Temp::new();
    let remembered = Remembered(Some(temp.0.join("wifi")));
    assert_eq!(remembered.mac("00008140-AAAA"), None);
    remembered.set_mac("00008140-AAAA", MAC);
    assert_eq!(remembered.mac("00008140-AAAA"), Some(MAC.into()));
    let directory = fs::metadata(temp.0.join("wifi")).unwrap();
    assert_eq!(directory.permissions().mode() & 0o777, 0o700);
    let path = temp.0.join("wifi/00008140-AAAA.mac");
    let file = fs::metadata(&path).unwrap();
    assert_eq!(file.permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_to_string(&path).unwrap(), format!("{MAC}\n"));
    // Older builds read the address file whole as an address.
    assert!(!temp.0.join("wifi/00008140-AAAA").exists());
    let address: IpAddr = "192.0.2.16".parse().unwrap();
    remembered.set("00008140-AAAA", address);
    let private = "06:aa:bb:cc:0d:ee";
    remembered.set_mac("00008140-AAAA", private);
    assert_eq!(
        fs::read_to_string(temp.0.join("wifi/00008140-AAAA")).unwrap(),
        "192.0.2.16\n"
    );
    assert_eq!(remembered.get("00008140-AAAA"), Some(address));
    assert_eq!(remembered.mac("00008140-AAAA"), Some(private.into()));
    // An unchanged MAC is not written again; a new one replaces the file.
    let inode = fs::metadata(&path).unwrap().ino();
    remembered.set_mac("00008140-AAAA", private);
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    remembered.set_mac("00008140-AAAA", MAC);
    assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
    let names: Vec<_> = fs::read_dir(temp.0.join("wifi"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "no temporary file is left: {names:?}");
    fs::write(temp.0.join("wifi/00008140-BBBB.mac"), b"garbage").unwrap();
    assert_eq!(remembered.mac("00008140-BBBB"), None);
    fs::write(
        temp.0.join("wifi/00008140-BBBB.mac"),
        b"0A:1B:2C:3D:4E:5F\n",
    )
    .unwrap();
    assert_eq!(remembered.mac("00008140-BBBB"), Some(MAC.into()));
    // Unwritable or absent storage is ignored.
    Remembered(Some(path.join("below-a-file"))).set_mac("X", MAC);
    Remembered(None).set_mac("X", MAC);
    assert_eq!(Remembered(None).mac("X"), None);
}

#[test]
fn every_phone_is_woken_by_what_is_remembered_of_it() {
    let temp = Temp::new();
    for udid in ["00008140-AAAA", "00008140-BBBB", "00008140-CCCC"] {
        temp.record(udid, HOST_ID);
    }
    let records = records(&temp.0, None).unwrap();
    let remembered = Remembered(Some(temp.0.join("wifi")));
    let a: IpAddr = "192.0.2.16".parse().unwrap();
    let b: IpAddr = "192.0.2.20".parse().unwrap();
    let private = "06:aa:bb:cc:0d:ee";
    remembered.set("00008140-AAAA", a);
    remembered.set("00008140-BBBB", b);
    remembered.set_mac("00008140-BBBB", private);
    remembered.set_mac("00008140-CCCC", "06:11:22:33:44:55");
    let wake = remembered.known(&records);
    // At a last address the packet carries the MAC the phone last used, or
    // the record's hardware MAC.
    assert_eq!(
        wake.addresses,
        [
            ("00008140-AAAA".into(), a, MAC.into()),
            ("00008140-BBBB".into(), b, private.into())
        ]
    );
    assert_eq!(wake.macs, [private, "06:11:22:33:44:55"]);
    assert_eq!(
        Remembered(None).known(&records).window(),
        BROWSE,
        "nothing remembered, no wake"
    );
}

fn answer(address: &str, mac: Option<&str>) -> Answer {
    Answer {
        address: address.parse().unwrap(),
        scope_id: 0,
        mac: mac.map(str::to_owned),
    }
}
fn last(udid: &str, address: &str) -> (String, IpAddr, String) {
    (udid.into(), address.parse().unwrap(), MAC.into())
}

#[test]
fn with_no_bonjour_answer_the_one_phone_with_a_last_address_is_tried() {
    let temp = Temp::new();
    temp.record("00008140-AAAA", HOST_ID);
    temp.record("00008140-BBBB", "OTHER-HOST");
    let records = records(&temp.0, None).unwrap();
    let remembered = Remembered(Some(temp.0.join("wifi")));
    remembered.set("00008140-BBBB", "192.0.2.20".parse().unwrap());
    remembered.set_mac("00008140-AAAA", "06:aa:bb:cc:0d:ee");
    let wake = remembered.known(&records);
    assert_eq!(
        choose(BTreeMap::new(), &wake).unwrap(),
        ("00008140-BBBB".into(), answer("192.0.2.20", None))
    );
}

#[test]
fn a_bonjour_answer_beats_a_last_address() {
    let wake = Wake {
        addresses: vec![last("00008140-AAAA", "192.0.2.16")],
        macs: Vec::new(),
    };
    let found = BTreeMap::from([("00008140-BBBB".into(), answer("192.0.2.30", Some(MAC)))]);
    assert_eq!(
        choose(found, &wake).unwrap(),
        ("00008140-BBBB".into(), answer("192.0.2.30", Some(MAC)))
    );
    let two = BTreeMap::from([
        ("00008140-AAAA".into(), answer("192.0.2.16", None)),
        ("00008140-BBBB".into(), answer("192.0.2.30", None)),
    ]);
    let error = choose(two, &wake).unwrap_err().to_string();
    assert!(error.contains("multiple paired iPhones found"), "{error}");
}

#[test]
fn several_last_addresses_and_no_answer_ask_for_a_device() {
    let wake = Wake {
        addresses: vec![
            last("00008140-AAAA", "192.0.2.16"),
            last("00008140-BBBB", "192.0.2.20"),
        ],
        macs: Vec::new(),
    };
    let error = choose(BTreeMap::new(), &wake).unwrap_err().to_string();
    assert!(error.contains("--device UDID"), "{error}");
    assert!(error.contains("no paired iPhone answered"), "{error}");
    assert!(!error.contains("found"), "none answered: {error}");
}

#[test]
fn no_answer_says_what_was_tried_and_how_to_be_found_again() {
    let unanswered = |wake: &Wake| choose(BTreeMap::new(), wake).unwrap_err().to_string();
    let first = unanswered(&Wake::default());
    assert!(
        first.contains("no saved address or Wi-Fi identity"),
        "{first}"
    );
    assert!(!first.contains("tried"), "{first}");
    let by_mac = unanswered(&Wake {
        addresses: Vec::new(),
        macs: vec![MAC.into()],
    });
    assert!(
        by_mac.contains("tried to wake it by its last Wi-Fi identity;"),
        "{by_mac}"
    );
    let both = no_answer(&Wake {
        addresses: vec![last("00008140-AAAA", "192.0.2.16")],
        macs: vec![MAC.into()],
    });
    assert!(
        both.contains("at its last address and by its last Wi-Fi identity"),
        "{both}"
    );
    let by_address = no_answer(&Wake {
        addresses: vec![last("00008140-AAAA", "192.0.2.16")],
        macs: Vec::new(),
    });
    assert!(
        by_address.contains("tried to wake it at its last address;"),
        "{by_address}"
    );
    for message in [&first, &by_mac, &both, &by_address] {
        assert!(message.to_lowercase().contains("unlock"), "{message}");
        assert!(message.contains("--wifi-address"), "{message}");
        assert!(message.contains("trust it over USB"), "{message}");
        assert!(!message.to_lowercase().contains("private"), "{message}");
    }
}

#[test]
fn power_assertion_asks_for_wireless_sync_with_a_short_timeout() {
    let framed = frame(&assertion_request());
    let length = u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize;
    assert_eq!(length, framed.len() - 4);
    let request: plist::Dictionary = plist::from_bytes(&framed[4..]).unwrap();
    let text = |key: &str| request[key].as_string().unwrap().to_owned();
    assert_eq!(text("CommandKey"), "CommandCreateAssertion");
    assert_eq!(
        text("AssertionTypeKey"),
        "AMDPowerAssertionTypeWirelessSync"
    );
    assert_eq!(text("AssertionNameKey"), LABEL);
    assert_eq!(request["AssertionTimeoutKey"].as_real(), Some(60.0));
    assert!(ASSERTION_RENEWAL * 2 < ASSERTION_TIMEOUT);
    assert!(accepted(
        &plist::Value::Dictionary(plist::Dictionary::new())
    ));
    let mut refused = plist::Dictionary::new();
    refused.insert("Error".into(), "InvalidRequest".into());
    assert!(!accepted(&plist::Value::Dictionary(refused)));
    assert!(!accepted(&plist::Value::String("{}".into())));
}

#[tokio::test(start_paused = true)]
async fn a_phone_announcing_sleep_right_after_being_held_is_noticed() {
    let heartbeat = |task: Option<tokio::task::JoinHandle<()>>, slept: Arc<AtomicBool>| Heartbeat {
        task,
        slept,
    };
    let slept = Arc::new(AtomicBool::new(false));
    let announce = Arc::clone(&slept);
    let falling_asleep = heartbeat(
        Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            announce.store(true, Ordering::Relaxed);
        })),
        slept,
    );
    assert!(falling_asleep.sleeps_within(SLEEP_DECIDED).await);
    let quiet = heartbeat(
        Some(tokio::spawn(std::future::pending())),
        Arc::new(AtomicBool::new(false)),
    );
    let started = tokio::time::Instant::now();
    assert!(!quiet.sleeps_within(SLEEP_DECIDED).await);
    assert!(started.elapsed() >= SLEEP_DECIDED);
    let ended = heartbeat(
        Some(tokio::spawn(async {})),
        Arc::new(AtomicBool::new(false)),
    );
    let started = tokio::time::Instant::now();
    assert!(!ended.sleeps_within(SLEEP_DECIDED).await);
    assert!(started.elapsed() < SLEEP_DECIDED);
    assert!(
        !heartbeat(None, Arc::new(AtomicBool::new(false)))
            .sleeps_within(SLEEP_DECIDED)
            .await
    );
}

#[tokio::test]
async fn phone_route_hides_its_pairing_record_from_debug_output() {
    let temp = Temp::new();
    temp.record("00008140-AAAA", HOST_ID);
    let record = records(&temp.0, None).unwrap().remove(0);
    let phone = Phone {
        address: "192.0.2.16".parse().unwrap(),
        scope_id: 0,
        pairing: Arc::new(record.pairing),
    };
    let shown = format!("{phone:?}");
    assert!(shown.contains("192.0.2.16"));
    assert!(!shown.contains(HOST_ID));
    assert!(!shown.contains("PRIVATE"));
    assert_eq!(phone.pairing_file().await.unwrap().host_id, HOST_ID);
}
