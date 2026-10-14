use super::*;
fn row(id: &str, handle: u32, connection_type: Connection) -> UsbmuxdDevice {
    UsbmuxdDevice {
        udid: id.into(),
        device_id: handle,
        connection_type,
    }
}
#[test]
fn exact_usb_selection_preserves_default_ambiguity_refusal() {
    let rows = usb_only(vec![
        row("BBBB", 2, Connection::Usb),
        row("AAAA", 1, Connection::Usb),
        row("WIFI", 3, Connection::Network("127.0.0.1".parse().unwrap())),
    ])
    .unwrap();
    assert_eq!(
        rows.iter().map(|d| d.udid.as_str()).collect::<Vec<_>>(),
        vec!["AAAA", "BBBB"]
    );
    assert!(select_device(rows.clone(), None).is_err());
    assert_eq!(
        select_device(rows.clone(), Some("BBBB")).unwrap().device_id,
        2
    );
    assert!(select_device(rows.clone(), Some("bbbb")).is_err());
    assert!(select_device(rows, Some("WIFI")).is_err());
    assert!(
        select_device(Vec::new(), None)
            .unwrap_err()
            .to_string()
            .contains("no iPhone")
    );
    assert_eq!(
        select_device(vec![row("ONLY", 1, Connection::Usb)], None)
            .unwrap()
            .udid,
        "ONLY"
    );
}
#[test]
fn invalid_device_identifiers_are_refused() {
    for s in ["", " space", "slash/value", "a\0b", "α"] {
        assert!(valid_identifier(s).is_err());
    }
    assert!(valid_identifier(&"A".repeat(129)).is_err());
}
#[test]
fn ambiguous_rows_and_untrusted_socket_values_fail_locally() {
    assert!(
        usb_only(vec![
            row("SAME", 1, Connection::Usb),
            row("SAME", 2, Connection::Usb)
        ])
        .is_err()
    );
    assert!(
        usb_only(vec![
            row("AAAA", 1, Connection::Usb),
            row("BBBB", 1, Connection::Usb)
        ])
        .is_err()
    );
    assert!(usb_only(vec![row("BAD\u{1b}", 1, Connection::Usb)]).is_err());
    assert!(usb_only(vec![row("A", 1, Connection::Usb); 257]).is_err());
    assert!(
        matches!(socket_address(Some("/tmp/private.sock".into())).unwrap(),UsbmuxdAddr::UnixSocket(p) if p=="/tmp/private.sock")
    );
    for s in ["relative.sock", "127.0.0.1:27015", "/tmp/a\0b"] {
        assert!(socket_address(Some(s.into())).is_err());
    }
}

#[tokio::test]
async fn explicit_task_scopes_isolate_selection_and_refuse_unconfigured_discovery() {
    assert!(selection().is_err());
    assert!(address().is_err());
    let left = scope(
        Config::new(Some("LEFT".into()), Some("/tmp/left.sock".into())).unwrap(),
        async {
            tokio::task::yield_now().await;
            assert_eq!(selection().unwrap().as_deref(), Some("LEFT"));
            assert!(
                matches!(address().unwrap(), UsbmuxdAddr::UnixSocket(path) if path == "/tmp/left.sock")
            );
        },
    );
    let right = scope(Config::new(Some("RIGHT".into()), None).unwrap(), async {
        tokio::task::yield_now().await;
        assert_eq!(selection().unwrap().as_deref(), Some("RIGHT"));
    });
    tokio::join!(left, right);
    assert!(selection().is_err());
}

#[tokio::test]
async fn offline_configuration_keeps_socket_validation_at_discovery_boundary() {
    let config = Config::new(None, Some("relative.sock".into())).unwrap();
    scope(config, async {
        assert!(
            address()
                .unwrap_err()
                .to_string()
                .contains("absolute local Unix socket")
        );
    })
    .await;
}

#[tokio::test]
async fn wifi_is_chosen_only_by_explicit_configuration() {
    scope(Config::default(), async {
        assert!(wifi_config().unwrap().is_none());
    })
    .await;
    let address: IpAddr = "192.0.2.16".parse().unwrap();
    let config = Config::new(Some("PHONE".into()), None)
        .unwrap()
        .over_wifi(Some(address), Some("/tmp/remembered".into()));
    scope(config, async move {
        let wifi = wifi_config().unwrap().unwrap();
        assert_eq!(wifi.address, Some(address));
        assert_eq!(wifi.pairing_records, Path::new("/var/lib/lockdown"));
        assert_eq!(
            wifi.remembered.as_deref(),
            Some(Path::new("/tmp/remembered"))
        );
        assert_eq!(selection().unwrap().as_deref(), Some("PHONE"));
        assert!(found().unwrap().is_none());
    })
    .await;
}

#[test]
fn usb_provider_keeps_its_identifier_and_transport() {
    let provider = DeviceProvider::usb(UsbmuxdProvider {
        addr: UsbmuxdAddr::UnixSocket("/tmp/never-opened.sock".into()),
        tag: 0,
        udid: "00008140-AAAA".into(),
        device_id: 7,
        label: "other".into(),
    });
    assert_eq!(provider.udid, "00008140-AAAA");
    assert_eq!(provider.transport(), Transport::Usb);
    assert_eq!(provider.label(), LABEL);
}

fn found_slot() -> *const Mutex<Option<wifi::Found>> {
    CURRENT.try_with(|scope| Arc::as_ptr(&scope.found)).unwrap()
}

#[tokio::test]
async fn nested_scopes_of_one_wifi_configuration_share_the_found_phone() {
    let address: IpAddr = "192.0.2.16".parse().unwrap();
    let wifi = || Config::default().over_wifi(Some(address), None);
    scope(wifi(), async {
        let outer = found_slot();
        // The engine's own nested scope reuses the phone the CLI found first.
        scope(wifi(), async { assert_eq!(found_slot(), outer) }).await;
        // Another transport, address or explicit device discovers on its own.
        scope(Config::default(), async { assert_ne!(found_slot(), outer) }).await;
        let elsewhere = Config::default().over_wifi(Some("192.0.2.17".parse().unwrap()), None);
        scope(elsewhere, async { assert_ne!(found_slot(), outer) }).await;
        let explicit = Config::new(Some("PHONE".into()), None)
            .unwrap()
            .over_wifi(Some(address), None);
        scope(explicit, async { assert_ne!(found_slot(), outer) }).await;
    })
    .await;
    let pinned = || {
        Config::new(Some("PHONE".into()), None)
            .unwrap()
            .over_wifi(Some(address), None)
    };
    scope(pinned(), async {
        let outer = found_slot();
        scope(pinned(), async {
            assert_eq!(found_slot(), outer);
            assert_eq!(selection().unwrap().as_deref(), Some("PHONE"));
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn the_latest_snapshot_is_shared_with_nested_scopes_of_one_configuration() {
    assert!(remembered_snapshot().is_none());
    scope(Config::default(), async {
        assert!(remembered_snapshot().is_none());
        scope(Config::default(), async {
            remember_snapshot(Path::new("/runs/a/snapshot"));
        })
        .await;
        assert_eq!(
            remembered_snapshot().as_deref(),
            Some(Path::new("/runs/a/snapshot"))
        );
        let other = Config::new(None, Some("/tmp/other.sock".into())).unwrap();
        scope(other, async { assert!(remembered_snapshot().is_none()) }).await;
    })
    .await;
    scope(Config::default(), async {
        assert!(remembered_snapshot().is_none())
    })
    .await;
}

/// A socket file nothing serves, as a stopped daemon leaves one. It is bound
/// but never listens, so a connection is refused for certain: a listener that
/// was dropped stays open in a child another test is spawning until that child
/// execs, and accepts connections meanwhile.
fn leave_socket_file(path: &Path) {
    use std::os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    };
    let socket = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    assert!(socket >= 0, "{}", std::io::Error::last_os_error());
    // Closes the descriptor when dropped; the socket file stays.
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let name = path.as_os_str().as_bytes();
    assert!(name.len() < address.sun_path.len());
    for (to, from) in address.sun_path.iter_mut().zip(name) {
        *to = *from as libc::c_char;
    }
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const address).cast(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    assert_eq!(bound, 0, "{}", std::io::Error::last_os_error());
}

#[tokio::test]
async fn a_usbmuxd_socket_nothing_serves_says_how_to_start_it() {
    use std::os::unix::fs::DirBuilderExt;
    // A socket address holds a path of at most 107 bytes: under a long
    // temporary directory, the sockets go in /tmp.
    let base = Some(std::env::temp_dir())
        .filter(|base| base.as_os_str().len() <= 64)
        .unwrap_or_else(|| "/tmp".into());
    let directory = base.join(format!("usbmuxd-stopped-{}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    // No socket file, and one that a stopped daemon left behind.
    let missing = directory.join("missing");
    let stale = directory.join("stale");
    leave_socket_file(&stale);
    for path in [missing, stale] {
        let address = UsbmuxdAddr::UnixSocket(path.to_str().unwrap().to_owned());
        let error = format!("{:#}", enumerate(&address).await.unwrap_err());
        assert!(
            error.starts_with("connecting to usbmuxd, which is not running; plug the iPhone in"),
            "{error}"
        );
    }
    std::fs::remove_dir_all(&directory).unwrap();
}
