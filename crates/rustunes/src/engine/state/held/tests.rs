//! The binding at its interface: built from a capture, guarding an offline
//! phone that holds the captured files, while a hook changes them.
use super::*;
use crate::engine::state::current::{
    DeviceFile,
    fixtures::{artwork_db, cdb, prefs},
};
use idevice::afc::opcode::AfcOpcode;
use music_protocol::protocol::transfer::CaptureControl;
use music_protocol::test_support::afc::{FakePhone, Node, Request};
use music_protocol::test_support::catalogue::Fixture as Catalogue;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

static NEXT: AtomicU64 = AtomicU64::new(0);
/// A trace directory of its own for each connection.
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "held-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Every held file, with the name its drift carries.
const FILES: [(&str, &str); 7] = [
    (device_files::PREFS, "iTunesPrefs"),
    (device_files::PREFS_PLIST, "iTunesPrefs.plist"),
    (device_files::DATABASE, "MediaLibrary.sqlitedb"),
    (device_files::DATABASE_WAL, "MediaLibrary.sqlitedb-wal"),
    (device_files::DATABASE_SHM, "MediaLibrary.sqlitedb-shm"),
    (device_files::CDB, "iTunesCDB"),
    (device_files::ARTWORK_DB, "ArtworkDB"),
];
const MARKER: &str = "iTunes_Control/marker";

#[derive(Clone, Copy)]
struct Shape {
    /// The catalogue has a write-ahead log.
    wal: bool,
    /// A computer left an iTunesCDB (a phone `rustunes init` prepared has
    /// none).
    cdb: bool,
    /// Whether the capture looked for the ArtworkDB (None: it did not) and
    /// found one.
    artwork: Option<bool>,
    /// 2 MiB of text in the catalogue (or its log), read in several requests.
    large: bool,
}

const PLAIN: Shape = Shape {
    wal: false,
    cdb: true,
    artwork: None,
    large: false,
};
const FULL: Shape = Shape {
    wal: true,
    cdb: true,
    artwork: Some(true),
    large: false,
};
/// Every file that may be absent is: a phone `rustunes init` prepared,
/// whose catalogue SQLite has checkpointed.
const BARE: Shape = Shape {
    wal: false,
    cdb: false,
    artwork: Some(false),
    large: false,
};

/// What a capture copied, and the files of a phone that still holds them.
struct Captured {
    current: Current,
    tree: BTreeMap<String, Node>,
}

impl Captured {
    fn new(shape: Shape) -> Self {
        let catalogue = Catalogue::new(shape.wal);
        if shape.large {
            let text = "x".repeat(1 << 20);
            catalogue
                .connection
                .execute("UPDATE item_extra SET comment=?1, copyright=?1", [text])
                .unwrap();
        }
        let catalog = catalogue.read(shape.wal).unwrap();
        let hashes = catalog.source_hashes();
        let mut phone = FakePhone::new(&[
            "iTunes_Control",
            "iTunes_Control/iTunes",
            "iTunes_Control/Artwork",
        ]);
        let database = fs::read(&catalogue.database).unwrap();
        assert_eq!(hashes.database, <[u8; 32]>::from(Sha256::digest(&database)));
        phone.file(device_files::DATABASE, &database);
        if shape.wal {
            let wal = fs::read(catalogue.database.with_file_name("fixture.db-wal")).unwrap();
            assert_eq!(hashes.wal, Some(Sha256::digest(&wal).into()));
            phone.file(device_files::DATABASE_WAL, &wal);
        }
        phone.file(device_files::DATABASE_SHM, &[0; 32]);
        phone.file(device_files::PREFS, &prefs());
        phone.file(device_files::PREFS_PLIST, &prefs());
        let cdb = match shape.cdb {
            true => {
                let bytes = cdb(catalog.database_pid(), 1);
                phone.file(device_files::CDB, &bytes);
                DeviceFile::Present(bytes)
            }
            false => DeviceFile::Absent,
        };
        let artwork_db = match shape.artwork {
            None => DeviceFile::NotCaptured,
            Some(false) => DeviceFile::Absent,
            Some(true) => {
                phone.file(device_files::ARTWORK_DB, &artwork_db());
                DeviceFile::Present(artwork_db())
            }
        };
        let library = identity::parse(&prefs()).unwrap();
        Self {
            current: Current {
                device: "synthetic".into(),
                identity: Identity::new("synthetic", &library).unwrap(),
                library,
                catalog,
                raw_preferences: prefs(),
                plist_preferences: prefs(),
                cdb,
                artwork_db,
                receipt_sha256: [7; 32],
            },
            tree: phone.tree,
        }
    }

    fn held(&self, artwork: ArtworkDb) -> HeldState {
        HeldState::new(&self.current, [9; 32], artwork).unwrap()
    }

    /// A phone holding the captured files.
    fn phone(&self) -> FakePhone {
        let mut phone = FakePhone::new(&[]);
        phone.tree = self.tree.clone();
        phone
    }
}

fn control() -> CaptureControl {
    CaptureControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

/// `state`'s guard on `phone`, over a connection allowed the requests the
/// binding reports and no more, with `inside` stating `MARKER`: a hook that
/// sees that stat acts during the guard. Returns what the guard gave and the
/// phone with its request log, after checking the log fits the cost.
async fn guard(state: &HeldState, phone: FakePhone, inside: bool) -> (Result<()>, FakePhone) {
    let trace = Temp::new();
    let allowance = state.guard_requests() + u64::from(inside);
    let (mut afc, server) = phone
        .connect(&trace.0, &control(), allowance.try_into().unwrap())
        .unwrap();
    let result = match inside {
        false => state.guard(&mut afc).await,
        true => {
            state
                .guard_around(&mut afc, async |afc| {
                    ensure!(afc.stat(MARKER).await?.is_none(), "marker present");
                    Ok(())
                })
                .await
        }
    };
    drop(afc);
    let phone = server.await.unwrap();
    assert!(phone.log.len() as u64 <= allowance);
    (result, phone)
}

/// Make `change` when the guard's inside runs.
fn during(
    change: impl Fn(&mut FakePhone) + Send + 'static,
) -> impl FnMut(&Request, &mut FakePhone) + Send + 'static {
    move |request, phone| {
        if request.path.as_deref() == Some(MARKER) {
            change(phone);
        }
    }
}

/// The drift `result` failed with: the file it names and how it differed.
/// Only a bound file there with other contents grants a refresh, wherever
/// the caller adds context.
fn drift(result: Result<()>) -> (&'static str, Drift) {
    let error = result.unwrap_err().context("a caller's context");
    let drift = error
        .downcast_ref::<StateReadDrift>()
        .unwrap_or_else(|| panic!("not drift: {error:#}"));
    assert_eq!(
        is_refreshable_drift(&error),
        matches!(drift.drift, Drift::Size | Drift::Digest)
    );
    (drift.file, drift.drift)
}

fn count(phone: &FakePhone, operation: AfcOpcode) -> usize {
    phone.log.iter().filter(|op| **op == operation).count()
}

#[tokio::test]
async fn unchanged_state_passes_within_its_reported_cost() {
    for wal in [false, true] {
        for cdb in [false, true] {
            for artwork in [None, Some(false), Some(true)] {
                let shape = Shape {
                    wal,
                    cdb,
                    artwork,
                    large: wal && cdb && artwork == Some(true),
                };
                let captured = Captured::new(shape);
                let hold = match artwork {
                    None => ArtworkDb::NotHeld,
                    Some(_) => ArtworkDb::Held,
                };
                let state = captured.held(hold);
                // Replies of 64 KiB, the least the cost allows for, too.
                for limit in [usize::MAX, 64 * 1024] {
                    let mut phone = captured.phone();
                    phone.read_limit = limit;
                    let (result, phone) = guard(&state, phone, false).await;
                    result.unwrap();
                    // Each held file is stat before and after; each bound
                    // one is read back once, between a stat and a stat.
                    let held = 6 + usize::from(artwork.is_some());
                    let bound = 3
                        + usize::from(wal)
                        + usize::from(cdb)
                        + usize::from(artwork == Some(true));
                    assert_eq!(count(&phone, AfcOpcode::FileOpen), bound);
                    assert_eq!(count(&phone, AfcOpcode::FileClose), bound);
                    assert_eq!(count(&phone, AfcOpcode::GetFileInfo), 2 * (held + bound));
                }
            }
        }
    }
}

#[tokio::test]
async fn a_large_catalogue_is_read_in_several_requests_within_the_cost() {
    let captured = Captured::new(Shape {
        large: true,
        ..PLAIN
    });
    let database = captured.current.catalog.source_hashes().database_bytes;
    assert!(database > 2 << 20, "{database}");
    let state = captured.held(ArtworkDb::NotHeld);
    let (result, phone) = guard(&state, captured.phone(), false).await;
    result.unwrap();
    // The prefs pair and the CDB take one request each.
    assert_eq!(
        count(&phone, AfcOpcode::Read) as u64,
        3 + database.div_ceil(1 << 20)
    );
    assert!(state.guard_read_bytes() >= database + 2 * 1232 + 388);
}

#[tokio::test]
async fn each_bound_file_with_other_bytes_is_refreshable_drift_naming_it() {
    let captured = Captured::new(FULL);
    let state = captured.held(ArtworkDb::Held);
    for (path, name) in FILES {
        if path == device_files::DATABASE_SHM {
            continue;
        }
        for (longer, kind) in [(false, Drift::Digest), (true, Drift::Size)] {
            let mut phone = captured.phone();
            let Some(Node::File(mut bytes)) = phone.tree.get(path).cloned() else {
                panic!("{path}");
            };
            match longer {
                false => *bytes.last_mut().unwrap() ^= 1,
                true => bytes.push(0),
            }
            phone.file(path, &bytes);
            let (result, _) = guard(&state, phone, false).await;
            assert_eq!(drift(result), (name, kind));
        }
    }
}

#[tokio::test]
async fn a_bound_file_gone_or_a_file_bound_absent_there_is_drift_naming_it() {
    let captured = Captured::new(FULL);
    let state = captured.held(ArtworkDb::Held);
    for (path, name) in FILES {
        if path == device_files::DATABASE_SHM {
            continue;
        }
        let mut phone = captured.phone();
        phone.tree.remove(path);
        let (result, _) = guard(&state, phone, false).await;
        assert_eq!(drift(result), (name, Drift::Disappeared));
    }
    // The log, the CDB and the ArtworkDB, each captured absent.
    let captured = Captured::new(BARE);
    let state = captured.held(ArtworkDb::Held);
    for (path, name) in [FILES[3], FILES[5], FILES[6]] {
        let mut phone = captured.phone();
        phone.file(path, b"appeared");
        let (result, _) = guard(&state, phone, false).await;
        assert_eq!(drift(result), (name, Drift::Appeared));
    }
}

#[tokio::test]
async fn any_held_file_changed_appearing_or_gone_during_the_guard_is_drift_naming_it() {
    let captured = Captured::new(FULL);
    let state = captured.held(ArtworkDb::Held);
    for (path, name) in FILES {
        // Written again while the caller's checks ran: a new mtime.
        let mut phone = captured.phone();
        phone.before_each(during(move |phone| {
            phone.set_attribute(path, "st_mtime", Some("3"))
        }));
        let (result, _) = guard(&state, phone, true).await;
        assert_eq!(drift(result), (name, Drift::Attributes));
        // Removed while they ran.
        let mut phone = captured.phone();
        phone.before_each(during(move |phone| {
            phone.tree.remove(path);
        }));
        let (result, _) = guard(&state, phone, true).await;
        assert_eq!(drift(result), (name, Drift::Attributes));
    }
    // Appearing while they ran: the files captured absent, and the index,
    // which SQLite may create at any time.
    let captured = Captured::new(BARE);
    let state = captured.held(ArtworkDb::Held);
    for (path, name) in [FILES[3], FILES[4], FILES[5], FILES[6]] {
        let mut phone = captured.phone();
        phone.tree.remove(device_files::DATABASE_SHM);
        phone.before_each(during(move |phone| phone.file(path, b"appeared")));
        let (result, _) = guard(&state, phone, true).await;
        assert_eq!(drift(result), (name, Drift::Attributes));
    }
    // With nothing inside: the prefs written again while the catalogue was
    // read back.
    let captured = Captured::new(FULL);
    let state = captured.held(ArtworkDb::Held);
    let mut phone = captured.phone();
    phone.before_each(|request, phone| {
        if request.operation == AfcOpcode::Read
            && request.path.as_deref() == Some(device_files::DATABASE)
        {
            phone.set_attribute(device_files::PREFS, "st_mtime", Some("3"));
        }
    });
    let (result, _) = guard(&state, phone, false).await;
    assert_eq!(drift(result), ("iTunesPrefs", Drift::Attributes));
}

#[tokio::test]
async fn the_inside_gives_its_value_or_its_own_error() {
    let captured = Captured::new(PLAIN);
    let state = captured.held(ArtworkDb::NotHeld);
    let trace = Temp::new();
    let allowance = 2 * state.guard_requests();
    let (mut afc, server) = captured
        .phone()
        .connect(&trace.0, &control(), allowance.try_into().unwrap())
        .unwrap();
    let value = state.guard_around(&mut afc, async |_| Ok(5)).await;
    assert_eq!(value.unwrap(), 5);
    let refused = state
        .guard_around(&mut afc, async |_| {
            Err::<(), _>(anyhow::anyhow!("staging holds leftovers"))
        })
        .await
        .unwrap_err();
    assert!(refused.downcast_ref::<StateReadDrift>().is_none());
    assert_eq!(refused.to_string(), "staging holds leftovers");
    drop(afc);
    server.await.unwrap();
}

#[tokio::test]
async fn an_artwork_db_not_held_is_never_looked_at() {
    let captured = Captured::new(Shape {
        artwork: Some(true),
        ..PLAIN
    });
    let state = captured.held(ArtworkDb::NotHeld);
    assert!(!state.holds_artwork() && state.artwork_db().is_none());
    // Apple's host rewrites it, adds and removes it at every request.
    let named = Arc::new(Mutex::new(Vec::new()));
    let mut phone = captured.phone();
    phone.before_each({
        let named = named.clone();
        move |request, phone| {
            named.lock().unwrap().push(request.path.clone());
            match phone.tree.contains_key(device_files::ARTWORK_DB) {
                true => phone.tree.remove(device_files::ARTWORK_DB),
                false => phone.tree.insert(
                    device_files::ARTWORK_DB.into(),
                    Node::File(b"another".to_vec()),
                ),
            };
        }
    });
    let (result, _) = guard(&state, phone, false).await;
    result.unwrap();
    let named = named.lock().unwrap();
    assert!(!named.is_empty());
    assert!(
        named
            .iter()
            .all(|path| path.as_deref() != Some(device_files::ARTWORK_DB))
    );
}

#[tokio::test]
async fn an_absent_artwork_db_held_must_stay_absent() {
    let captured = Captured::new(Shape {
        artwork: Some(false),
        ..PLAIN
    });
    let state = captured.held(ArtworkDb::Held);
    assert!(state.holds_artwork() && state.artwork_db().is_none());
    let (result, _) = guard(&state, captured.phone(), false).await;
    result.unwrap();
    let mut phone = captured.phone();
    phone.file(device_files::ARTWORK_DB, &artwork_db());
    let (result, _) = guard(&state, phone, false).await;
    assert_eq!(drift(result), ("ArtworkDB", Drift::Appeared));
}

/// A phone `rustunes init` prepared has no iTunesCDB: its absence is bound
/// at two stats and no read, and the templates take VoiceMemo 1.
#[tokio::test]
async fn a_phone_init_prepared_binds_the_cdb_absent() {
    let mut captured = Captured::new(Shape {
        cdb: false,
        ..PLAIN
    });
    let state = captured.held(ArtworkDb::NotHeld);
    assert_eq!(state.copied_cdb(), None);
    let (result, phone) = guard(&state, captured.phone(), false).await;
    result.unwrap();
    assert_eq!(count(&phone, AfcOpcode::FileOpen), 3);
    assert_eq!(count(&phone, AfcOpcode::GetFileInfo), 2 * 6 + 2 * 3);
    let templates = state.templates("host").unwrap();
    assert_eq!(anchor(&templates.request, "VoiceMemo"), "1");
    assert_eq!(anchor(&templates.finished, "VoiceMemo"), "1");
    // The CDB present instead costs its read.
    let pid = captured.current.catalog.database_pid();
    captured.current.cdb = DeviceFile::Present(cdb(pid, 1));
    let with = captured.held(ArtworkDb::NotHeld);
    assert_eq!(with.guard_requests() - state.guard_requests(), 4 + 1);
}

fn anchor(message: &plist::Value, dataclass: &str) -> String {
    let params = message.as_dictionary().unwrap().get("Params").unwrap();
    let anchors = params.as_dictionary().unwrap().get("DataclassAnchors");
    let anchors = anchors.unwrap().as_dictionary().unwrap();
    anchors.get(dataclass).unwrap().as_string().unwrap().into()
}

#[test]
fn the_binding_qualifies_the_copied_cdb_and_bounds_the_artwork_db() {
    let captured = Captured::new(PLAIN);
    let current = &captured.current;
    let pid = current.catalog.database_pid();
    let mut other = Captured::new(PLAIN);
    for bytes in [
        cdb(0, 1),
        cdb(pid ^ 1, 1),
        cdb(pid, 0),
        cdb(pid, 2),
        cdb(pid, 1)[..100].to_vec(),
    ] {
        other.current.cdb = DeviceFile::Present(bytes);
        assert!(HeldState::new(&other.current, [9; 32], ArtworkDb::NotHeld).is_err());
    }
    // A capture that did not look is not taken for an absence.
    other.current.cdb = DeviceFile::NotCaptured;
    assert!(HeldState::new(&other.current, [9; 32], ArtworkDb::NotHeld).is_err());
    // The bound CDB is these bytes; its stale Media anchor 7 never replaces
    // the catalogue's.
    let state = captured.held(ArtworkDb::NotHeld);
    let bytes = cdb(pid, 1);
    assert_eq!(
        state.copied_cdb(),
        Some((bytes.len() as u64, Sha256::digest(&bytes).into()))
    );
    let media = current.catalog.persisted_media_anchor();
    assert_ne!(media, 7);
    let templates = state.templates("host").unwrap();
    assert_eq!(anchor(&templates.request, "Media"), media.to_string());
    // A held ArtworkDB the capture did not look for, an empty one or one
    // over its bound is refused; one not held is not read.
    assert!(HeldState::new(current, [9; 32], ArtworkDb::Held).is_err());
    for bytes in [Vec::new(), vec![0; MAX_ASSET as usize + 1]] {
        other.current.cdb = DeviceFile::Present(cdb(pid, 1));
        other.current.artwork_db = DeviceFile::Present(bytes);
        assert!(HeldState::new(&other.current, [9; 32], ArtworkDb::Held).is_err());
        assert!(HeldState::new(&other.current, [9; 32], ArtworkDb::NotHeld).is_ok());
    }
    // Every binding names the receipt of the input that decided what to send.
    assert!(HeldState::new(current, [0; 32], ArtworkDb::NotHeld).is_err());
}

#[test]
fn only_the_rows_named_unfilled_may_lack_a_location() {
    let catalogue = Catalogue::new(false);
    catalogue
        .connection
        .execute_batch(
            "UPDATE item SET base_location_id=0 WHERE item_pid=1;
             UPDATE item_extra SET location='' WHERE item_pid=1;
             UPDATE item_state SET has_non_purgeable_asset=0 WHERE item_pid=1;",
        )
        .unwrap();
    let mut captured = Captured::new(PLAIN);
    captured.current.catalog = catalogue.read(false).unwrap();
    let pid = captured.current.catalog.database_pid();
    captured.current.cdb = DeviceFile::Present(cdb(pid, 1));
    let current = &captured.current;
    let track = &current.catalog.tracks()[0];
    assert!(track.afc_path.is_none());
    assert!(HeldState::new(current, [9; 32], ArtworkDb::NotHeld).is_err());
    let unfilled = BTreeSet::from([track.sync_id]);
    let state = HeldState::with_unfilled(current, [9; 32], ArtworkDb::NotHeld, &unfilled);
    assert!(state.unwrap().paths().is_empty());
    let other = BTreeSet::from([track.sync_id ^ 1]);
    assert!(HeldState::with_unfilled(current, [9; 32], ArtworkDb::NotHeld, &other).is_err());
}

/// A bound file's stat of an unsupported shape is an ordinary refusal, never
/// drift that could grant a refresh; only the typed drift does, not text
/// that reads like it.
#[tokio::test]
async fn a_malformed_or_oversized_stat_is_a_refusal_not_drift() {
    let captured = Captured::new(PLAIN);
    let state = captured.held(ArtworkDb::NotHeld);
    for (key, value) in [
        ("st_ifmt", Some("S_IFLNK")),
        ("st_size", Some("not-a-number")),
        ("st_size", Some("99999999999")),
        ("st_size", None),
        ("st_mtime", Some("invalid")),
        ("st_mtime", None),
    ] {
        let mut phone = captured.phone();
        phone.set_attribute(device_files::DATABASE, key, value);
        let (result, _) = guard(&state, phone, false).await;
        let error = result.unwrap_err().context("a caller's context");
        assert!(
            error.downcast_ref::<StateReadDrift>().is_none() && !is_refreshable_drift(&error),
            "{key}: {error:#}"
        );
    }
    assert!(!is_refreshable_drift(&anyhow::anyhow!(
        "state read drift in MediaLibrary.sqlitedb-wal: regular file size changed"
    )));
}
