//! One run per phone, from the outside: a run that reaches a phone another
//! run holds is refused at once, and a lock ends with the process that held
//! it.
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const UDID: &str = "00008030-001A2B3C4D5E6F70";

/// A scratch home of this test's own. A socket address holds a path of at
/// most 107 bytes: under a long temporary directory, it goes in /tmp.
fn scratch(name: &str) -> PathBuf {
    let base = Some(std::env::temp_dir())
        .filter(|base| base.as_os_str().len() <= 64)
        .unwrap_or_else(|| "/tmp".into());
    let directory = base.join(format!("rustunes-phone-lock-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// A usbmuxd at a socket in `directory` that lists one phone over USB and
/// closes any other request, such as for its pairing record.
fn usbmuxd(directory: &Path) -> PathBuf {
    let socket = directory.join("usbmuxd");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut header = [0; 16];
            if stream.read_exact(&mut header).is_err() {
                continue;
            }
            let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize - 16;
            let mut body = vec![0; length];
            if stream.read_exact(&mut body).is_err() {
                continue;
            }
            let request: plist::Dictionary = plist::from_bytes(&body).unwrap();
            if request.get("MessageType").and_then(|v| v.as_string()) != Some("ListDevices") {
                continue;
            }
            let mut properties = plist::Dictionary::new();
            properties.insert("ConnectionType".into(), "USB".into());
            properties.insert("SerialNumber".into(), UDID.into());
            let mut device = plist::Dictionary::new();
            device.insert("DeviceID".into(), 1.into());
            device.insert("Properties".into(), properties.into());
            let mut reply = plist::Dictionary::new();
            reply.insert("DeviceList".into(), vec![plist::Value::from(device)].into());
            let mut body = Vec::new();
            plist::to_writer_xml(&mut body, &reply).unwrap();
            // The request's version, message type and tag.
            let mut packet = (body.len() as u32 + 16).to_le_bytes().to_vec();
            packet.extend_from_slice(&header[4..]);
            packet.extend(body);
            let _ = stream.write_all(&packet);
        }
    });
    socket
}

/// Run `device info` isolated from this host's settings and state, against
/// `socket`; its output and its process.
fn device_info(home: &Path, socket: &Path) -> (Output, u32) {
    let child = Command::new(env!("CARGO_BIN_EXE_rustunes"))
        .args(["device", "info"])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("USBMUXD_SOCKET_ADDRESS", socket)
        .env_remove("RUSTUNES_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let process = child.id();
    (child.wait_with_output().unwrap(), process)
}

/// Whether a run got past the lock: this usbmuxd gives no pairing record.
fn passed_the_lock(output: &Output) -> bool {
    String::from_utf8_lossy(&output.stderr).contains("existing pairing record unavailable")
}

#[test]
fn a_run_is_refused_the_phone_another_run_holds_until_that_run_ends() {
    let home = scratch("refused");
    let socket = usbmuxd(&home);
    let (first, process) = device_info(&home, &socket);
    assert!(
        passed_the_lock(&first),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let locks: Vec<_> = std::fs::read_dir(home.join("state/rustunes/locks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(locks.len(), 1, "{locks:?}");
    assert_eq!(
        std::fs::read_to_string(&locks[0]).unwrap(),
        format!("{process}\n")
    );

    // The first run has exited, so its lock is free for another holder.
    let mut held = File::options().write(true).open(&locks[0]).unwrap();
    held.try_lock().unwrap();
    held.set_len(0).unwrap();
    writeln!(held, "{}", std::process::id()).unwrap();
    let (refused, _) = device_info(&home, &socket);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "\nError: another rustunes run (process {}) is using this phone; wait for it to finish, then run this command again\n",
            std::process::id()
        )),
        "{stderr}"
    );

    drop(held);
    let (after, _) = device_info(&home, &socket);
    assert!(
        passed_the_lock(&after),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    std::fs::remove_dir_all(&home).unwrap();
}
