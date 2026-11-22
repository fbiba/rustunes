//! Removing what a write stopped part way left: only its own bytes, or the
//! start of them, ever go, and a locked connection can do it.
use super::*;
use crate::test_support::afc::{FakePhone, Node};

const PATH: &str = "iTunes_Control/iTunes/iTunesPrefs.tmp";
const BYTES: &[u8] = b"frpd and the rest of the image";

async fn run(found: Option<Node>) -> (Result<bool>, FakePhone) {
    let trace = Temporary::new();
    // Locked: no Started permit unlocked it.
    let (mut transfer, peer) = pair(&trace.0, Duration::from_secs(5));
    let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
    if let Some(node) = found {
        phone.tree.insert(PATH.to_owned(), node);
    }
    let server = tokio::spawn(phone.serve(peer));
    let result = transfer.remove_interrupted_write(PATH, BYTES).await;
    drop(transfer);
    (result, server.await.unwrap())
}

#[tokio::test]
async fn the_whole_file_or_any_start_of_it_is_removed() {
    for length in [0, 1, 10, BYTES.len()] {
        let (result, phone) = run(Some(Node::File(BYTES[..length].to_vec()))).await;
        assert!(result.unwrap(), "{length}");
        assert!(!phone.tree.contains_key(PATH), "{length}");
    }
}

#[tokio::test]
async fn an_absent_path_is_nothing_to_remove() {
    let (result, phone) = run(None).await;
    assert!(!result.unwrap());
    assert!(!phone.log.contains(&AfcOpcode::RemovePath));
}

#[tokio::test]
async fn other_bytes_a_longer_file_or_a_folder_are_never_removed() {
    let mut longer = BYTES.to_vec();
    longer.push(b'!');
    let mut other = BYTES[..10].to_vec();
    other[3] = b'X';
    for (found, message) in [
        (Node::File(other), "differs from"),
        (Node::File(longer), "is larger"),
        (Node::Directory, "is not what the interrupted write left"),
    ] {
        let kept = found.clone();
        let (result, phone) = run(Some(found)).await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(message), "{error}");
        assert!(error.contains("nothing removed"), "{error}");
        assert_eq!(phone.tree.get(PATH), Some(&kept));
        assert!(!phone.log.contains(&AfcOpcode::RemovePath));
    }
}

/// A staged operation pair whose write a killed host cut short, down to an
/// empty operation file and no signature, is removed as the whole pair is.
#[tokio::test]
async fn a_staged_pair_cut_short_is_removed() {
    let operation = b"<plist>operation</plist>";
    let signature = [9_u8; 21];
    for (written, signed) in [
        (0, None),
        (10, None),
        (operation.len(), Some(0)),
        (operation.len(), Some(7)),
    ] {
        let trace = Temporary::new();
        // Locked: no Started permit unlocked it.
        let (mut transfer, peer) = pair(&trace.0, Duration::from_secs(5));
        let mut phone = FakePhone::new(&[
            "iTunes_Control",
            "iTunes_Control/Sync",
            "iTunes_Control/Sync/Media",
        ]);
        phone.tree.insert(
            METADATA.to_owned(),
            Node::File(operation[..written].to_vec()),
        );
        if let Some(signed) = signed {
            phone.tree.insert(
                SIGNATURE.to_owned(),
                Node::File(signature[..signed].to_vec()),
            );
        }
        let server = tokio::spawn(phone.serve(peer));
        let removed = transfer
            .remove_staged_operation(operation, &signature)
            .await
            .unwrap();
        drop(transfer);
        let phone = server.await.unwrap();
        assert_eq!(
            removed.len(),
            1 + usize::from(signed.is_some()),
            "{written}"
        );
        assert!(!phone.tree.contains_key(METADATA), "{written}");
        assert!(!phone.tree.contains_key(SIGNATURE), "{written}");
    }
}
