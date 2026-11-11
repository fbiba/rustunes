//! The offline phone other crates test against: reads that take several
//! requests or come back short, stats a test sets, and files that change
//! between two requests.
use super::*;
use crate::test_support::afc::FakePhone;

const PATH: &str = "iTunes_Control/iTunes/MediaLibrary.sqlitedb";

fn bytes(length: usize) -> Vec<u8> {
    (0..length).map(|at| (at % 251) as u8).collect()
}

async fn read(phone: FakePhone, length: u64) -> (Result<[u8; 32]>, FakePhone) {
    let trace = Temporary::new();
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (mut transfer, server) = phone.connect(&trace.0, &control, MAX_OPS).unwrap();
    let result = transfer.read_hash(PATH, length, MAX_CATALOG).await;
    drop(transfer);
    (result, server.await.unwrap())
}

#[tokio::test]
async fn a_file_larger_than_a_request_or_answered_short_reads_whole() {
    let file = bytes(2 * CHUNK + 5);
    for (limit, reads) in [(usize::MAX, 3), (64 * 1024, 33)] {
        let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
        phone.file(PATH, &file);
        phone.read_limit = limit;
        let (result, phone) = read(phone, file.len() as u64).await;
        assert_eq!(result.unwrap()[..], Sha256::digest(&file)[..]);
        let count = |operation| phone.log.iter().filter(|op| **op == operation).count();
        assert_eq!(count(AfcOpcode::Read), reads, "{limit}");
        // Stat, open, the reads, close, stat.
        assert_eq!(phone.log.len(), reads + 4);
    }
}

#[tokio::test]
async fn a_change_between_two_requests_and_a_set_stat_are_seen() {
    let file = bytes(2 * CHUNK);
    let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
    phone.file(PATH, &file);
    phone.before_each(|request, phone| {
        // After the first read, the file is written again: a new mtime.
        if request.index == 3 && request.path.as_deref() == Some(PATH) {
            phone.set_attribute(PATH, "st_mtime", Some("3"));
        }
    });
    let (result, _) = read(phone, file.len() as u64).await;
    assert!(format!("{:#}", result.unwrap_err()).contains("readback attributes changed"));

    let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
    phone.file(PATH, &file);
    phone.set_attribute(PATH, "st_ifmt", Some("S_IFLNK"));
    let (result, phone) = read(phone, file.len() as u64).await;
    assert!(format!("{:#}", result.unwrap_err()).contains("not a regular file"));
    assert_eq!(phone.log, [AfcOpcode::GetFileInfo]);
}
