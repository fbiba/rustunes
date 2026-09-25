use super::*;

// Included inside the transport's offline duplex test module.

#[tokio::test]
async fn a_locked_connection_refuses_every_mutating_request_below_the_public_methods() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
    for mode in [
        AfcFopenMode::Rw,
        AfcFopenMode::WrOnly,
        AfcFopenMode::Wr,
        AfcFopenMode::Append,
        AfcFopenMode::RdAppend,
    ] {
        let error = transfer.open("Airlock/Media/1", mode).await.unwrap_err();
        assert!(
            error.to_string().contains("no Started permit"),
            "{mode:?}: {error:#}"
        );
    }
    for operation in [
        AfcOpcode::Write,
        AfcOpcode::WriteFile,
        AfcOpcode::WritePart,
        AfcOpcode::FileSetSize,
        AfcOpcode::Truncate,
        AfcOpcode::RenamePath,
        AfcOpcode::RemovePath,
        AfcOpcode::RemovePathAndContents,
        AfcOpcode::MakeDir,
        AfcOpcode::MakeLink,
        AfcOpcode::SetFileTime,
        AfcOpcode::WriteFileAtom,
    ] {
        let error = transfer
            .op(operation, vec![0; 16], Vec::new())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(
            error.to_string().contains("no Started permit"),
            "{operation:?}: {error:#}"
        );
    }
    // Refusing counted nothing and discarded nothing.
    assert_eq!(transfer.sequence, 0);
    assert!(transfer.usable);
    let server = tokio::spawn(async move {
        // Stat, a read-only open and its close all pass the lock.
        attributes(&mut peer, 3).await;
        let open = request(&mut peer, AfcOpcode::FileOpen).await;
        assert_eq!(&open.header_payload[..8], &1_u64.to_le_bytes());
        reply(
            &mut peer,
            &open,
            AfcOpcode::FileOpenRes,
            42_u64.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
        close(&mut peer).await;
        // Only once unlocked does a MakeDir reach the phone.
        let make = request(&mut peer, AfcOpcode::MakeDir).await;
        assert_eq!(make.header_payload, b"Airlock");
        status(&mut peer, &make, 0).await;
        assert!(peer.read_raw(1).await.is_err());
    });
    transfer.stat("Airlock/Media/1").await.unwrap();
    let handle = transfer
        .open("Airlock/Media/1", AfcFopenMode::RdOnly)
        .await
        .unwrap();
    transfer
        .status(
            AfcOpcode::FileClose,
            handle.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await
        .unwrap();
    transfer.unlock_writes();
    transfer
        .status(AfcOpcode::MakeDir, b"Airlock".to_vec(), Vec::new())
        .await
        .unwrap();
    drop(transfer);
    server.await.unwrap();
}

/// One full read of a 42-handle file holding `bytes`, as `read_hash` asks it.
async fn served_read(peer: &mut Idevice, bytes: &[u8]) {
    attributes(peer, bytes.len() as u64).await;
    let open = request(peer, AfcOpcode::FileOpen).await;
    assert_eq!(&open.header_payload[..8], &1_u64.to_le_bytes());
    reply(
        peer,
        &open,
        AfcOpcode::FileOpenRes,
        42_u64.to_le_bytes().to_vec(),
        Vec::new(),
    )
    .await;
    let read = request(peer, AfcOpcode::Read).await;
    reply(peer, &read, AfcOpcode::Data, Vec::new(), bytes.to_vec()).await;
    close(peer).await;
    attributes(peer, bytes.len() as u64).await;
}

#[tokio::test]
async fn a_locked_connection_removes_only_a_staged_pair_holding_exactly_the_operation() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
    let operation = b"<plist>operation</plist>".to_vec();
    let signature = [9_u8; 21];
    let served = operation.clone();
    let server = tokio::spawn(async move {
        for (path, bytes) in [(SIGNATURE, &signature[..]), (METADATA, &served[..])] {
            attributes(&mut peer, bytes.len() as u64).await;
            served_read(&mut peer, bytes).await;
            let remove = request(&mut peer, AfcOpcode::RemovePath).await;
            assert_eq!(remove.header_payload, path.as_bytes());
            assert!(remove.payload.is_empty());
            status(&mut peer, &remove, 0).await;
            absent(&mut peer).await;
        }
        assert!(peer.read_raw(1).await.is_err());
    });
    assert_eq!(
        transfer
            .remove_staged_operation(&operation, &signature)
            .await
            .unwrap(),
        [SIGNATURE, METADATA]
    );
    // The allowance went with the removal it was checked for.
    let error = transfer
        .op(
            AfcOpcode::RemovePath,
            METADATA.as_bytes().to_vec(),
            Vec::new(),
        )
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(error.to_string().contains("no Started permit"), "{error:#}");
    drop(transfer);
    server.await.unwrap();
}

#[tokio::test]
async fn staged_files_that_are_not_the_operation_are_never_removed() {
    let signature = [9_u8; 21];
    for (size, bytes, message) in [
        (20, None, "is not this operation"),
        (21, Some([8_u8; 21]), "differs from this operation"),
    ] {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        let server = tokio::spawn(async move {
            attributes(&mut peer, size).await;
            if let Some(bytes) = bytes {
                served_read(&mut peer, &bytes).await;
            }
            // No RemovePath follows.
            assert!(peer.read_raw(1).await.is_err());
        });
        let error = transfer
            .remove_staged_operation(b"operation", &signature)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
        drop(transfer);
        server.await.unwrap();
    }
}
