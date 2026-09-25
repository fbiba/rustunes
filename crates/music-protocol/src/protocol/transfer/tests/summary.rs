use super::*;

// Included inside the transport's offline duplex test module.

fn summary_pair(trace: &Path) -> (Transfer, Idevice) {
    let (mut transfer, peer) = pair(trace, Duration::from_secs(5));
    transfer.readonly_summary = true;
    // Mutations below are refused by the summary transport, not the write lock.
    transfer.unlock_writes();
    (transfer, peer)
}

async fn open_read(peer: &mut Idevice, size: u64) {
    attributes(peer, size).await;
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
}

fn trace_files(trace: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(trace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

async fn assert_no_request(peer: &mut Idevice) {
    assert!(
        tokio::time::timeout(Duration::from_millis(20), AfcPacket::read(peer))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn summary_many_chunks_save_one_complete_hash_and_attributes_receipt() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    let source = vec![0xa5; 24 * CHUNK + 3];
    let size = source.len() as u64;
    let expected: [u8; 32] = Sha256::digest(&source).into();
    let server = tokio::spawn(async move {
        open_read(&mut peer, size).await;
        for chunk in source.chunks(CHUNK) {
            let read = request(&mut peer, AfcOpcode::Read).await;
            reply(
                &mut peer,
                &read,
                AfcOpcode::Data,
                Vec::new(),
                chunk.to_vec(),
            )
            .await;
        }
        close(&mut peer).await;
        attributes(&mut peer, size).await;
    });
    assert_eq!(
        transfer
            .read_hash("iTunes_Control/Music/F16/TEST.m4a", size, MAX_AUDIO)
            .await
            .unwrap(),
        expected
    );
    server.await.unwrap();
    let files = trace_files(&trace.0);
    assert_eq!(files, vec![trace.0.join("afc-readback-0000.plist")]);
    let record = Value::from_file(&files[0]).unwrap();
    let record = record.as_dictionary().unwrap();
    assert_eq!(record["Bytes"].as_unsigned_integer(), Some(size));
    assert_eq!(record["SHA256"].as_string(), Some(hex(&expected).as_str()));
    assert_eq!(record["Operations"].as_unsigned_integer(), Some(29));
    assert_eq!(record["FirstOperation"].as_unsigned_integer(), Some(0));
    assert_eq!(record["BeforeAttributes"], record["AfterAttributes"]);
    assert_eq!(
        record["BeforeAttributes"].as_dictionary().unwrap()["st_size"].as_string(),
        Some(size.to_string().as_str())
    );
    assert_eq!(transfer.recent_frames.len(), SUMMARY_FRAMES);
    assert!(std::fs::metadata(&files[0]).unwrap().len() < 4096);
}

#[tokio::test]
async fn summary_empty_file_is_a_valid_hashed_four_operation_readback() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    let server = tokio::spawn(async move {
        open_read(&mut peer, 0).await;
        close(&mut peer).await;
        attributes(&mut peer, 0).await;
    });
    assert_eq!(
        transfer
            .read_hash("iTunes_Control/iTunes/empty-wal", 0, MAX_CATALOG)
            .await
            .unwrap(),
        <[u8; 32]>::from(Sha256::digest([]))
    );
    server.await.unwrap();
    assert_eq!(transfer.sequence, 4);
    assert_eq!(trace_files(&trace.0).len(), 1);
}

#[tokio::test]
async fn summary_refuses_mutations_before_any_io_and_cannot_be_reused() {
    for operation in 0..6 {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        let result = match operation {
            0 => transfer
                .write_new("Airlock/Media/1", &mut Cursor::new([1]), 1)
                .await
                .map(|_| ()),
            1 => transfer
                .write_audio_new("Airlock/Media/1", &mut Cursor::new([1]), 1)
                .await
                .map(|_| ()),
            2 => transfer
                .rename_absent("Airlock/Media/1.m4a", "Airlock/Media/1", 1)
                .await
                .map(|_| ()),
            3 => transfer
                .open("Airlock/Media/1", AfcFopenMode::Rw)
                .await
                .map(|_| ()),
            4 => transfer
                .op(AfcOpcode::Write, 42_u64.to_le_bytes().to_vec(), vec![1])
                .await
                .map(|_| ()),
            _ => transfer
                .op(AfcOpcode::FileSetSize, vec![0; 16], Vec::new())
                .await
                .map(|_| ()),
        };
        assert!(result.is_err());
        assert_eq!(transfer.sequence, 0);
        assert!(!transfer.usable);
        assert!(
            transfer
                .stat("iTunes_Control/iTunes/iTunesCDB")
                .await
                .is_err()
        );
        assert_no_request(&mut peer).await;
        assert_eq!(
            trace_files(&trace.0),
            vec![trace.0.join("afc-summary-failure.plist")]
        );
    }
}

#[tokio::test]
async fn summary_malformed_read_discards_without_close_and_saves_bounded_headers_only() {
    for malformed in 0..4 {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        let server = tokio::spawn(async move {
            open_read(&mut peer, CHUNK as u64).await;
            let read = request(&mut peer, AfcOpcode::Read).await;
            let (opcode, header, payload) = match malformed {
                0 => (AfcOpcode::Data, Vec::new(), Vec::new()),
                1 => (AfcOpcode::Data, Vec::new(), vec![0xa5; CHUNK + 1]),
                2 => (AfcOpcode::Data, vec![0; 1024], vec![0xa5; 1]),
                _ => (
                    AfcOpcode::FileOpenRes,
                    42_u64.to_le_bytes().to_vec(),
                    Vec::new(),
                ),
            };
            reply(&mut peer, &read, opcode, header, payload).await;
            assert_no_request(&mut peer).await;
        });
        assert!(
            transfer
                .read_hash("iTunes_Control/Music/F16/TEST.m4a", CHUNK as u64, MAX_AUDIO)
                .await
                .is_err()
        );
        assert!(!transfer.usable);
        assert!(
            transfer
                .stat("iTunes_Control/iTunes/iTunesCDB")
                .await
                .is_err()
        );
        server.await.unwrap();
        let files = trace_files(&trace.0);
        assert_eq!(files, vec![trace.0.join("afc-summary-failure.plist")]);
        let record = Value::from_file(&files[0]).unwrap();
        let frames = record.as_dictionary().unwrap()["RecentFrames"]
            .as_array()
            .unwrap();
        assert_eq!(frames.len(), 3);
        for frame in frames {
            let frame = frame.as_dictionary().unwrap();
            assert!(!frame.contains_key("Payload"));
            if let Some(header) = frame.get("ReplyHeaderPrefix") {
                assert!(header.as_data().unwrap().len() <= SUMMARY_HEADER);
            }
        }
        assert!(std::fs::metadata(&files[0]).unwrap().len() < 16 * 1024);
    }
}

#[tokio::test]
async fn summary_partial_reads_exhaust_original_budget_without_close_or_retry() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    transfer.operation_limit = 14;
    let server = tokio::spawn(async move {
        open_read(&mut peer, 100).await;
        for _ in 0..12 {
            let read = request(&mut peer, AfcOpcode::Read).await;
            reply(&mut peer, &read, AfcOpcode::Data, Vec::new(), vec![0xa5]).await;
        }
        assert_no_request(&mut peer).await;
    });
    assert!(
        transfer
            .read_hash("iTunes_Control/Music/F16/TEST.m4a", 100, MAX_AUDIO)
            .await
            .is_err()
    );
    server.await.unwrap();
    assert!(!transfer.usable);
    assert_eq!(transfer.sequence, 14);
    let record = Value::from_file(trace.0.join("afc-summary-failure.plist")).unwrap();
    assert_eq!(
        record.as_dictionary().unwrap()["RecentFrames"]
            .as_array()
            .unwrap()
            .len(),
        SUMMARY_FRAMES
    );
    let readback = record.as_dictionary().unwrap()["Readback"]
        .as_dictionary()
        .unwrap();
    assert_eq!(
        readback["Path"].as_string(),
        Some("iTunes_Control/Music/F16/TEST.m4a")
    );
    assert_eq!(readback["ExpectedBytes"].as_unsigned_integer(), Some(100));
    assert_eq!(readback["BytesRead"].as_unsigned_integer(), Some(12));
    assert_eq!(trace_files(&trace.0).len(), 1);
}

#[tokio::test]
async fn summary_cancellation_deadline_and_budget_refuse_before_io_and_log_once() {
    for failure in 0..3 {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        match failure {
            0 => transfer.cancelled = Some(Arc::new(AtomicBool::new(true))),
            1 => transfer.deadline = Instant::now(),
            _ => transfer.operation_limit = 0,
        }
        assert!(
            transfer
                .stat("iTunes_Control/iTunes/iTunesCDB")
                .await
                .is_err()
        );
        assert!(!transfer.usable);
        assert!(
            transfer
                .stat("iTunes_Control/iTunes/iTunesCDB")
                .await
                .is_err()
        );
        assert_eq!(transfer.sequence, 0);
        assert_no_request(&mut peer).await;
        assert_eq!(trace_files(&trace.0).len(), 1);
    }
}

#[tokio::test]
async fn summary_complete_read_error_discards_without_close() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    let server = tokio::spawn(async move {
        open_read(&mut peer, 1).await;
        let read = request(&mut peer, AfcOpcode::Read).await;
        status(&mut peer, &read, 1).await;
        assert_no_request(&mut peer).await;
    });
    assert!(
        transfer
            .read_hash("iTunes_Control/Music/F16/TEST.m4a", 1, MAX_AUDIO)
            .await
            .is_err()
    );
    assert!(!transfer.usable);
    server.await.unwrap();
    assert_eq!(trace_files(&trace.0).len(), 1);
}

#[tokio::test]
async fn summary_malformed_attributes_discard_but_notfound_is_a_valid_stat() {
    for malformed in [false, true] {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        let server = tokio::spawn(async move {
            let stat = request(&mut peer, AfcOpcode::GetFileInfo).await;
            if malformed {
                reply(
                    &mut peer,
                    &stat,
                    AfcOpcode::Data,
                    Vec::new(),
                    b"st_size\0".to_vec(),
                )
                .await;
            } else {
                status(&mut peer, &stat, 8).await;
            }
        });
        let result = transfer.stat("iTunes_Control/iTunes/missing").await;
        if malformed {
            assert!(result.is_err());
            assert!(!transfer.usable);
            assert_eq!(trace_files(&trace.0).len(), 1);
        } else {
            assert!(result.unwrap().is_none());
            assert!(transfer.usable);
            assert!(trace_files(&trace.0).is_empty());
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn summary_checks_every_attribute_except_access_time() {
    for access_time_only in [false, true] {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        let server = tokio::spawn(async move {
            open_read(&mut peer, 0).await;
            close(&mut peer).await;
            let stat = request(&mut peer, AfcOpcode::GetFileInfo).await;
            let key = if access_time_only {
                "st_atime"
            } else {
                "extra_flag"
            };
            let payload = format!("st_ifmt\0S_IFREG\0st_size\x000\0st_mtime\x00123\0{key}\x001\0");
            reply(
                &mut peer,
                &stat,
                AfcOpcode::Data,
                Vec::new(),
                payload.into_bytes(),
            )
            .await;
        });
        let result = transfer
            .read_hash("iTunes_Control/iTunes/empty-wal", 0, MAX_CATALOG)
            .await;
        server.await.unwrap();
        assert_eq!(result.is_ok(), access_time_only);
        assert_eq!(transfer.usable, access_time_only);
        assert_eq!(trace_files(&trace.0).len(), 1);
        assert_eq!(
            trace.0.join("afc-readback-0000.plist").exists(),
            access_time_only
        );
        if access_time_only {
            let receipt = Value::from_file(trace.0.join("afc-readback-0000.plist")).unwrap();
            let receipt = receipt.as_dictionary().unwrap();
            assert!(
                !receipt["BeforeAttributes"]
                    .as_dictionary()
                    .unwrap()
                    .contains_key("st_atime")
            );
            assert!(
                receipt["AfterAttributes"]
                    .as_dictionary()
                    .unwrap()
                    .contains_key("st_atime")
            );
        }
    }
}

#[tokio::test]
async fn summary_cancellation_during_read_and_inflight_deadline_discard_without_close() {
    for cancel in [false, true] {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = summary_pair(&trace.0);
        let cancelled = Arc::new(AtomicBool::new(false));
        transfer.cancelled = Some(cancelled.clone());
        if !cancel {
            transfer.deadline = Instant::now() + Duration::from_millis(100);
        }
        let server = tokio::spawn(async move {
            open_read(&mut peer, 2).await;
            let read = request(&mut peer, AfcOpcode::Read).await;
            if cancel {
                cancelled.store(true, Ordering::Relaxed);
                reply(&mut peer, &read, AfcOpcode::Data, Vec::new(), vec![1]).await;
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            assert_no_request(&mut peer).await;
        });
        assert!(
            transfer
                .read_hash("iTunes_Control/Music/F16/TEST.m4a", 2, MAX_AUDIO)
                .await
                .is_err()
        );
        assert!(!transfer.usable);
        assert_eq!(transfer.sequence, 3);
        assert!(
            transfer
                .stat("iTunes_Control/iTunes/iTunesCDB")
                .await
                .is_err()
        );
        server.await.unwrap();
        assert_eq!(
            trace_files(&trace.0),
            vec![trace.0.join("afc-summary-failure.plist")]
        );
    }
}

#[tokio::test]
async fn summary_read_exact_uses_the_same_receipt_without_logging_output_bytes() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    let server = tokio::spawn(async move {
        open_read(&mut peer, 1).await;
        let read = request(&mut peer, AfcOpcode::Read).await;
        reply(&mut peer, &read, AfcOpcode::Data, Vec::new(), vec![0xa5]).await;
        close(&mut peer).await;
        attributes(&mut peer, 1).await;
    });
    let mut output = Vec::new();
    assert_eq!(
        transfer
            .read_exact("iTunes_Control/iTunes/test", 1, MAX_CATALOG, &mut output)
            .await
            .unwrap(),
        <[u8; 32]>::from(Sha256::digest([0xa5]))
    );
    assert_eq!(output, [0xa5]);
    server.await.unwrap();
    assert_eq!(
        trace_files(&trace.0),
        vec![trace.0.join("afc-readback-0000.plist")]
    );
}

#[tokio::test]
async fn current_profile_malformed_chunk_has_one_capped_diagnostic_and_no_cleanup() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    transfer.current_summary = true;
    transfer.cancelled = Some(Arc::new(AtomicBool::new(false)));
    let server = tokio::spawn(async move {
        open_read(&mut peer, 1).await;
        let read = request(&mut peer, AfcOpcode::Read).await;
        reply(
            &mut peer,
            &read,
            AfcOpcode::Data,
            Vec::new(),
            vec![0xab; CHUNK],
        )
        .await;
        assert_no_request(&mut peer).await;
    });
    let mut sink = Vec::new();
    assert!(
        transfer
            .read_exact("iTunes_Control/Music/F16/TEST.m4a", 1, MAX_AUDIO, &mut sink)
            .await
            .is_err()
    );
    server.await.unwrap();
    assert!(sink.is_empty());
    let files = trace_files(&trace.0);
    assert_eq!(files, vec![trace.0.join("afc-summary-failure.plist")]);
    assert!(std::fs::metadata(&files[0]).unwrap().len() <= CURRENT_METADATA_LIMIT as u64);
}

#[test]
fn current_profile_oversized_complete_attributes_refuse_before_output_write() {
    let trace = Temporary::new();
    let (mut transfer, _) = summary_pair(&trace.0);
    transfer.current_summary = true;
    let mut record = Dictionary::new();
    record.insert(
        "BeforeAttributes".into(),
        "<&".repeat(CURRENT_METADATA_LIMIT).into(),
    );
    assert!(
        transfer
            .summary_write("afc-readback-0000.plist", record)
            .is_err()
    );
    assert!(trace_files(&trace.0).is_empty());
}

#[test]
fn current_error_formatter_stops_before_rendering_the_full_error() {
    #[derive(Debug)]
    struct Huge(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl std::fmt::Display for Huge {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..100_000 {
                self.0.fetch_add(1, Ordering::Relaxed);
                f.write_str("x")?;
            }
            Ok(())
        }
    }
    impl std::error::Error for Huge {}
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let text = bounded_error(&anyhow::Error::new(Huge(calls.clone())));
    assert_eq!(text.len(), 2048);
    assert_eq!(calls.load(Ordering::Relaxed), 2049);
}

#[tokio::test]
async fn a_summary_connection_pipelines_stats_counting_each_as_an_operation() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    let paths = ["Airlock/Media/a", "Airlock/Media/absent", "Airlock/Media/c"];
    let server = tokio::spawn(async move {
        // Every request arrives before any reply is sent.
        let mut requests = Vec::new();
        for _ in 0..3 {
            requests.push(request(&mut peer, AfcOpcode::GetFileInfo).await);
        }
        for request in &requests {
            if request.header_payload.ends_with(b"absent") {
                status(&mut peer, request, 8).await;
            } else {
                let bytes = b"st_ifmt\0S_IFREG\0st_size\x009\0st_mtime\0123\0".to_vec();
                reply(&mut peer, request, AfcOpcode::Data, Vec::new(), bytes).await;
            }
        }
        peer
    });
    let stats = transfer.stat_all(&paths).await.unwrap();
    let _peer = server.await.unwrap();
    assert_eq!(
        stats
            .iter()
            .map(|s| s.as_ref().map(|a| a["st_size"].clone()))
            .collect::<Vec<_>>(),
        [Some("9".to_owned()), None, Some("9".to_owned())]
    );
    assert_eq!(transfer.sequence, 3);
    assert!(transfer.usable);
    assert_eq!(transfer.recent_frames.len(), 3);
}

#[tokio::test]
async fn pipelined_stats_beyond_the_operation_budget_send_nothing() {
    let trace = Temporary::new();
    let (mut transfer, mut peer) = summary_pair(&trace.0);
    transfer.operation_limit = 2;
    let server = tokio::spawn(async move {
        assert!(peer.read_raw(1).await.is_err());
    });
    assert!(
        transfer
            .stat_all(&["Airlock/Media/a", "Airlock/Media/b", "Airlock/Media/c"])
            .await
            .is_err()
    );
    assert!(!transfer.usable);
    drop(transfer);
    server.await.unwrap();
}
