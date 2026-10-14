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
    assert_eq!(packet.len(), 6 + 16 * 6);
    assert_eq!(packet[..6], [0xff; 6]);
    assert!(
        packet[6..]
            .chunks(6)
            .all(|chunk| chunk == [0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f])
    );
    // Any datagram to the wake port wakes the phone; a bad address still sends.
    assert_eq!(magic_packet("not a mac"), vec![0xff; 6]);
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
