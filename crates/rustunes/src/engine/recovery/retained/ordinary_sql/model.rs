use super::*;

pub(super) fn add(t: &mut BTreeMap<String, Table>, name: &str, row: Row) -> Result<()> {
    ensure!(
        t.get_mut(name)
            .context("table absent")?
            .rows
            .insert(row, 1)
            .is_none(),
        "new {name} row collides"
    );
    Ok(())
}
pub(super) fn map(
    a: &Snapshot,
    z: &Snapshot,
    name: &str,
    expected: &mut BTreeMap<String, Table>,
    new_maps: &mut Vec<Map>,
) -> Result<Map> {
    let t = a.table("sort_map")?;
    let after = z.table("sort_map")?;
    let r = unique(after, "name", Cell::Text(name.into()))?;
    let order = int(after, r, "name_order")?;
    let section = int(after, r, "name_section")?;
    let Cell::Blob(key) = after.at(r, "sort_key")? else {
        anyhow::bail!("collation key is not binary")
    };
    ensure!(
        order > 0 && (0..=27).contains(&section) && key.first() == Some(&0x30) && key.len() <= 4096,
        "collation output exceeds qualified structural bounds"
    );
    unique(after, "name_order", Cell::Integer(order))?;
    let result = Map {
        name: name.into(),
        order,
        section,
        sort_key: key.clone(),
    };
    if absent(t, "name", Cell::Text(name.into()))? {
        ensure!(
            absent(t, "name_order", Cell::Integer(order))?,
            "new collation order already occupied"
        );
        if !new_maps.iter().any(|m| m.name == name) {
            let mut row = vec![Cell::Integer(0); t.columns.len()];
            set(t, &mut row, "name", Cell::Text(name.into()))?;
            set(t, &mut row, "name_order", Cell::Integer(order))?;
            set(t, &mut row, "name_section", Cell::Integer(section))?;
            set(t, &mut row, "sort_key", Cell::Blob(key.clone()))?;
            ensure!(&row == r, "unapproved new collation cells");
            add(expected, "sort_map", row)?;
            new_maps.push(result.clone());
        }
    } else {
        ensure!(
            unique(t, "name", Cell::Text(name.into()))? == r,
            "existing collation changed"
        );
    }
    Ok(result)
}
pub(super) fn grouping(map: &Map) -> Result<Vec<u8>> {
    let end = map
        .sort_key
        .iter()
        .position(|b| *b == 1)
        .context("group collation separator absent")?;
    ensure!(end > 1, "empty grouping key");
    Ok(map.sort_key[1..end].to_vec())
}
pub(super) fn group_row(
    a: &Snapshot,
    g: &Group,
    id: i64,
    target: i64,
    album_artist: i64,
    display: &Map,
    sort: &Map,
) -> Result<Row> {
    let t = a.table(g.table)?;
    let mut r = vec![Cell::Integer(0); t.columns.len()];
    for (k, n) in [
        (g.key, id),
        ("sync_id", g.wire),
        ("representative_item_pid", target),
    ] {
        set(t, &mut r, k, Cell::Integer(n))?;
    }
    set(t, &mut r, g.table, Cell::Text(g.label.clone()))?;
    if g.table != "genre" {
        set(
            t,
            &mut r,
            &format!("sort_{}", g.table),
            Cell::Text(g.sort_label.clone()),
        )?;
    }
    set(t, &mut r, "grouping_key", Cell::Blob(grouping(display)?))?;
    if g.table != "genre" {
        set(t, &mut r, "app_data", Cell::Null)?;
    }
    match g.table {
        "album" => {
            for k in [
                "feed_url",
                "cloud_library_id",
                "editorial_notes",
                "cloud_channel_name",
            ] {
                set(t, &mut r, k, Cell::Text(String::new()))?;
            }
            for (k, n) in [
                ("album_artist_pid", album_artist),
                ("keep_local_status", 2),
                ("album_type", 1),
            ] {
                set(t, &mut r, k, Cell::Integer(n))?;
            }
        }
        "album_artist" => {
            set(
                t,
                &mut r,
                "cloud_universal_library_id",
                Cell::Text(String::new()),
            )?;
            for (k, n) in [
                ("sort_order", sort.order),
                ("name_order", display.order),
                ("sort_order_section", sort.section),
            ] {
                set(t, &mut r, k, Cell::Integer(n))?;
            }
        }
        "item_artist" => {
            set(t, &mut r, "series_name", Cell::Text(String::new()))?;
            set(t, &mut r, "sort_series_name", Cell::Null)?;
        }
        "genre" => {}
        _ => anyhow::bail!("unqualified group kind"),
    }
    Ok(r)
}
pub(super) fn revision_row(t: &Table, pid: i64, class: i64, revision: i64) -> Result<Row> {
    let mut r = vec![Cell::Integer(0); t.columns.len()];
    for (k, n) in [
        ("entity_pid", pid),
        ("class", class),
        ("revision", revision),
    ] {
        set(t, &mut r, k, Cell::Integer(n))?;
    }
    Ok(r)
}
pub(super) fn compare(
    p: &PreparedAttempt,
    w: &Wire,
    f: &BeforeFacts,
    a: &Snapshot,
    z: &Snapshot,
    window: Window,
    budget: &Budget,
) -> Result<Observed> {
    ensure!(
        a.schema == z.schema && a.version == z.version,
        "ordinary SQL schema/version differs"
    );
    let mut expected = a.tables.clone();
    // A later run's listening also changes statistics; the playback absorption
    // below covers it together with the write's own recent-counter resets.
    let recent_resets = if window.is_same_run() {
        reset_recent(a, z, &mut expected, budget)?
    } else {
        Vec::new()
    };
    let mut new_maps = Vec::new();
    let title = map(a, z, &w.title, &mut expected, &mut new_maps)?;
    let mut maps = Vec::new();
    let mut ids = Vec::new();
    for g in &f.groups {
        budget.check()?;
        let t = z.table(g.table)?;
        let r = if g.wire != 0 {
            unique(t, "sync_id", Cell::Integer(g.wire))?
        } else {
            unique(t, g.table, Cell::Text(g.label.clone()))?
        };
        let id = int(t, r, g.key)?;
        ensure!(
            id > 0 && id != f.slot && !ids.contains(&id),
            "aliased new group identity"
        );
        for (name, old) in &a.tables {
            budget.check()?;
            for (j, k) in old.columns.iter().enumerate() {
                if k.ends_with("_pid") || k == "genre_id" {
                    ensure!(
                        old.rows.keys().all(|r| r[j] != Cell::Integer(id)),
                        "new group identity referenced in {name}.{k}"
                    );
                }
            }
        }
        ids.push(id);
        maps.push((
            map(a, z, &g.label, &mut expected, &mut new_maps)?,
            map(a, z, &g.sort_label, &mut expected, &mut new_maps)?,
        ));
    }
    ensure!(
        ids.len() == 3 + usize::from(w.genre.is_some()),
        "group plan shape differs"
    );
    for (i, g) in f.groups.iter().enumerate() {
        let mut row = group_row(a, g, ids[i], f.slot, ids[1], &maps[i].0, &maps[i].1)?;
        if g.table == "album" {
            set(
                a.table("album")?,
                &mut row,
                "album_year",
                Cell::Integer(w.year),
            )?;
        }
        add(&mut expected, g.table, row)?;
    }
    if let Some(genre) = &w.genre {
        ensure!(f.groups[3].label == *genre, "ordinary genre plan differs");
    }
    let item = z.table("item")?;
    let ir = item.one("item_pid", f.slot)?;
    let stats = z.table("item_stats")?;
    let sr = stats.one("item_pid", f.slot)?;
    let dates = [
        real(stats.at(sr, "date_accessed")?)?,
        real(item.at(ir, "date_downloaded")?)?,
    ];
    ensure!(
        dates
            .iter()
            .all(|d| *d > 0.0 && *d < 3124137600.0 && *d + 978307200.0 >= w.timestamp)
            && (0.0..=0.001).contains(&(dates[1] - dates[0])),
        "receiver date pair/range differs"
    );
    let extra = z.table("item_extra")?;
    let xr = extra.one("item_pid", f.slot)?;
    let Cell::Blob(integrity) = extra.at(xr, "integrity")? else {
        anyhow::bail!("new ordinary ALAC integrity is not binary")
    };
    ensure!(
        integrity.len() == 57 && integrity[..2] == [4, 0] && integrity[2..].iter().any(|b| *b != 0),
        "new ordinary ALAC integrity shape differs"
    );
    let file = p
        .content
        .path
        .rsplit_once('/')
        .context("target basename absent")?
        .1;
    let mut target_rows = rows::new_rows(
        a,
        w,
        rows::NewValues {
            id: f.slot,
            groups: [ids[0], ids[1], ids[2]],
            base: f.base,
            file,
            track: p.content.track_pid,
            bytes: p.content.source.file_bytes,
            maps: [
                (title.order, title.section),
                (maps[0].1.order, maps[0].1.section),
                (maps[2].1.order, maps[2].1.section),
            ],
            dates,
            integrity,
        },
    )?;
    // Display/search and sort references are independent. Existing groups can
    // retain different canonical labels; none is replaced by source spelling.
    for (name, k, n) in [
        ("item", "album_artist_order", maps[1].1.order),
        ("item", "album_artist_order_section", maps[1].1.section),
        ("item_search", "search_album", maps[0].0.order),
        ("item_search", "search_artist", maps[2].0.order),
        ("item_search", "search_album_artist", maps[1].0.order),
    ] {
        set(
            a.table(name)?,
            target_rows.get_mut(name).context("target row absent")?,
            k,
            Cell::Integer(n),
        )?;
    }
    if w.genre.is_some() {
        for (name, key, n) in [
            ("item", "genre_id", ids[3]),
            ("item", "genre_order", maps[3].0.order),
            ("item", "genre_order_section", maps[3].0.section),
        ] {
            set(
                a.table(name)?,
                target_rows
                    .get_mut(name)
                    .context("genre target row absent")?,
                key,
                Cell::Integer(n),
            )?;
        }
    }
    set(
        a.table("item_extra")?,
        target_rows
            .get_mut("item_extra")
            .context("target extra row absent")?,
        "year",
        Cell::Integer(w.year),
    )?;
    if let Some(text) = &w.copyright {
        set(
            a.table("item_extra")?,
            target_rows
                .get_mut("item_extra")
                .context("target extra row absent")?,
            "copyright",
            Cell::Text(text.clone()),
        )?;
    }
    for (name, r) in target_rows {
        add(&mut expected, name, r)?;
    }
    let rt = a.table("entity_revision")?;
    let mut revision = f.maximum;
    // Observed local insertion order: new album artist; provisional new album;
    // new item artist; synchronization boundary; new item; final new album.
    revision += 1;
    add(
        &mut expected,
        "entity_revision",
        revision_row(rt, ids[1], f.groups[1].class, revision)?,
    )?;
    // The provisional new album's revision, which the final one replaces.
    revision += 1;
    revision += 1;
    add(
        &mut expected,
        "entity_revision",
        revision_row(rt, ids[2], f.groups[2].class, revision)?,
    )?;
    if w.genre.is_some() {
        revision += 1;
        add(
            &mut expected,
            "entity_revision",
            revision_row(rt, ids[3], f.groups[3].class, revision)?,
        )?;
    }
    revision += 1;
    let last_synced = revision;
    revision += 1;
    let target_revision = revision_row(rt, f.slot, 0, revision)?;
    if let Some(old) = &f.tombstone {
        replace(&mut expected, "entity_revision", old, target_revision)?;
    } else {
        add(&mut expected, "entity_revision", target_revision)?;
    }
    revision += 1;
    add(
        &mut expected,
        "entity_revision",
        revision_row(rt, ids[0], f.groups[0].class, revision)?,
    )?;
    let props = a.table("_MLDatabaseProperties")?;
    let afterprops = z.table("_MLDatabaseProperties")?;
    let generation = decimal(afterprops, "MLSyncClientGenerationID")?;
    ensure!(
        generation != 0 && generation != f.generation,
        "generation unchanged/zero"
    );
    let hero = revision;
    for (k, n) in [
        (
            "MLSyncClientSyncAnchor",
            i64::from(p.current_media_anchor) + 1,
        ),
        ("MLSyncClientLastSyncedRevision", last_synced),
        ("MLArtistHeroImageImportDatabaseRevision", hero),
        ("MLSyncClientGenerationID", generation),
    ] {
        let old = unique(props, "key", Cell::Text(k.into()))?;
        let mut r = old.clone();
        set(props, &mut r, "value", Cell::Text(n.to_string()))?;
        if &r != old {
            replace(&mut expected, "_MLDatabaseProperties", old, r)?;
        }
    }
    budget.check()?;
    // A refusal leaves `expected` unchanged, so the classification below still
    // sends a playback-tables-only difference to the immediate-After bridge.
    let playback = window
        .absorb(&mut expected, &z.tables, budget)
        .ok()
        .flatten();
    budget.check()?;
    let differences: Vec<_> = expected
        .iter()
        .filter_map(|(name, t)| (z.tables.get(name) != Some(t)).then_some(name.as_str()))
        .collect();
    if !differences.is_empty()
        && differences.iter().all(|name| {
            [
                "item_stats",
                "container",
                "container_item",
                "entity_revision",
            ]
            .contains(name)
        })
    {
        return Err(PlaybackEffectMismatch("statistics/history/revision").into());
    }
    ensure!(
        expected == z.tables,
        "unapproved source-derived ordinary ALAC rows/cells: {}",
        differences.join(",")
    );
    Ok(Observed {
        new_maps,
        dates,
        integrity: integrity.clone(),
        recent_resets,
        playback,
    })
}

fn reset_recent(
    a: &Snapshot,
    z: &Snapshot,
    expected: &mut BTreeMap<String, Table>,
    budget: &Budget,
) -> Result<Vec<serde_json::Value>> {
    let before = a.table("item_stats")?;
    let after = z.table("item_stats")?;
    let mut resets = Vec::new();
    for (old, count) in &before.rows {
        budget.check()?;
        ensure!(
            *count == 1,
            "ordinary prior statistics multiplicity differs"
        );
        let id = int(before, old, "item_pid")?;
        let new = after.one("item_pid", id)?;
        let mut row = old.clone();
        for key in ["play_count_recent", "skip_count_recent"] {
            let from = int(before, old, key)?;
            let to = int(after, new, key)?;
            ensure!(
                (0..=i64::from(u32::MAX)).contains(&from)
                    && (0..=i64::from(u32::MAX)).contains(&to),
                "ordinary recent counter exceeds native bounds"
            );
            if !(to == from || (from > 0 && to == 0)) {
                return Err(PlaybackEffectMismatch("recent statistics").into());
            }
            if to != from {
                set(before, &mut row, key, Cell::Integer(to))?;
                resets
                    .push(serde_json::json!({"item_pid":id,"column":key,"before":from,"after":to}));
            }
        }
        if row != *old {
            replace(expected, "item_stats", old, row)?;
        }
    }
    Ok(resets)
}
