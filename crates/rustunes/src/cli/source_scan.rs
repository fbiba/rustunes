//! Read-only CLI reporting for source selection and inspection. This does not
//! compare a device catalog, establish ownership, or prepare a sync operation.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::engine::planning::source_collection::{
    self, Selection, SelectionOptions, SourceCollection,
};

/// Print one complete source report. False means an empty selection, inspection
/// failures or unresolved audio duplicate groups; it never authorizes syncing.
pub fn run(
    root: &Path,
    options: &SelectionOptions,
    selection_only: bool,
    json: bool,
) -> Result<bool> {
    let selection = source_collection::select(root, options)?;
    let collection = if selection_only {
        None
    } else {
        let mut last = std::time::Instant::now();
        Some(source_collection::inspect_with_progress(
            &selection,
            |done, total| {
                if super::machine::enabled() {
                    return super::machine::progress(
                        super::machine::Progress::new(
                            "inspect_sources",
                            "files",
                            done,
                            Some(total),
                        )
                        .message(format!("Inspected {done}/{total} source files")),
                    );
                }
                if done == 0 || done == total || last.elapsed().as_secs() >= 5 {
                    eprintln!("Inspected {done}/{total} source files");
                    last = std::time::Instant::now();
                }
                Ok(())
            },
        )?)
    };
    let report = report(&selection, collection.as_ref())?;
    let success = report["success"]
        .as_bool()
        .context("source report missing success state")?;
    if super::machine::enabled() {
        if !success {
            super::machine::outcome("findings");
        }
        super::machine::report("scan", &report)?;
        return Ok(success);
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if json {
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        stdout.write_all(b"\n")?;
    } else {
        stdout.write_all(human(&report)?.as_bytes())?;
    }
    stdout.flush()?;
    Ok(success)
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("source report requires UTF-8 paths")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn report(selection: &Selection, collection: Option<&SourceCollection>) -> Result<Value> {
    let stats = selection.stats();
    let options = selection.options();
    let success = !selection.files().is_empty()
        && collection.is_none_or(|c| {
            c.failures.is_empty() && c.duplicates.is_empty() && c.mp3_duplicates().is_empty()
        });
    let selected = selection.files().iter().map(|source| Ok(json!({
        "relative_path":source.relative_path(), "path":path_text(source.path())?, "file_bytes":source.file_bytes()
    }))).collect::<Result<Vec<_>>>()?;
    let excluded_counts: serde_json::Map<String, Value> = stats
        .excluded
        .iter()
        .map(|(reason, count)| (reason.as_str().to_owned(), json!(count)))
        .collect();
    let exclusions: Vec<_> = selection
        .exclusions()
        .iter()
        .map(|excluded| {
            json!({
                "relative_path":excluded.relative_path, "reason":excluded.reason.as_str(),
                "descendants_unscanned":excluded.descendants_unscanned
            })
        })
        .collect();
    let mut result = json!({
        "schema":"rustunes.source-scan",
        "mode":if collection.is_some() {"inspection"} else {"selection-only"},
        "success":success, "root":path_text(selection.root())?, "base_directory":path_text(selection.base_directory())?,
        "scope":{"source_only":true,"device_compared":false,"sync_plan":false,"sync_compatibility_guaranteed":false},
        "options":{"recursive":options.recursive,
            "include":options.include,"exclude":options.exclude},
        "counts":{"selected_files":stats.selected_files,"selected_bytes":stats.selected_bytes,
            "visited_entries":stats.visited_entries,"visited_directories":stats.visited_directories,
            "excluded_entries":selection.exclusions().len(),"excluded_by_reason":excluded_counts},
        "selected":selected,"exclusions":exclusions
    });
    if let Some(collection) = collection {
        let mut tracks: Vec<_> = collection.tracks.iter().map(|track| {
            let m = &track.metadata;
            let f = &track.fingerprint;
            json!({"relative_path":track.relative_path,
                "metadata":{"title":m.title,"artist":m.artist,"album":m.album,"album_artist":m.album_artist,
                    "genre":m.genre,"composer":m.composer,"copyright":m.copyright,"year":m.year,
                    "release_date":m.release_date,"compilation":m.compilation,
                    "duplicate_outer_atoms":m.duplicate_outer_atoms(),
                    "content_rating":m.content_rating,"track_number":m.track,"track_total":m.track_total,
                    "disc_number":m.disc,"disc_total":m.disc_total},
                "audio_properties":{"sample_rate":m.sample_rate,"bit_depth":m.bit_depth,"channels":m.channels,
                    "timescale":m.timing.timescale,"duration_ticks":m.timing.duration_ticks,"packets":m.timing.packets},
                "fingerprint":{"file_bytes":f.file_bytes,"full_file_sha256":hex(&f.full_file_sha256),
                    "encoded_audio":{"format":"encoded-alac","sha256":hex(&f.audio.sha256),
                        "alac_config_sha256":hex(&f.audio.alac_config_sha256),
                        "sample_sizes_sha256":hex(&f.audio.sample_sizes_sha256),"media_sha256":hex(&f.audio.media_sha256),
                        "timescale":f.audio.timescale,"duration_ticks":f.audio.duration_ticks,"media_bytes":f.audio.media_bytes}}})
        }).collect();
        for track in &collection.mp3_tracks {
            let mut row = serde_json::to_value(&track.source)?;
            row["relative_path"] = json!(track.relative_path);
            tracks.push(row);
        }
        if !collection.mp3_tracks.is_empty() {
            tracks.sort_by(|a, b| {
                a["relative_path"]
                    .as_str()
                    .cmp(&b["relative_path"].as_str())
            });
        }
        let failures: Vec<_> = collection
            .failures
            .iter()
            .map(|failure| json!({"relative_path":failure.relative_path,"message":failure.message}))
            .collect();
        let duplicates: Vec<_> = collection.duplicates.iter().map(|group|json!({
            "encoded_alac_sha256":hex(&group.encoded_alac_sha256),"relative_paths":group.relative_paths,
            "identical_complete_files":group.identical_complete_files,"identical_standard_metadata":group.identical_standard_metadata,
            "resolution":"explicit-selection-required"
        })).collect();
        result["counts"]["inspected_files"] = json!(collection.inspected_len());
        result["counts"]["failed_files"] = json!(collection.failures.len());
        result["counts"]["duplicate_groups"] = json!(collection.duplicates.len());
        result["metadata_scope"] = json!(
            "Standard catalog fields; repeated values and absence remain explicit. Additional source tags and cover bytes are not expanded into catalog fields."
        );
        result["fingerprint_scope"] =
            json!("Encoded ALAC identity only; no decoded-PCM equivalence claim.");
        if !collection.mp3_tracks.is_empty() {
            result["fingerprint_scope"] = json!(
                "Typed encoded ALAC or complete MPEG-1 Layer III frame-byte identity; no decoded-PCM equivalence or MPEG insertion qualification."
            );
            result["mp3_duplicate_groups"] = json!(collection.mp3_duplicates());
            result["counts"]["duplicate_groups"] =
                json!(collection.duplicates.len() + collection.mp3_duplicates().len());
        }
        result["tracks"] = json!(tracks);
        result["failures"] = json!(failures);
        result["duplicates"] = json!(duplicates);
    }
    Ok(result)
}

fn human(report: &Value) -> Result<String> {
    use std::fmt::Write;
    let counts = &report["counts"];
    let quoted = |value: &Value| serde_json::to_string(value);
    let mut out = String::new();
    writeln!(
        out,
        "Source {}: {}",
        report["mode"].as_str().unwrap_or("scan"),
        quoted(&report["root"])?
    )?;
    writeln!(
        out,
        "Source-only report; no device comparison or sync plan. Sync compatibility is not guaranteed."
    )?;
    writeln!(
        out,
        "Selected {} files ({} bytes); excluded {} entries. Recursive={}.",
        counts["selected_files"],
        counts["selected_bytes"],
        counts["excluded_entries"],
        report["options"]["recursive"]
    )?;
    writeln!(
        out,
        "Include={}; exclude={}",
        quoted(&report["options"]["include"])?,
        quoted(&report["options"]["exclude"])?
    )?;
    if let Some(tracks) = report.get("tracks").and_then(Value::as_array) {
        for track in tracks {
            if track["codec"] == "mpeg1-layer3" {
                writeln!(
                    out,
                    "MPEG source {} title={} artist={} album={}; {} checked frames, {} embedded pictures; insertion unqualified.",
                    quoted(&track["relative_path"])?,
                    quoted(&track["metadata"]["title"])?,
                    quoted(&track["metadata"]["artist"])?,
                    quoted(&track["metadata"]["album"])?,
                    track["fingerprint"]["audio"]["frame_count"],
                    track["artwork"].as_array().map_or(0, Vec::len)
                )?;
                continue;
            }
            writeln!(
                out,
                "Source {} title={}",
                quoted(&track["relative_path"])?,
                quoted(&track["metadata"]["title"])?
            )?;
        }
    } else {
        for source in report["selected"]
            .as_array()
            .context("missing selected report")?
        {
            writeln!(out, "Selected {}", quoted(&source["relative_path"])?)?;
        }
    }
    for excluded in report["exclusions"]
        .as_array()
        .context("missing exclusions report")?
    {
        writeln!(
            out,
            "Excluded {}: {}{}",
            quoted(&excluded["relative_path"])?,
            excluded["reason"].as_str().unwrap_or("unknown"),
            if excluded["descendants_unscanned"] == true {
                " (directory descendants unscanned)"
            } else {
                ""
            }
        )?;
    }
    if let Some(failures) = report.get("failures").and_then(Value::as_array) {
        for failure in failures {
            writeln!(
                out,
                "Failed {}: {}",
                quoted(&failure["relative_path"])?,
                quoted(&failure["message"])?
            )?;
        }
    }
    if let Some(duplicates) = report.get("duplicates").and_then(Value::as_array) {
        for group in duplicates {
            writeln!(
                out,
                "Duplicate audio {}: identical_files={}, identical_standard_metadata={}; explicit selection required.",
                quoted(&group["relative_paths"])?,
                group["identical_complete_files"],
                group["identical_standard_metadata"]
            )?;
        }
    }
    if let Some(groups) = report.get("mp3_duplicate_groups").and_then(Value::as_array) {
        for group in groups {
            writeln!(
                out,
                "Duplicate MPEG audio {}: explicit selection required.",
                quoted(group)?
            )?;
        }
    }
    if report["success"] == false {
        writeln!(
            out,
            "Source scan requires attention: selection is empty, files failed inspection, or duplicate groups remain."
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    const AUDIO: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/silence-alac.m4a"
    ));

    #[test]
    fn inspection_report_preserves_duplicate_conflicts_and_human_escaping() {
        let path = std::env::temp_dir().join(format!(
            "source-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let strange = "quoted\"line\n.m4a";
        fs::write(path.join(strange), AUDIO).unwrap();
        fs::write(path.join("copy.m4a"), AUDIO).unwrap();
        let selection = source_collection::select(&path, &SelectionOptions::default()).unwrap();
        let collection = source_collection::inspect(&selection).unwrap();
        let value = report(&selection, Some(&collection)).unwrap();
        assert_eq!(value["success"], false);
        assert_eq!(value["counts"]["duplicate_groups"], 1);
        assert_eq!(value["scope"]["sync_plan"], false);
        assert_eq!(
            value["tracks"][0]["metadata"]["artist"],
            json!(["First artist"])
        );
        let text = human(&value).unwrap();
        assert!(text.contains("quoted\\\"line\\n.m4a"));
        assert!(!text.contains(strange));
        assert!(text.contains("Duplicate audio"));
        let discovery = report(&selection, None).unwrap();
        assert_eq!(discovery["success"], true);
        assert!(discovery.get("tracks").is_none());
        fs::remove_file(path.join(strange)).unwrap();
        fs::remove_file(path.join("copy.m4a")).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn mpeg_inspection_and_duplicate_human_reports_are_explicit() {
        let path = std::env::temp_dir().join(format!(
            "mpeg-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let bytes = [frame.clone(), frame].concat();
        fs::write(path.join("a.mp3"), &bytes).unwrap();
        let selection = source_collection::select(&path, &SelectionOptions::default()).unwrap();
        let collection = source_collection::inspect(&selection).unwrap();
        let value = report(&selection, Some(&collection)).unwrap();
        assert_eq!(value["counts"]["inspected_files"], 1);
        assert_eq!(value["tracks"][0]["codec"], "mpeg1-layer3");
        assert!(human(&value).unwrap().contains("insertion unqualified"));
        fs::write(path.join("b.mp3"), bytes).unwrap();
        let selection = source_collection::select(&path, &SelectionOptions::default()).unwrap();
        let collection = source_collection::inspect(&selection).unwrap();
        let value = report(&selection, Some(&collection)).unwrap();
        assert_eq!(value["success"], false);
        assert_eq!(value["counts"]["duplicate_groups"], 1);
        assert!(human(&value).unwrap().contains("Duplicate MPEG audio"));
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn empty_selection_is_not_reported_as_successful() {
        let path = std::env::temp_dir().join(format!(
            "source-scan-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("song.ogg"), b"ignored").unwrap();
        let selection = source_collection::select(&path, &SelectionOptions::default()).unwrap();
        let value = report(&selection, None).unwrap();
        assert_eq!(value["success"], false);
        assert_eq!(value["exclusions"][0]["reason"], "unsupported-extension");
        assert!(human(&value).unwrap().contains("selection is empty"));
        fs::remove_file(path.join("song.ogg")).unwrap();
        fs::remove_dir(path).unwrap();
    }
}
