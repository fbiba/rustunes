//! Full native schema fixtures for the Clear command's playlist choice.
use super::*;
use crate::engine::verification::sql_tables::{
    Capture,
    reader::{self, Budget},
    tests::Fixture,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

/// Library container -3, Playback History 77, the empty ordinary playlists 88
/// (with a membership revision) and 89, and 90 holding item 2. Each ordinary
/// playlist has its seed sentinel and live revision; last synced is 8.
const PLAYLISTS: &str = "INSERT INTO container(container_pid,name,is_hidden)VALUES(-3,'Library',1);
    INSERT INTO container(container_pid,name,distinguished_kind,is_hidden)VALUES(77,'Playback History',10000,1);
    INSERT INTO container(container_pid,sync_id,name,is_src_remote,contained_media_type,keep_local_status)VALUES
        (88,88,'First',1,0,2),(89,89,'Second',1,0,2),(90,90,'Held',1,8,2);
    INSERT INTO container_item(container_item_pid,container_pid,item_pid,position,occurrence_id)VALUES(500,-3,2,0,''),(631,90,2,0,'2_0');
    INSERT INTO container_item_media_type(container_pid,media_type,count)VALUES(90,8,1);
    INSERT INTO container_seed(container_pid,item_pid,seed_order)VALUES(88,0,0),(89,0,0),(90,0,0);
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES
        (5,88,0,1,0),(6,88,0,1,1),(7,89,0,1,0),(8,90,0,1,0);
    UPDATE _MLDatabaseProperties SET value='8' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');";

fn tables(path: &Path, out: &Path) -> Tables {
    reader::materialize(
        Capture {
            database: path,
            database_sha256: Sha256::digest(fs::read(path).unwrap()).into(),
            wal: None,
        },
        out,
        &mut Budget::new(),
    )
    .unwrap()
    .tables
}

struct Pair(Fixture);
impl Pair {
    fn new(both: &str, after: &str) -> Self {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
        // The fixture's own after state removes a track; start from before.
        fs::copy(&fixture.before, &fixture.after).unwrap();
        for path in [&fixture.before, &fixture.after] {
            Connection::open(path).unwrap().execute_batch(both).unwrap();
        }
        Connection::open(&fixture.after)
            .unwrap()
            .execute_batch(after)
            .unwrap();
        Self(fixture)
    }
    fn tables(&self) -> (Tables, Tables) {
        let dir = &self.0.dir;
        (
            tables(&self.0.before, &dir.join("b")),
            tables(&self.0.after, &dir.join("a")),
        )
    }
}

#[test]
fn a_clear_deletes_ordinary_playlists_and_keeps_the_rest() {
    let pair = Pair::new(
        &format!(
            "{PLAYLISTS} INSERT INTO container(container_pid,sync_id,name,is_src_remote,smart_is_dynamic,smart_criteria)VALUES(92,92,'Smart',1,1,x'00');
            INSERT INTO container(container_pid,sync_id,name)VALUES(93,0,'Made here');"
        ),
        "",
    );
    let (before, _) = pair.tables();
    let found = clear_playlists(&before).unwrap();
    let named = |list: &[(i64, String)]| -> Vec<(i64, String)> { list.to_vec() };
    assert_eq!(
        named(&found.delete),
        [
            (88, "First".into()),
            (89, "Second".into()),
            (90, "Held".into())
        ]
    );
    assert_eq!(
        named(&found.kept),
        [(92, "Smart".into()), (93, "Made here".into())]
    );
    // While tracks remain, 90's member and count are allowed; the deletion
    // itself still needs it empty, and other references refuse either way.
    qualify_clear(&before, &[88, 89, 90]).unwrap();
    assert!(qualify_deletion(&before, &[88, 89, 90]).is_err());
    assert!(qualify_clear(&before, &[92]).is_err());
    let pair = Pair::new(
        &format!(
            "{PLAYLISTS} INSERT INTO artwork_token(entity_pid,artwork_token,entity_type)VALUES(90,'t',1);"
        ),
        "",
    );
    let (before, _) = pair.tables();
    assert!(qualify_clear(&before, &[90]).is_err());
}
