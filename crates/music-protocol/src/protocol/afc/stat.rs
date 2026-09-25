//! Decode an exact retained AFC GetFileInfo exchange into factual observations.
//! Only canonical ObjectNotFound status 8 establishes absence. Attributes do not
//! grant deletion authority or establish preservation of other assets.
use super::copy::{bounded, bounded_controlled};
use anyhow::{Context, Result, ensure};
use idevice::{
    IdeviceError,
    afc::{AfcClient, errors::AfcError, opcode::AfcOpcode, packet::AfcPacket},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::time::Instant;

/// One file's GetFileInfo attributes, keys and values as the phone sent them.
pub type Attributes = BTreeMap<String, String>;

#[derive(PartialEq, Eq, Debug)]
pub enum AssetStat {
    Absent,
    Present(BTreeMap<String, String>),
}
pub fn decode_stat(request: &[u8], reply: &[u8], path: &str) -> Result<AssetStat> {
    use idevice::afc::MAGIC;
    let word = |n: u64| n.to_le_bytes();
    let mut expected = Vec::new();
    for n in [
        MAGIC,
        40 + path.len() as u64,
        40 + path.len() as u64,
        0,
        AfcOpcode::GetFileInfo as u64,
    ] {
        expected.extend(word(n));
    }
    expected.extend(path.as_bytes());
    ensure!(
        request == expected,
        "GetFileInfo request is not exact for the target and packet number 0"
    );
    ensure!(
        (40..=40 + 16384).contains(&reply.len()),
        "AFC stat response bound"
    );
    let n = |at| u64::from_le_bytes(reply[at..at + 8].try_into().unwrap());
    ensure!(
        n(0) == MAGIC && n(8) == reply.len() as u64 && n(24) == 0,
        "AFC stat framing mismatch"
    );
    if n(32) == AfcOpcode::Status as u64 {
        ensure!(
            reply.len() == 48 && n(16) == 48 && n(40) == 8,
            "AFC status is not canonical ObjectNotFound (status 8)"
        );
        return Ok(AssetStat::Absent);
    }
    ensure!(
        n(32) == AfcOpcode::Data as u64
            && n(16) == 40
            && reply.len() > 40
            && reply.last() == Some(&0),
        "AFC stat attributes shape differs"
    );
    let values = reply[40..reply.len() - 1]
        .split(|b| *b == 0)
        .map(std::str::from_utf8)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        !values.is_empty() && values.len().is_multiple_of(2) && values.len() <= 256,
        "AFC attribute count"
    );
    let mut attrs = BTreeMap::new();
    for pair in values.chunks_exact(2) {
        ensure!(
            !pair[0].is_empty()
                && pair[0].len() <= 256
                && pair[1].len() <= 4096
                && !pair.iter().any(|s| s.chars().any(char::is_control))
                && attrs.insert(pair[0].into(), pair[1].into()).is_none(),
            "invalid/duplicate AFC attribute"
        );
    }
    Ok(AssetStat::Present(attrs))
}

/// One lock-step GetFileInfo, bounded like every copy operation (and
/// controlled when `cancelled` is given). Only a typed ObjectNotFound is None;
/// any other status or transport error is returned and the connection must
/// then be discarded.
pub async fn attributes(
    afc: &mut AfcClient,
    path: &str,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
    name: &str,
) -> Result<Option<Attributes>> {
    let operation = async {
        match afc
            .send_op(AfcOpcode::GetFileInfo, path.as_bytes().to_vec(), Vec::new())
            .await
        {
            Ok(packet) => Ok(Some(packet)),
            Err(IdeviceError::Afc(AfcError::ObjectNotFound)) => Ok(None),
            Err(error) => Err(error),
        }
    };
    let reply = match cancelled {
        Some(flag) => bounded_controlled(deadline, name, operation, flag).await,
        None => bounded(deadline, name, operation).await,
    }
    .map_err(|failure| failure.error)?;
    reply_attributes(reply, name)
}

/// The attributes in one GetFileInfo reply as `pipelined` returns it (None
/// for ObjectNotFound). A reply that is not plain attribute data, or whose
/// attributes are malformed, is an error naming `name`.
pub fn reply_attributes(reply: Option<AfcPacket>, name: &str) -> Result<Option<Attributes>> {
    let Some(packet) = reply else {
        return Ok(None);
    };
    ensure!(
        packet.header.operation == AfcOpcode::Data && packet.header_payload.is_empty(),
        "{name}: unexpected attributes reply; connection discarded"
    );
    parse_attributes(&packet.payload)
        .with_context(|| name.to_owned())
        .map(Some)
}

/// Key and value pairs, each ended by a zero byte, within the 16 KiB reply
/// bound `decode_stat` applies. An empty or repeated key is refused.
pub fn parse_attributes(bytes: &[u8]) -> Result<Attributes> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= 16 * 1024 && bytes.ends_with(&[0]),
        "attributes exceed supported shape"
    );
    let text =
        std::str::from_utf8(&bytes[..bytes.len() - 1]).context("attributes are not UTF-8")?;
    let fields: Vec<_> = text.split('\0').take(129).collect();
    ensure!(
        fields.len() <= 128 && fields.len().is_multiple_of(2),
        "invalid attribute field count"
    );
    let mut attrs = Attributes::new();
    for pair in fields.chunks_exact(2) {
        ensure!(
            !pair[0].is_empty()
                && attrs
                    .insert(pair[0].to_owned(), pair[1].to_owned())
                    .is_none(),
            "empty or duplicate attribute key"
        );
    }
    Ok(attrs)
}

/// The size of a regular file that has a modification time. The size must be
/// plain decimal digits.
pub fn regular_size(attrs: &Attributes) -> Result<u64> {
    ensure!(
        attrs.get("st_ifmt").map(String::as_str) == Some("S_IFREG")
            && attrs.contains_key("st_mtime"),
        "not a regular file with modification time"
    );
    let raw = attrs.get("st_size").context("file size absent")?;
    ensure!(
        !raw.is_empty() && raw.len() <= 20 && raw.bytes().all(|b| b.is_ascii_digit()),
        "invalid file size"
    );
    Ok(raw.parse()?)
}

/// `attrs` without its access time, which reading the file changes. This is
/// the projection stored as a file's attributes.
pub fn without_access_time(mut attrs: Attributes) -> Attributes {
    attrs.remove("st_atime");
    attrs
}

/// Whether two observations of one file differ at most in access time.
pub fn unchanged(before: &Attributes, after: &Attributes) -> bool {
    before
        .iter()
        .filter(|(key, _)| key.as_str() != "st_atime")
        .eq(after.iter().filter(|(key, _)| key.as_str() != "st_atime"))
}

/// Stat requests one window puts in flight. afcd answers a connection's
/// requests in order, so a window costs about one round trip, not one each.
pub const STAT_WINDOW: usize = 64;
/// Pipelined requests number their packets from here, apart from a client's
/// own count, which only its lock-step calls advance.
static PIPELINED_PACKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 48);

/// Send one GetFileInfo for each path (at most `STAT_WINDOW`), then read the
/// replies: each path's Data packet, or None for ObjectNotFound. Each reply
/// must carry its request's packet number. Any other status or transport
/// error is returned, and the connection must then be discarded.
pub async fn pipelined(
    afc: &mut idevice::afc::AfcClient,
    paths: &[&str],
) -> std::result::Result<Vec<Option<idevice::afc::packet::AfcPacket>>, idevice::IdeviceError> {
    use idevice::afc::{MAGIC, packet::AfcPacketHeader};
    use std::sync::atomic::Ordering;
    if paths.len() > STAT_WINDOW {
        return Err(IdeviceError::UnexpectedResponse(
            "pipelined stat window too large".into(),
        ));
    }
    let first = PIPELINED_PACKETS.fetch_add(paths.len() as u64, Ordering::Relaxed);
    for (number, path) in (first..).zip(paths) {
        let header_payload = path.as_bytes().to_vec();
        let length = header_payload.len() as u64 + AfcPacketHeader::LEN;
        afc.send(AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: length,
                header_payload_len: length,
                packet_num: number,
                operation: AfcOpcode::GetFileInfo,
            },
            header_payload,
            payload: Vec::new(),
        })
        .await?;
    }
    let mut replies = Vec::with_capacity(paths.len());
    for number in first..first + paths.len() as u64 {
        match afc.read().await {
            Ok(packet) if packet.header.packet_num != number => {
                return Err(IdeviceError::UnexpectedResponse(
                    "pipelined stat reply out of order".into(),
                ));
            }
            Ok(packet) => replies.push(Some(packet)),
            Err(IdeviceError::Afc(AfcError::ObjectNotFound)) => replies.push(None),
            Err(error) => return Err(error),
        }
    }
    Ok(replies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::afc::{
        dropped_by_phone,
        duplex::{pair, reply, status},
    };
    use idevice::{
        Idevice, IdeviceError,
        afc::{
            MAGIC,
            opcode::AfcOpcode,
            packet::{AfcPacket, AfcPacketHeader},
        },
    };
    use std::time::Duration;

    const PATH: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb";

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    fn mode(controlled: bool) -> Option<Arc<AtomicBool>> {
        controlled.then(|| Arc::new(AtomicBool::new(false)))
    }
    fn payload(pairs: &[(&str, &str)]) -> Vec<u8> {
        pairs
            .iter()
            .flat_map(|(key, value)| [key.as_bytes(), &[0], value.as_bytes(), &[0]].concat())
            .collect()
    }
    fn file(size: &str) -> Vec<(&str, &str)> {
        vec![
            ("st_ifmt", "S_IFREG"),
            ("st_size", size),
            ("st_mtime", "123"),
            ("st_atime", "456"),
        ]
    }
    fn attrs(pairs: &[(&str, &str)]) -> Attributes {
        parse_attributes(&payload(pairs)).unwrap()
    }
    fn packet(operation: AfcOpcode, header_payload: Vec<u8>, payload: Vec<u8>) -> AfcPacket {
        let header_payload_len = AfcPacketHeader::LEN + header_payload.len() as u64;
        AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_payload_len + payload.len() as u64,
                header_payload_len,
                packet_num: 0,
                operation,
            },
            header_payload,
            payload,
        }
    }

    #[test]
    fn malformed_attribute_payloads_are_rejected_without_lossy_normalization() {
        for bytes in [
            b"".as_slice(),
            b"\0",
            b"key\0value",
            b"key\0",
            b"key\0one\0key\0two\0",
            b"\0value\0",
            b"key\0\xff\0",
        ] {
            assert!(parse_attributes(bytes).is_err(), "{bytes:?}");
        }
        assert_eq!(parse_attributes(b"key\0value\0").unwrap()["key"], "value");
        assert_eq!(parse_attributes(b"key\0\0").unwrap()["key"], "");
    }

    #[test]
    fn attribute_replies_are_bounded_to_16_kib_and_64_pairs() {
        // The reply bound decode_stat applies: 16384 bytes of attributes.
        let sized = |bytes: usize| [b"k\0".as_slice(), &vec![b'v'; bytes - 3], b"\0"].concat();
        assert_eq!(parse_attributes(&sized(16 * 1024)).unwrap().len(), 1);
        assert!(parse_attributes(&sized(16 * 1024 + 1)).is_err());
        assert!(parse_attributes(&vec![0; 16 * 1024 + 1]).is_err());
        let pairs = |count: usize| {
            (0..count)
                .flat_map(|n| format!("k{n}\0v\0").into_bytes())
                .collect::<Vec<_>>()
        };
        assert_eq!(parse_attributes(&pairs(64)).unwrap().len(), 64);
        assert!(parse_attributes(&pairs(65)).is_err());
    }

    #[test]
    fn regular_size_wants_a_regular_file_with_mtime_and_plain_decimal_digits() {
        assert_eq!(regular_size(&attrs(&file("10"))).unwrap(), 10);
        assert_eq!(regular_size(&attrs(&file("0"))).unwrap(), 0);
        assert_eq!(
            regular_size(&attrs(&file("18446744073709551615"))).unwrap(),
            u64::MAX
        );
        for size in [
            "",
            "+5",
            "-5",
            " 5",
            "5 ",
            "0x10",
            "1_0",
            "1.0",
            "18446744073709551616",
            "000000000000000000005",
        ] {
            assert!(regular_size(&attrs(&file(size))).is_err(), "{size:?}");
        }
        for missing in ["st_ifmt", "st_size", "st_mtime"] {
            let mut invalid = attrs(&file("10"));
            invalid.remove(missing);
            assert!(regular_size(&invalid).is_err(), "{missing}");
        }
        let mut link = attrs(&file("10"));
        link.insert("st_ifmt".into(), "S_IFLNK".into());
        assert!(regular_size(&link).is_err());
    }

    #[test]
    fn access_time_is_dropped_and_ignored_but_nothing_else() {
        let before = attrs(&file("10"));
        let without = without_access_time(before.clone());
        assert!(!without.contains_key("st_atime"));
        assert_eq!(without.len(), before.len() - 1);
        assert_eq!(without_access_time(without.clone()), without);

        let mut later = before.clone();
        later.insert("st_atime".into(), "999".into());
        assert!(unchanged(&before, &later));
        assert!(unchanged(&before, &without));
        assert!(unchanged(&without, &before));
        for (key, value) in [("st_mtime", "124"), ("st_size", "11"), ("st_blocks", "8")] {
            let mut changed = before.clone();
            changed.insert(key.into(), value.into());
            assert!(!unchanged(&before, &changed), "{key}");
            assert!(!unchanged(&changed, &before), "{key}");
        }
        let mut dropped = before.clone();
        dropped.remove("st_mtime");
        assert!(!unchanged(&before, &dropped));
    }

    #[test]
    fn a_reply_is_attribute_data_or_a_named_error() {
        assert_eq!(reply_attributes(None, "stat test").unwrap(), None);
        let data = packet(AfcOpcode::Data, Vec::new(), payload(&file("5")));
        assert_eq!(
            reply_attributes(Some(data), "stat test").unwrap(),
            Some(attrs(&file("5")))
        );
        let wrong = [
            packet(AfcOpcode::FileOpenRes, Vec::new(), payload(&file("5"))),
            packet(AfcOpcode::Data, vec![0; 8], payload(&file("5"))),
        ];
        for reply in wrong {
            let error = reply_attributes(Some(reply), "stat test").unwrap_err();
            assert_eq!(
                error.to_string(),
                "stat test: unexpected attributes reply; connection discarded"
            );
        }
        let malformed = packet(AfcOpcode::Data, Vec::new(), b"key\0value".to_vec());
        let error = reply_attributes(Some(malformed), "stat test").unwrap_err();
        assert!(format!("{error:#}").starts_with("stat test: "));
    }

    async fn stat_once(
        operation: AfcOpcode,
        header_payload: Vec<u8>,
        payload: Vec<u8>,
        controlled: bool,
    ) -> Result<Option<Attributes>> {
        let (mut afc, mut peer) = pair();
        let server = tokio::spawn(async move {
            let request = AfcPacket::read(&mut peer).await.unwrap();
            assert_eq!(request.header.operation, AfcOpcode::GetFileInfo);
            assert_eq!(request.header_payload, PATH.as_bytes());
            assert!(request.payload.is_empty());
            reply(&mut peer, &request, operation, header_payload, payload).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let result = attributes(&mut afc, PATH, soon(), mode(controlled), "stat test").await;
        drop(afc);
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn attributes_sends_one_stat_and_maps_only_object_not_found_to_none() {
        for controlled in [false, true] {
            let found = stat_once(AfcOpcode::Data, Vec::new(), payload(&file("5")), controlled)
                .await
                .unwrap();
            assert_eq!(found, Some(attrs(&file("5"))));

            let absent = stat_once(
                AfcOpcode::Status,
                8u64.to_le_bytes().to_vec(),
                Vec::new(),
                controlled,
            )
            .await
            .unwrap();
            assert_eq!(absent, None);

            // Any other status is an error, never absence.
            let denied = stat_once(
                AfcOpcode::Status,
                10u64.to_le_bytes().to_vec(),
                Vec::new(),
                controlled,
            )
            .await
            .unwrap_err();
            let shown = format!("{denied:#}");
            if controlled {
                assert_eq!(shown, "stat test: complete AFC status");
            } else {
                assert!(shown.starts_with("stat test: ") && shown.contains("PermDenied"));
            }

            let wrong = stat_once(AfcOpcode::FileOpenRes, Vec::new(), Vec::new(), controlled)
                .await
                .unwrap_err();
            assert_eq!(
                wrong.to_string(),
                "stat test: unexpected attributes reply; connection discarded"
            );
            let malformed = stat_once(AfcOpcode::Data, Vec::new(), b"key".to_vec(), controlled)
                .await
                .unwrap_err();
            assert!(format!("{malformed:#}").starts_with("stat test: "));
        }
    }

    #[tokio::test]
    async fn attributes_keeps_a_dropped_connection_recognisable_and_stops_at_the_deadline() {
        for controlled in [false, true] {
            let (mut afc, peer) = pair();
            drop(peer);
            let error = attributes(&mut afc, PATH, soon(), mode(controlled), "stat test")
                .await
                .unwrap_err();
            assert!(dropped_by_phone(&error), "{error:#}");
        }
        let (mut afc, mut peer) = pair();
        let error = attributes(&mut afc, PATH, Instant::now(), None, "stat test")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "stat test: session deadline expired");
        let flag = Arc::new(AtomicBool::new(true));
        attributes(&mut afc, PATH, soon(), Some(flag), "stat test")
            .await
            .unwrap_err();
        drop(afc);
        assert!(peer.read_raw(1).await.is_err());
    }

    /// Takes the whole window before answering any request. An absent file
    /// is every fifth path; the others report their index as their size. With
    /// `skew`, the second reply (a stat, not an absence) carries the wrong number.
    async fn answer_window(peer: &mut Idevice, count: usize, skew: bool) {
        let mut requests = Vec::new();
        for _ in 0..count {
            requests.push(AfcPacket::read(peer).await.unwrap());
        }
        let first = requests[0].header.packet_num;
        for (n, mut request) in requests.into_iter().enumerate() {
            assert_eq!(request.header.operation, AfcOpcode::GetFileInfo);
            assert_eq!(
                request.header_payload,
                format!("iTunes_Control/Music/F00/{n}.m4a").as_bytes()
            );
            assert_eq!(request.header.packet_num, first + n as u64);
            if skew && n == 1 {
                request.header.packet_num += 1;
            }
            if n % 5 == 0 {
                status(peer, &request, 8).await;
            } else {
                let size = n.to_string();
                let attributes = payload(&file(&size));
                reply(peer, &request, AfcOpcode::Data, Vec::new(), attributes).await;
            }
        }
    }

    #[tokio::test]
    async fn pipelined_stats_put_a_window_in_flight_and_answer_in_order() {
        let paths: Vec<String> = (0..STAT_WINDOW)
            .map(|n| format!("iTunes_Control/Music/F00/{n}.m4a"))
            .collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (mut afc, mut peer) = pair();
        let server =
            tokio::spawn(async move { answer_window(&mut peer, STAT_WINDOW, false).await });
        let replies = pipelined(&mut afc, &paths).await.unwrap();
        server.await.unwrap();
        assert_eq!(replies.len(), STAT_WINDOW);
        for (n, found) in replies.into_iter().enumerate() {
            let stat = reply_attributes(found, "stat test").unwrap();
            match stat {
                None => assert_eq!(n % 5, 0),
                Some(attrs) => {
                    assert_ne!(n % 5, 0);
                    assert_eq!(regular_size(&attrs).unwrap(), n as u64);
                }
            }
        }
    }

    #[tokio::test]
    async fn pipelined_stats_discard_a_reply_with_another_requests_number() {
        let paths = [
            "iTunes_Control/Music/F00/0.m4a",
            "iTunes_Control/Music/F00/1.m4a",
        ];
        let (mut afc, mut peer) = pair();
        let server = tokio::spawn(async move { answer_window(&mut peer, 2, true).await });
        let error = pipelined(&mut afc, &paths).await.unwrap_err();
        assert!(matches!(&error, IdeviceError::UnexpectedResponse(_)));
        assert!(format!("{error}").contains("out of order"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pipelined_stats_refuse_a_window_over_the_bound_before_sending() {
        let paths = vec![PATH; STAT_WINDOW + 1];
        let (mut afc, mut peer) = pair();
        let error = pipelined(&mut afc, &paths).await.unwrap_err();
        assert!(format!("{error}").contains("window too large"), "{error}");
        drop(afc);
        assert!(peer.read_raw(1).await.is_err());
        // The bound itself is a whole window of 64 requests.
        assert_eq!(STAT_WINDOW, 64);
    }
    #[test]
    fn canonical_stat_only_accepts_exact_target_packet_status8() {
        use idevice::afc::{MAGIC, opcode::AfcOpcode};
        let path = "iTunes_Control/Music/F03/ZRLD.m4a";
        let mut req = Vec::new();
        for n in [
            MAGIC,
            40 + path.len() as u64,
            40 + path.len() as u64,
            0,
            AfcOpcode::GetFileInfo as u64,
        ] {
            req.extend(n.to_le_bytes());
        }
        req.extend(path.as_bytes());
        let mut reply = Vec::new();
        for n in [MAGIC, 48, 48, 0, AfcOpcode::Status as u64, 8] {
            reply.extend(n.to_le_bytes());
        }
        assert_eq!(decode_stat(&req, &reply, path).unwrap(), AssetStat::Absent);
        for (offset, value) in [
            (0, 0),
            (8, 49),
            (16, 40),
            (24, 1),
            (32, AfcOpcode::Data as u64),
            (40, 1),
        ] {
            let mut bad = reply.clone();
            bad[offset..offset + 8].copy_from_slice(&u64::to_le_bytes(value));
            assert!(decode_stat(&req, &bad, path).is_err());
        }
        assert!(decode_stat(&req, &reply, "iTunes_Control/Music/F03/OTHER.m4a").is_err());
        reply.push(0);
        assert!(decode_stat(&req, &reply, path).is_err());
    }
    #[test]
    fn canonical_present_attributes_are_distinct_from_absence_and_reject_duplicates() {
        use idevice::afc::{MAGIC, opcode::AfcOpcode};
        let path = "iTunes_Control/Music/F03/ZRLD.m4a";
        let mut request = Vec::new();
        for n in [
            MAGIC,
            40 + path.len() as u64,
            40 + path.len() as u64,
            0,
            AfcOpcode::GetFileInfo as u64,
        ] {
            request.extend(n.to_le_bytes());
        }
        request.extend(path.as_bytes());
        let make = |payload: &[u8]| {
            let mut frame = Vec::new();
            for n in [
                MAGIC,
                40 + payload.len() as u64,
                40,
                0,
                AfcOpcode::Data as u64,
            ] {
                frame.extend(n.to_le_bytes());
            }
            frame.extend(payload);
            frame
        };
        let valid = make(b"st_ifmt\0S_IFREG\0st_size\059890\0st_mtime\0100\0");
        let AssetStat::Present(attributes) = decode_stat(&request, &valid, path).unwrap() else {
            panic!("not present")
        };
        assert_eq!(attributes.get("st_size").unwrap(), "59890");
        for invalid in [
            b"st_size\059890\0st_size\059890\0".as_slice(),
            b"st_size\059890",
            b"\059890\0",
            b"st_size\059890\0dangling\0",
        ] {
            assert!(decode_stat(&request, &make(invalid), path).is_err());
        }
    }
}
