//! Carrying a journal forward. `read_prior` reads what a journal any build
//! wrote still tells: which phone tracks it bound, from which sources and with
//! which covers, and which identities it reserved. It reads a private copy by
//! a few long-standing columns, so neither the journal's schema nor its hash
//! definitions need to match this build's, and nothing it says is trusted
//! until a fresh capture confirms it. `Journal::record_pull` writes what a
//! pull confirmed into a new journal.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// An identity a journal reserved for good: a track (namespace 1), a cover
/// cache ID (2) or a canonical lowercase music path (3). A new journal keeps
/// them, so none is ever handed out again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reserved {
    pub namespace: i64,
    pub value: Vec<u8>,
}

/// An active binding of an earlier journal.
pub struct PriorBinding {
    /// Its last verified content, its audio identities as this build computes
    /// them from the stored components.
    pub content: Content,
    /// The absolute source path it records or its insert batch's input names.
    pub source: Option<PathBuf>,
    /// The FLAC it was converted from. Its `source_audio` is as stored.
    pub conversion: Option<Conversion>,
}

/// What an earlier journal tells a pull.
pub struct Prior {
    pub bindings: Vec<PriorBinding>,
    pub reserved: BTreeSet<Reserved>,
    /// Work it left unfinished, described. Its effect is whatever the phone
    /// holds now, which the pull reads.
    pub unfinished: Vec<String>,
    /// Bindings it holds that this build cannot read, described; their
    /// tracks are matched like any other phone track.
    pub unreadable: Vec<String>,
}

/// Largest batch input read for its source paths.
const MAX_INPUT: u64 = 64 << 20;

/// What a journal names in its state's `runs`, unfinished work apart from
/// finished. Settling unfinished work reads its whole run again, and `resume`
/// finds an unfinished operation's run by its input receipt. Planning reads
/// only a finished batch's or operation's input and plan again.
pub struct RunReferences {
    pub unfinished: RunNames,
    pub finished: RunNames,
}

/// The evidence directory of each batch and replacement, and the input
/// receipt (the SHA-256 of an `input.json` a run holds) of each single-track
/// operation.
#[derive(Default)]
pub struct RunNames {
    pub evidence: Vec<PathBuf>,
    pub inputs: BTreeSet<[u8; 32]>,
}

/// Each table of work, the states in which that work is unfinished, and what
/// it is.
const UNFINISHED: [(&str, &str, &str); 5] = [
    ("attempts", "0,1", "an unfinished single-track operation"),
    ("insert_batches", "0,1,5", "an unfinished insert batch"),
    ("removal_batches", "0,1", "an unfinished removal batch"),
    ("metadata_batches", "0,1", "an unfinished metadata batch"),
    ("coordinations", "0", "an unfinished replacement or Clear"),
];

/// Open a private copy of the journal in `directory`, made in `scratch`, a
/// new directory. The original is only read.
fn private_copy(directory: &Path, scratch: &Path) -> Result<Connection> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(scratch)
        .context("creating the private copy of the journal")?;
    // An interrupted write leaves a rollback journal beside the database;
    // opening the copy rolls it back there, never in the original.
    for name in [DB_NAME, "journal.sqlite-journal", "journal.sqlite-wal"] {
        let from = directory.join(name);
        let mut source = match File::open(&from) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && name != DB_NAME => {
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", from.display()));
            }
        };
        ensure!(
            source.metadata()?.is_file(),
            "{} is not a file",
            from.display()
        );
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(scratch.join(name))?;
        std::io::copy(&mut source, &mut copy)?;
        copy.sync_all()?;
    }
    let c = Connection::open(scratch.join(DB_NAME))?;
    ensure!(
        c.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? == APPLICATION_ID,
        "{} is not a rustunes journal",
        directory.display()
    );
    Ok(c)
}

fn has_table(c: &Connection, table: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}

/// Read what the journal in `directory` names in `runs`, from a private copy
/// made in `scratch`, a new directory.
pub fn run_references(directory: &Path, scratch: &Path) -> Result<RunReferences> {
    let c = private_copy(directory, scratch)?;
    let mut references = RunReferences {
        unfinished: RunNames::default(),
        finished: RunNames::default(),
    };
    // A schema without one of these tables names nothing through it.
    for (table, states, _) in UNFINISHED {
        if !has_table(&c, table)? {
            continue;
        }
        let column = if table == "attempts" {
            "input_receipt"
        } else {
            "evidence"
        };
        let mut q = c.prepare(&format!("SELECT {column},state IN({states}) FROM {table}"))?;
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            let named = if r.get(1)? {
                &mut references.unfinished
            } else {
                &mut references.finished
            };
            if table == "attempts" {
                named.inputs.insert(
                    r.get::<_, Vec<u8>>(0)?
                        .as_slice()
                        .try_into()
                        .context("an attempt's input receipt is not 32 bytes")?,
                );
            } else {
                named.evidence.push(PathBuf::from(r.get::<_, String>(0)?));
            }
        }
    }
    Ok(references)
}

/// Read the journal in `directory` from a private copy made in `scratch`, a
/// new directory. The original is only read.
pub fn read_prior(directory: &Path, scratch: &Path) -> Result<Prior> {
    let c = private_copy(directory, scratch)?;
    let has = |table: &str| has_table(&c, table);
    let mut prior = Prior {
        bindings: Vec::new(),
        reserved: BTreeSet::new(),
        unfinished: Vec::new(),
        unreadable: Vec::new(),
    };
    let sources = if has("binding_sources")? {
        recorded_sources(&c)?
    } else {
        BTreeMap::new()
    };
    let batch_sources = batch_sources(&c, &mut prior.unreadable)?;
    let conversions = conversions(&c, has("binding_conversions")?)?;
    let mut q = c.prepare("SELECT r.id,r.track,r.path,v.content FROM active_bindings a JOIN binding_records r ON r.id=a.binding_id JOIN binding_versions v ON v.binding_id=a.binding_id AND v.version=a.version ORDER BY r.id LIMIT 50001")?;
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        ensure!(
            prior.bindings.len() < 50_000,
            "earlier journal binding bound"
        );
        let id: i64 = r.get(0)?;
        let track: Vec<u8> = r.get(1)?;
        let path: String = r.get(2)?;
        let read = (|| -> Result<PriorBinding> {
            let content = decode_content_as(&r.get::<_, Vec<u8>>(3)?, false)?;
            ensure!(
                content.track_pid.to_be_bytes().as_slice() == track && content.path == path,
                "binding key differs from its content"
            );
            let conversion = conversions.get(&id).cloned();
            let source = sources
                .get(&id)
                .or_else(|| batch_sources.get(&id))
                .cloned()
                .or_else(|| conversion.as_ref().map(|c| c.source_path.clone()));
            Ok(PriorBinding {
                content,
                source,
                conversion,
            })
        })();
        match read {
            Ok(binding) => prior.bindings.push(binding),
            Err(error) => prior.unreadable.push(format!("{path}: {error:#}")),
        }
    }
    drop(rows);
    drop(q);
    prior.reserved = reserved(&c)?;
    for (table, states, what) in UNFINISHED {
        let count = c.query_row(
            &format!("SELECT count(*) FROM {table} WHERE state IN({states})"),
            [],
            |r| r.get::<_, i64>(0),
        );
        match count {
            Ok(0) => {}
            Ok(_) => prior.unfinished.push(what.into()),
            // A later schema without this table has nothing of it to finish.
            Err(_) if !has(table)? => {}
            Err(error) => prior.unfinished.push(format!("{what} ({error})")),
        }
    }
    Ok(prior)
}

fn recorded_sources(c: &Connection) -> Result<BTreeMap<i64, PathBuf>> {
    let mut q = c.prepare("SELECT binding_id,source FROM binding_sources LIMIT 100001")?;
    let rows = q.query_map([], |r| {
        Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Each batch-bound binding's source path, as its batch's retained input
/// names it. The input is trusted only when its bytes have the SHA-256 the
/// journal recorded for it; any other trouble leaves the source unnamed.
fn batch_sources(c: &Connection, unreadable: &mut Vec<String>) -> Result<BTreeMap<i64, PathBuf>> {
    let mut sources = BTreeMap::new();
    let mut q = match c.prepare("SELECT b.id,b.evidence,b.input_receipt,t.binding_id,t.track FROM insert_targets t JOIN insert_batches b ON b.id=t.batch WHERE t.outcome=1 AND t.binding_id IS NOT NULL ORDER BY b.id") {
        Ok(q) => q,
        // A schema without insert batches has no batch sources.
        Err(_) => return Ok(sources),
    };
    let mut targets: BTreeMap<i64, (PathBuf, Vec<u8>, Vec<(i64, Vec<u8>)>)> = BTreeMap::new();
    let mut rows = q.query([])?;
    while let Some(r) = rows.next()? {
        let entry = targets
            .entry(r.get(0)?)
            .or_insert_with(|| (PathBuf::new(), Vec::new(), Vec::new()));
        entry.0 = PathBuf::from(r.get::<_, String>(1)?);
        entry.1 = r.get(2)?;
        entry.2.push((r.get(3)?, r.get(4)?));
    }
    for (batch, (evidence, receipt, bound)) in targets {
        let read = (|| -> Result<BTreeMap<String, PathBuf>> {
            let path = evidence.join("input.json");
            let file = File::open(&path)?;
            ensure!(file.metadata()?.len() <= MAX_INPUT, "batch input too large");
            let mut bytes = Vec::new();
            file.take(MAX_INPUT).read_to_end(&mut bytes)?;
            ensure!(
                Sha256::digest(&bytes).as_slice() == receipt,
                "its retained input differs from the journal"
            );
            let input: serde_json::Value = serde_json::from_slice(&bytes)?;
            let mut named = BTreeMap::new();
            for member in input["members"].as_array().context("no members")? {
                let (Some(track), Some(source)) =
                    (member["track"].as_str(), member["source_path"].as_str())
                else {
                    continue;
                };
                if source.starts_with('/') {
                    named.insert(track.to_owned(), PathBuf::from(source));
                }
            }
            Ok(named)
        })();
        match read {
            Ok(named) => {
                for (binding, track) in bound {
                    let track = track
                        .try_into()
                        .map(|t: [u8; 8]| format!("{:016x}", u64::from_be_bytes(t)));
                    if let Some(source) = track.ok().and_then(|t| named.get(&t)) {
                        sources.insert(binding, source.clone());
                    }
                }
            }
            Err(error) => {
                unreadable.push(format!("insert batch {batch}'s source paths: {error:#}"))
            }
        }
    }
    Ok(sources)
}

fn conversions(c: &Connection, pulled: bool) -> Result<BTreeMap<i64, Conversion>> {
    let mut out = BTreeMap::new();
    let mut queries = vec![
        "SELECT t.binding_id,c.source_path,c.source_bytes,c.source_sha,c.source_audio,c.ffmpeg FROM insert_conversions c JOIN insert_targets t ON t.batch=c.batch AND t.ordinal=c.ordinal WHERE t.outcome=1 AND t.binding_id IS NOT NULL",
    ];
    if pulled {
        queries.push("SELECT binding_id,source_path,source_bytes,source_sha,source_audio,ffmpeg FROM binding_conversions");
    }
    for sql in queries {
        let Ok(mut q) = c.prepare(sql) else {
            continue;
        };
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            let conversion = (|| -> Result<Conversion> {
                Ok(Conversion {
                    source_path: PathBuf::from(r.get::<_, String>(1)?),
                    source_bytes: u64::try_from(r.get::<_, i64>(2)?)?,
                    source_sha256: array(&r.get::<_, Vec<u8>>(3)?)?,
                    source_audio: array(&r.get::<_, Vec<u8>>(4)?)?,
                    ffmpeg: r.get(5)?,
                })
            })();
            if let Ok(conversion) = conversion {
                out.insert(r.get(0)?, conversion);
            }
        }
    }
    Ok(out)
}

/// Every identity the journal reserved or saw removed, as the namespaces this
/// build accepts. Values this build would refuse are dropped: the fresh
/// capture's own rows are reserved again by the pull.
fn reserved(c: &Connection) -> Result<BTreeSet<Reserved>> {
    let mut out = BTreeSet::new();
    let mut add = |namespace: i64, value: Vec<u8>| {
        let valid = match namespace {
            1 => array::<8>(&value).is_ok_and(|v| u64::from_be_bytes(v) != 0),
            2 => array::<4>(&value).is_ok_and(|v| u32::from_be_bytes(v) >= 100),
            3 => decode_reserved_path(&value).is_ok(),
            _ => false,
        };
        if valid {
            out.insert(Reserved { namespace, value });
        }
    };
    for query in [
        "SELECT namespace,value FROM reservations",
        "SELECT 1,track FROM removal_targets",
        "SELECT 3,CAST(lower(path) AS BLOB) FROM removal_targets WHERE path IS NOT NULL",
    ] {
        let Ok(mut q) = c.prepare(&format!("{query} LIMIT 250001")) else {
            continue;
        };
        let mut rows = q.query([])?;
        while let Some(r) = rows.next()? {
            add(r.get(0)?, r.get(1)?);
        }
    }
    ensure!(out.len() <= 100_000, "earlier journal reservation bound");
    Ok(out)
}

/// One binding a pull records from a fresh capture.
pub struct Pulled {
    pub content: Content,
    /// The absolute path of the source file whose audio the track holds.
    pub source: Option<PathBuf>,
    pub conversion: Option<Conversion>,
}

impl Journal {
    /// Record a pull into this journal, which must be new and empty: each
    /// binding as its first version (provenance 3, receipt kind 4) under the
    /// capture's `receipt` and Media `anchor`, and every identity in
    /// `reserved` together with the bindings' own, all consumed.
    pub fn record_pull(
        &mut self,
        pulled: &[Pulled],
        reserved: &BTreeSet<Reserved>,
        receipt: [u8; 32],
        anchor: u32,
    ) -> Result<()> {
        self.usable()?;
        ensure!(
            self._read_only_copy.is_none(),
            "read-only journal cannot record a pull"
        );
        nonzero_hash(&receipt)?;
        ensure!(pulled.len() < 50_000, "pull binding bound");
        let mut reservations = reserved.clone();
        for p in pulled {
            validate_content(&p.content)?;
            reservations.insert(Reserved {
                namespace: 1,
                value: p.content.track_pid.to_be_bytes().to_vec(),
            });
            reservations.insert(Reserved {
                namespace: 3,
                value: p.content.path.to_ascii_lowercase().into_bytes(),
            });
            if let Some(art) = &p.content.artwork {
                reservations.insert(Reserved {
                    namespace: 2,
                    value: art.cache_id.to_be_bytes().to_vec(),
                });
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let used: i64 = tx.query_row(
            "SELECT (SELECT count(*) FROM binding_records)+(SELECT count(*) FROM attempts)+(SELECT count(*) FROM reservations)+(SELECT count(*) FROM insert_batches)+(SELECT count(*) FROM removal_batches)+(SELECT count(*) FROM metadata_batches)+(SELECT count(*) FROM coordinations)",
            [],
            |r| r.get(0),
        )?;
        ensure!(used == 0, "a pull is recorded into a new, empty journal");
        for r in &reservations {
            reserve(&tx, r.namespace, &r.value, None, true)?;
        }
        for p in pulled {
            let content = &p.content;
            tx.execute(
                "INSERT INTO binding_records(audio,track,path,first_attempt,provenance) VALUES(?1,?2,?3,NULL,3)",
                params![
                    content.source.audio.sha256,
                    content.track_pid.to_be_bytes(),
                    content.path
                ],
            )
            .context("two pulled tracks share an audio identity, track or path")?;
            let binding = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO binding_versions VALUES(?1,1,?2,?3,?4,NULL,4)",
                params![binding, encode_content(content)?, receipt, anchor],
            )?;
            tx.execute(
                "INSERT INTO active_bindings VALUES(?1,?2,1)",
                params![content.source.audio.sha256, binding],
            )?;
            if let Some(source) = &p.source {
                tx.execute(
                    "INSERT INTO binding_sources VALUES(?1,?2)",
                    params![
                        binding,
                        source.to_str().context("source path is not UTF-8")?
                    ],
                )?;
            }
            if let Some(c) = &p.conversion {
                tx.execute(
                    "INSERT INTO binding_conversions VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        binding,
                        c.source_path.to_str().context("FLAC path is not UTF-8")?,
                        i64::try_from(c.source_bytes)?,
                        c.source_sha256,
                        c.source_audio,
                        c.ffmpeg
                    ],
                )?;
            }
        }
        insertion_batch::validate_storage(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }
}
