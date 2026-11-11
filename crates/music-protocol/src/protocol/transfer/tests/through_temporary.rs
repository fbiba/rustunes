//! Writing a new file through `<name>.tmp`, as Apple Devices writes its prefs.
use super::*;
use crate::test_support::afc::{FakePhone, Node};

const PREFS: &str = "iTunes_Control/iTunes/iTunesPrefs";

async fn run(phone: FakePhone, unlock: bool, bytes: &[u8]) -> (Result<[u8; 32]>, FakePhone) {
    let trace = Temporary::new();
    let (mut transfer, peer) = pair(&trace.0, Duration::from_secs(5));
    if unlock {
        transfer.unlock_writes();
    }
    let server = tokio::spawn(phone.serve(peer));
    let result = transfer.write_new_through_temporary(PREFS, bytes).await;
    drop(transfer);
    (result, server.await.unwrap())
}

#[tokio::test]
async fn writes_the_temporary_then_renames_it_and_reads_it_back() {
    let phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
    let (result, phone) = run(phone, true, b"frpd bytes").await;
    assert_eq!(result.unwrap()[..], Sha256::digest(b"frpd bytes")[..]);
    assert_eq!(
        phone.tree.get(PREFS),
        Some(&Node::File(b"frpd bytes".to_vec()))
    );
    assert!(!phone.tree.contains_key(&format!("{PREFS}.tmp")));
    let renamed = phone
        .log
        .iter()
        .position(|op| *op == AfcOpcode::RenamePath)
        .unwrap();
    let written = phone
        .log
        .iter()
        .position(|op| *op == AfcOpcode::Write)
        .unwrap();
    let read = phone
        .log
        .iter()
        .position(|op| *op == AfcOpcode::Read)
        .unwrap();
    assert!(written < renamed && renamed < read);
}

#[tokio::test]
async fn never_overwrites_an_existing_file_or_temporary() {
    for existing in [PREFS.to_owned(), format!("{PREFS}.tmp")] {
        let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
        phone
            .tree
            .insert(existing.clone(), Node::File(b"theirs".to_vec()));
        let (result, phone) = run(phone, true, b"ours").await;
        assert!(result.is_err(), "{existing}");
        assert_eq!(
            phone.tree.get(&existing),
            Some(&Node::File(b"theirs".to_vec()))
        );
        assert!(!phone.log.contains(&AfcOpcode::Write));
        assert!(!phone.log.contains(&AfcOpcode::RenamePath));
    }
}

#[tokio::test]
async fn a_locked_connection_sends_nothing() {
    let phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
    let (result, phone) = run(phone, false, b"ours").await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no Started permit"),
    );
    assert!(phone.log.is_empty());
}
