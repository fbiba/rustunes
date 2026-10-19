//! Planner and conformance tests on the full native-schema fixture library of
//! `verification::metadata_edit_sql::tests` (five tracks, sync ids 1001..1005).
use super::*;
use crate::engine::verification::metadata_edit_sql::tests::{World, num, text, update};
use rusqlite::Connection;

fn before() -> Tables {
    World::new().before()
}
fn before_with(sql: &str) -> Tables {
    let world = World::new();
    Connection::open(&world.fixture.before)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    world.before()
}
fn request(track: u64) -> EditRequest {
    EditRequest {
        track,
        ..Default::default()
    }
}
fn planned(before: &Tables, requests: &[EditRequest], ids: &[u64]) -> Result<Vec<TrackUpdate>> {
    let mut queue = ids.iter().copied();
    plan(before, requests, &mut || {
        queue
            .next()
            .expect("the planner asked for more fresh ids than the test supplied")
    })
}
fn refusal(before: &Tables, requests: &[EditRequest], ids: &[u64]) -> String {
    format!(
        "{:#}",
        planned(before, requests, ids).expect_err("the plan should have been refused")
    )
}
fn pid(update: &TrackUpdate, key: &str) -> i64 {
    update.item[key].as_signed_integer().unwrap()
}

#[test]
fn title_sort_defaults_to_the_new_title_and_unchanged_fields_drop() {
    let b = before();
    let out = planned(
        &b,
        &[EditRequest {
            title: Some("New".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1001,
            &[("title", text("New")), ("sort_name", text("New"))]
        )]
    );
    let explicit = planned(
        &b,
        &[EditRequest {
            title: Some("The New".into()),
            sort_title: Some("New, The".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        explicit,
        [update(
            1001,
            &[("title", text("The New")), ("sort_name", text("New, The"))]
        )]
    );
    // Same title without a sort value keeps the current sort name: dropped.
    let same = planned(
        &b,
        &[EditRequest {
            title: Some("Track 1".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert!(same.is_empty());
    // A sort value alone is allowed for the title; the title travels with it.
    let sort_only = planned(
        &b,
        &[EditRequest {
            sort_title: Some("One, Track".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        sort_only,
        [update(
            1001,
            &[
                ("title", text("Track 1")),
                ("sort_name", text("One, Track"))
            ]
        )]
    );
}

#[test]
fn numbers_flags_and_copyright_send_only_what_differs() {
    let b = before();
    let out = planned(
        &b,
        &[EditRequest {
            year: Some(2002),      // current
            track_number: Some(3), // current
            track_count: Some(12), // 9
            disc_number: Some(2),  // 1
            disc_count: Some(1),   // current
            is_compilation: Some(true),
            content_rating: Some(2),
            copyright: Some("(C) New".into()),
            ..request(1003)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1003,
            &[
                ("track_count", num(12)),
                ("disc_number", num(2)),
                ("is_compilation", Value::Boolean(true)),
                ("content_rating", num(2)),
                ("copyright", text("(C) New")),
            ]
        )]
    );
    assert!(
        planned(
            &b,
            &[EditRequest {
                year: Some(2002),
                is_compilation: Some(false),
                ..request(1003)
            }],
            &[]
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn clears_send_zero_and_an_empty_copyright() {
    let b = before();
    let out = planned(
        &b,
        &[EditRequest {
            year: Some(0),
            track_number: Some(0),
            track_count: Some(0),
            disc_count: Some(0),
            copyright: Some(String::new()),
            ..request(1003)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1003,
            &[
                ("copyright", text("")),
                ("disc_count", num(0)),
                ("track_count", num(0)),
                ("track_number", num(0)),
                ("year", num(0)),
            ]
        )]
    );
}

#[test]
fn a_comment_is_sent_with_its_line_breaks_and_an_empty_one_only_to_clear() {
    let b = before();
    let comment = "line one\nline two\ttabbed";
    let out = planned(
        &b,
        &[EditRequest {
            comment: Some(comment.into()),
            ..request(1003)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(out, [update(1003, &[("comment", text(comment))])]);
    // The track has no comment, so clearing it changes nothing.
    assert!(
        planned(
            &b,
            &[EditRequest {
                comment: Some(String::new()),
                ..request(1003)
            }],
            &[]
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        planned(
            &b,
            &[EditRequest {
                comment: Some("a\u{7}b".into()),
                ..request(1003)
            }],
            &[]
        )
        .is_err()
    );
}

#[test]
fn requests_without_remaining_change_drop_and_order_is_kept() {
    let b = before();
    let out = planned(
        &b,
        &[
            EditRequest {
                year: Some(2010),
                ..request(1004)
            },
            request(1002),
            EditRequest {
                year: Some(2011),
                ..request(1003)
            },
        ],
        &[],
    )
    .unwrap();
    assert_eq!(
        out.iter().map(|u| u.sync_id).collect::<Vec<_>>(),
        [1004, 1003]
    );
    assert!(planned(&b, &[], &[]).unwrap().is_empty());
}

#[test]
fn invalid_requests_are_refused() {
    let b = before();
    let long = "x".repeat(4097);
    let cases: Vec<(&str, Vec<EditRequest>, &str)> = vec![
        (
            "empty string",
            vec![EditRequest {
                genre: Some(String::new()),
                ..request(1001)
            }],
            "empty strings are refused",
        ),
        (
            "empty sort",
            vec![EditRequest {
                title: Some("A".into()),
                sort_title: Some(String::new()),
                ..request(1001)
            }],
            "empty strings are refused",
        ),
        (
            "long text",
            vec![EditRequest {
                copyright: Some(long),
                ..request(1001)
            }],
            "4096",
        ),
        (
            "control character",
            vec![EditRequest {
                title: Some("a\u{7}b".into()),
                ..request(1001)
            }],
            "control",
        ),
        (
            "empty composer",
            vec![EditRequest {
                composer: Some(String::new()),
                ..request(1001)
            }],
            "empty strings are refused",
        ),
        (
            "year 10000",
            vec![EditRequest {
                year: Some(10000),
                ..request(1001)
            }],
            "year must be between 0 and 9999",
        ),
        (
            "content rating three",
            vec![EditRequest {
                content_rating: Some(3),
                ..request(1001)
            }],
            "content_rating must be between 0 and 2",
        ),
        (
            "track zero",
            vec![EditRequest {
                year: Some(2000),
                ..request(0)
            }],
            "track zero",
        ),
        (
            "unknown track",
            vec![EditRequest {
                year: Some(2000),
                ..request(4242)
            }],
            "not in the catalogue",
        ),
        (
            "two requests for one track",
            vec![
                EditRequest {
                    year: Some(2000),
                    ..request(1001)
                },
                EditRequest {
                    year: Some(2001),
                    ..request(1001)
                },
            ],
            "more than one request",
        ),
    ];
    for (name, requests, reason) in cases {
        let error = refusal(&b, &requests, &[]);
        assert!(error.contains(reason), "{name}: {error}");
    }
}

#[test]
fn genre_and_composer_follow_the_recipe() {
    let b = before();
    let out = planned(
        &b,
        &[
            EditRequest {
                genre: Some("Rock".into()),
                composer: Some("Fresh Composer".into()),
                ..request(1003)
            },
            // Already Pop: dropped.
            EditRequest {
                genre: Some("Pop".into()),
                ..request(1004)
            },
        ],
        &[],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1003,
            &[
                ("genre", text("Rock")),
                ("composer", text("Fresh Composer")),
                ("sort_composer", text("Fresh Composer")),
            ]
        )]
    );
    let sort_only = refusal(
        &b,
        &[EditRequest {
            sort_composer: Some("Comp A, Sort".into()),
            ..request(1002)
        }],
        &[],
    );
    assert!(sort_only.contains("sort value"), "{sort_only}");
    // A name that differs from a stored one only by case would be merged.
    let merged = refusal(
        &b,
        &[EditRequest {
            genre: Some("rock".into()),
            ..request(1003)
        }],
        &[],
    );
    assert!(merged.contains("case or punctuation"), "{merged}");
    let merged_composer = refusal(
        &b,
        &[EditRequest {
            composer: Some("comp a".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(
        merged_composer.contains("case or punctuation"),
        "{merged_composer}"
    );
    // The exact stored name is a plain join and needs its stored sort name.
    let join = planned(
        &b,
        &[EditRequest {
            composer: Some("Comp A".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(join[0].item["sort_composer"], text("Comp A"));
    let wrong_sort = refusal(
        &b,
        &[EditRequest {
            composer: Some("Comp A".into()),
            sort_composer: Some("A, Comp".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(wrong_sort.contains("sort name"), "{wrong_sort}");
}

#[test]
fn artist_moves_only_for_the_stored_representative() {
    let b = before();
    // Track 2 is the representative of artist 12.
    let out = planned(
        &b,
        &[EditRequest {
            artist: Some("Newcomer".into()),
            ..request(1002)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1002,
            &[
                ("artist", text("Newcomer")),
                ("sort_artist", text("Newcomer"))
            ]
        )]
    );
    assert!(!out[0].item.contains_key("artist_pid"));
    let explicit = planned(
        &b,
        &[EditRequest {
            artist: Some("Newcomer".into()),
            sort_artist: Some("Newcomer, A".into()),
            ..request(1002)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(explicit[0].item["sort_artist"], text("Newcomer, A"));
    // Track 5 shares artist 12 but is not its representative.
    let error = refusal(
        &b,
        &[EditRequest {
            artist: Some("Newcomer".into()),
            ..request(1005)
        }],
        &[],
    );
    assert!(error.contains("representative"), "{error}");
    // Moving into an existing group keeps its exact stored name and sort.
    let join = planned(
        &b,
        &[EditRequest {
            artist: Some("Survivor".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(join[0].item["artist"], text("Survivor"));
    let sort = refusal(
        &b,
        &[EditRequest {
            artist: Some("Survivor".into()),
            sort_artist: Some("Survivor, The".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(sort.contains("sort name"), "{sort}");
    let case = refusal(
        &b,
        &[EditRequest {
            artist: Some("survivor".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(case.contains("case or punctuation"), "{case}");
    let sort_only = refusal(
        &b,
        &[EditRequest {
            sort_artist: Some("Target, The".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(sort_only.contains("sort value"), "{sort_only}");
}

#[test]
fn album_moves_join_by_sync_id_or_draw_a_fresh_one() {
    let b = before();
    // Existing album "Second" (sync 112) of the same album artist.
    let join = planned(
        &b,
        &[EditRequest {
            album: Some("Second".into()),
            ..request(1001)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(
        join,
        [update(
            1001,
            &[
                ("album", text("Second")),
                ("sort_album", text("Second")),
                ("album_pid", num(112))
            ]
        )]
    );
    // "Other" exists under album artist 21 only, so track 1 gets a fresh album.
    let fresh = planned(
        &b,
        &[EditRequest {
            album: Some("Other".into()),
            ..request(1001)
        }],
        &[77],
    )
    .unwrap();
    assert_eq!(pid(&fresh[0], "album_pid"), 77);
    // An album sort name that differs from the stored one is not silently kept.
    let sort = refusal(
        &b,
        &[EditRequest {
            album: Some("Second".into()),
            sort_album: Some("Second, The".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(sort.contains("another album artist or name"), "{sort}");
    // Changing only the sort name is not a recipe operation.
    let sort_only = refusal(
        &b,
        &[EditRequest {
            sort_album: Some("Album, The".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(sort_only.contains("sort value"), "{sort_only}");
}

#[test]
fn tracks_moving_to_one_new_album_join_it_from_the_next_session() {
    let b = before();
    let both = [1001, 1005].map(|track| EditRequest {
        album: Some("Shared".into()),
        ..request(track)
    });
    // Track 5 names the album track 1 creates, which the phone would create
    // again under the same id; it waits for the next session.
    let error = refusal(&b, &both, &[7]);
    assert!(error.contains("created earlier in this session"), "{error}");
    assert_eq!(
        schedule(&b, &both, 8, &mut counter()).unwrap(),
        [vec![1001], vec![1005]]
    );
    let shared = join_in_turn(&b, &both);
    assert_eq!(pid(&shared[1], "album_pid"), pid(&shared[0], "album_pid"));
    // Track 3 belongs to another album artist: a second, distinct album.
    let split = planned(
        &b,
        &[
            EditRequest {
                album: Some("Shared".into()),
                ..request(1001)
            },
            EditRequest {
                album: Some("Shared".into()),
                ..request(1003)
            },
        ],
        &[7, 8],
    )
    .unwrap();
    assert_eq!(pid(&split[0], "album_pid"), 7);
    assert_eq!(pid(&split[1], "album_pid"), 8);
}

/// Each request planned against the catalogue the ones before it leave, one
/// session each, as `schedule` models them.
fn join_in_turn(before: &Tables, requests: &[EditRequest]) -> Vec<TrackUpdate> {
    let mut cat = Catalogue::read(before).unwrap();
    let mut fresh = counter();
    let mut updates = Vec::new();
    for request in requests {
        let planned = plan_on(&cat, std::slice::from_ref(request), &mut fresh).unwrap();
        cat.simulate(&conform_on(&cat, &planned).unwrap()).unwrap();
        updates.extend(planned);
    }
    updates
}

#[test]
fn representatives_moving_to_one_new_album_artist_join_it_from_the_next_session() {
    let b = before();
    // Tracks 1 and 3 represent their artists and album artists, as two
    // tracks of one album respelled to a new joint album artist did.
    let both = [1001, 1003].map(|track| EditRequest {
        artist: Some("Joint".into()),
        album_artist: Some("Joint".into()),
        album: Some("Shared".into()),
        ..request(track)
    });
    let error = refusal(&b, &both, &[100, 200]);
    assert!(error.contains("created earlier in this session"), "{error}");
    assert_eq!(
        schedule(&b, &both, 8, &mut counter()).unwrap(),
        [vec![1001], vec![1003]]
    );
    let joined = join_in_turn(&b, &both);
    for key in ["artist_pid", "album_pid"] {
        assert_eq!(pid(&joined[1], key), pid(&joined[0], key), "{key}");
    }
}

#[test]
fn fresh_ids_skip_zero_existing_ids_and_ids_used_in_the_plan() {
    let b = before();
    // 110 is an album, 120 an album artist, 1002 a track; 77 is used once.
    let out = planned(
        &b,
        &[
            EditRequest {
                album: Some("Shared".into()),
                ..request(1001)
            },
            EditRequest {
                album: Some("Shared".into()),
                ..request(1003)
            },
        ],
        &[0, 110, 120, 1002, 77, 77, 78],
    )
    .unwrap();
    assert_eq!(pid(&out[0], "album_pid"), 77);
    assert_eq!(pid(&out[1], "album_pid"), 78);
    let mut calls = 0;
    let error = plan(
        &b,
        &[EditRequest {
            album: Some("Shared".into()),
            ..request(1001)
        }],
        &mut || {
            calls += 1;
            110
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("no usable id"));
    assert!(calls > 100, "the planner keeps drawing before it gives up");
}

#[test]
fn year_with_an_album_move_is_allowed_only_for_a_new_album() {
    let b = before();
    let fresh = planned(
        &b,
        &[EditRequest {
            year: Some(1999),
            album: Some("Brand New".into()),
            ..request(1001)
        }],
        &[9],
    )
    .unwrap();
    assert_eq!(pid(&fresh[0], "album_pid"), 9);
    assert_eq!(fresh[0].item["year"], num(1999));
    let error = refusal(
        &b,
        &[EditRequest {
            year: Some(1999),
            album: Some("Second".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(error.contains("year"), "{error}");
}

#[test]
fn a_compilation_track_cannot_create_an_album_unless_it_stops_being_one() {
    let b = before_with("UPDATE item SET is_compilation=1 WHERE item_pid=1;");
    let error = refusal(
        &b,
        &[EditRequest {
            album: Some("Brand New".into()),
            ..request(1001)
        }],
        &[9],
    );
    assert!(error.contains("compilation"), "{error}");
    let out = planned(
        &b,
        &[EditRequest {
            album: Some("Brand New".into()),
            is_compilation: Some(false),
            ..request(1001)
        }],
        &[9],
    )
    .unwrap();
    assert_eq!(out[0].item["is_compilation"], Value::Boolean(false));
}

#[test]
fn album_artist_change_sends_the_full_set_with_album_before_artist_ids() {
    let b = before();
    // Track 1 is the representative of artist 11 and album artist 20.
    let out = planned(
        &b,
        &[EditRequest {
            album_artist: Some("Fresh Artist".into()),
            ..request(1001)
        }],
        &[100, 200],
    )
    .unwrap();
    assert_eq!(
        out,
        [update(
            1001,
            &[
                ("artist", text("Target")),
                ("sort_artist", text("Target")),
                ("album", text("Album")),
                ("sort_album", text("Album")),
                ("album_artist", text("Fresh Artist")),
                ("sort_album_artist", text("Fresh Artist")),
                ("album_pid", num(100)),
                ("artist_pid", num(200)),
            ]
        )]
    );
    // Track 3 (representative of artist 13 and album artist 21) joins the
    // existing album artist 20 and its existing album "Second" (112): no ids.
    let join = planned(
        &b,
        &[EditRequest {
            album_artist: Some("Artist".into()),
            album: Some("Second".into()),
            ..request(1003)
        }],
        &[],
    )
    .unwrap();
    assert_eq!(pid(&join[0], "album_pid"), 112);
    assert_eq!(pid(&join[0], "artist_pid"), 120);
    assert_eq!(join[0].item["artist"], text("Other"));
    // An existing album artist without an album of that name: one fresh id.
    let partial = planned(
        &b,
        &[EditRequest {
            album_artist: Some("Artist".into()),
            ..request(1003)
        }],
        &[300],
    )
    .unwrap();
    assert_eq!(pid(&partial[0], "album_pid"), 300);
    assert_eq!(pid(&partial[0], "artist_pid"), 120);
    // Artist and album can change in the same request.
    let both = planned(
        &b,
        &[EditRequest {
            album_artist: Some("Fresh Artist".into()),
            artist: Some("Target Two".into()),
            album: Some("Fresh Album".into()),
            ..request(1001)
        }],
        &[100, 200],
    )
    .unwrap();
    assert_eq!(both[0].item["artist"], text("Target Two"));
    assert_eq!(both[0].item["album"], text("Fresh Album"));
}

#[test]
fn album_artist_change_needs_the_representative_of_both_groups() {
    let b = before();
    // Track 2 represents its artist but not album artist 20 (track 1 does).
    let error = refusal(
        &b,
        &[EditRequest {
            album_artist: Some("Fresh Artist".into()),
            ..request(1002)
        }],
        &[100, 200],
    );
    assert!(error.contains("album artist group"), "{error}");
    // Track 4 is a plain member of artist 13 and album artist 21.
    let error = refusal(
        &b,
        &[EditRequest {
            album_artist: Some("Fresh Artist".into()),
            ..request(1004)
        }],
        &[100, 200],
    );
    assert!(error.contains("representative"), "{error}");
    let sort_only = refusal(
        &b,
        &[EditRequest {
            sort_album_artist: Some("Artist, The".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(sort_only.contains("sort value"), "{sort_only}");
}

#[test]
fn planner_output_is_conformant_and_hand_edited_variants_are_not() {
    let b = before();
    let out = planned(
        &b,
        &[
            EditRequest {
                album_artist: Some("Fresh Artist".into()),
                ..request(1001)
            },
            EditRequest {
                album: Some("Second".into()),
                genre: Some("Pop".into()),
                ..request(1005)
            },
        ],
        &[100, 200],
    )
    .unwrap();
    check_conformant(&b, &out).unwrap();
    let edits = conform(&b, &out).unwrap();
    assert_eq!(edits.len(), 2);
    assert!(edits[0].album_artist.is_some());
    // Dropping any of the full set's keys breaks conformance.
    for key in [
        "artist",
        "sort_artist",
        "album",
        "sort_album",
        "album_pid",
        "artist_pid",
        "sort_album_artist",
    ] {
        let mut broken = out.clone();
        broken[0].item.remove(key);
        assert!(check_conformant(&b, &broken).is_err(), "without {key}");
    }
    // An unused album id that is really a track id is refused.
    let mut clash = out.clone();
    clash[0].item.insert("album_pid".into(), num(1002));
    assert!(check_conformant(&b, &clash).is_err());
    // A second update may follow one that creates an album, but not join it.
    let mut joined = out.clone();
    joined.push(update(1003, &[("track_number", num(5))]));
    check_conformant(&b, &joined).unwrap();
    let again = vec![
        update(
            1001,
            &[
                ("album", text("Fresh")),
                ("sort_album", text("Fresh")),
                ("album_pid", num(500)),
            ],
        ),
        update(
            1005,
            &[
                ("album", text("Fresh")),
                ("sort_album", text("Fresh")),
                ("album_pid", num(500)),
            ],
        ),
    ];
    let error = format!("{:#}", check_conformant(&b, &again).unwrap_err());
    assert!(error.contains("created earlier in this session"), "{error}");
    let different = vec![
        again[0].clone(),
        update(
            1005,
            &[
                ("album", text("Fresh Too")),
                ("sort_album", text("Fresh Too")),
                ("album_pid", num(500)),
            ],
        ),
    ];
    assert!(check_conformant(&b, &different).is_err());
    // Track 3 has another album artist, so it cannot join that album.
    let foreign = vec![
        again[0].clone(),
        update(
            1003,
            &[
                ("album", text("Fresh")),
                ("sort_album", text("Fresh")),
                ("album_pid", num(500)),
            ],
        ),
    ];
    assert!(check_conformant(&b, &foreign).is_err());
}

#[test]
fn grouping_key_is_the_sort_key_between_the_section_byte_and_the_separator() {
    assert_eq!(
        grouping_key(&[0x30, 0x41, 0x42, 0x01, 0x07]),
        Some(&[0x41, 0x42][..])
    );
    assert_eq!(grouping_key(&[0x31, 0x41, 0x42]), Some(&[0x41, 0x42][..]));
    assert_eq!(grouping_key(&[0x30, 0x01, 0x07]), None);
    assert_eq!(grouping_key(&[0x30]), None);
    assert_eq!(grouping_key(&[]), None);
}

#[test]
fn names_keyed_apart_by_punctuation_or_spacing_do_not_merge() {
    // Pairs shaped like ones the phone keyed apart.
    assert!(!may_merge("Headliner/ Guest", "Headliner, Guest"));
    assert!(!may_merge("Northside", "NORTH SIDE"));
    assert!(!may_merge("Duo One & Two", "Duo One, Two"));
    assert!(!may_merge("Example", "Exemple"));
    assert!(may_merge(" The  Example Band", "the example band"));
    // Beyond printable ASCII only letters and digits count.
    assert!(may_merge("Ελλάδα, Β", "ελλάδα/ β"));
    assert!(may_merge("A\u{ff0c} B", "A, B"));
    // An artist and album artist joined with "/ " are respelled with ", ".
    let b = before_with(
        "UPDATE item_artist SET item_artist='Target/ Guest',sort_item_artist='Target/ Guest' WHERE item_artist_pid=11;
         UPDATE album_artist SET album_artist='Target/ Guest',sort_album_artist='Target/ Guest' WHERE album_artist_pid=20;",
    );
    let respelled = EditRequest {
        artist: Some("Target, Guest".into()),
        album_artist: Some("Target, Guest".into()),
        ..request(1001)
    };
    let out = planned(&b, std::slice::from_ref(&respelled), &[100, 200]).unwrap();
    assert_eq!(out[0].item["artist"], text("Target, Guest"));
    assert_eq!(out[0].item["album_artist"], text("Target, Guest"));
    assert_eq!(
        schedule(&b, &[respelled], 8, &mut counter()).unwrap(),
        [vec![1001]]
    );
    // Differing from the stored name only in case and spacing still merges.
    let merged = refusal(
        &b,
        &[EditRequest {
            artist: Some("target/  guest".into()),
            ..request(1001)
        }],
        &[],
    );
    assert!(merged.contains("case or punctuation"), "{merged}");
}

fn counter() -> impl FnMut() -> u64 {
    let mut next = 900;
    move || {
        next += 1;
        next
    }
}

#[test]
fn a_whole_artist_group_moves_one_representative_per_session() {
    let b = before();
    // Tracks 3 and 4 share artist 13 (representative 3). Each session moves
    // the then-current representative; the lowest remaining member follows.
    let requests = [1003, 1004].map(|track| EditRequest {
        artist: Some("Newcomer".into()),
        ..request(track)
    });
    let sessions = schedule(&b, &requests, 8, &mut counter()).unwrap();
    assert_eq!(sessions, [vec![1003], vec![1004]]);
    // In the other order the same two sessions are needed.
    let reversed = [requests[1].clone(), requests[0].clone()];
    assert_eq!(
        schedule(&b, &reversed, 8, &mut counter()).unwrap(),
        [vec![1003], vec![1004]]
    );
    // Fields without a group rule travel together in the first session.
    let mixed = [
        requests[0].clone(),
        EditRequest {
            title: Some("Renamed".into()),
            ..requests[1].clone()
        },
    ];
    assert_eq!(
        schedule(&b, &mixed, 8, &mut counter()).unwrap(),
        [vec![1003, 1004], vec![1004]]
    );
    assert!(
        matches!(schedule(&b, &requests, 1, &mut counter()), Err(e) if format!("{e:#}").contains("more than 1"))
    );
}

#[test]
fn a_member_that_never_becomes_its_groups_representative_is_refused() {
    let b = before();
    // Track 5 shares artist 12 with its representative 2, which stays.
    let error = format!(
        "{:#}",
        schedule(
            &b,
            &[EditRequest {
                artist: Some("Newcomer".into()),
                ..request(1005)
            }],
            8,
            &mut counter()
        )
        .unwrap_err()
    );
    assert!(
        error.contains("0000000000003ed") && error.contains("representative"),
        "{error}"
    );
    // Moving the representative too lets it follow in a second session.
    let both = [1002, 1005].map(|track| EditRequest {
        artist: Some("Newcomer".into()),
        ..request(track)
    });
    assert_eq!(
        schedule(&b, &both, 8, &mut counter()).unwrap(),
        [vec![1002], vec![1005]]
    );
}

#[test]
fn a_year_waits_for_the_session_after_its_album_join() {
    let b = before();
    let requests = [EditRequest {
        album: Some("Second".into()),
        year: Some(1999),
        ..request(1001)
    }];
    let (kept, waiting) = pack(&Catalogue::read(&b).unwrap(), &requests, &mut counter());
    assert!(waiting.is_empty());
    assert_eq!(
        (kept[0].album.as_deref(), kept[0].year),
        (Some("Second"), None)
    );
    assert_eq!(
        schedule(&b, &requests, 8, &mut counter()).unwrap(),
        [vec![1001], vec![1001]]
    );
}

#[test]
fn group_names_take_the_phones_spelling_when_only_case_differs() {
    let b = before();
    let cat = Catalogue::read(&b).unwrap();
    let mut edit = EditRequest {
        artist: Some("survivor".into()),
        genre: Some("POP".into()),
        album: Some("SECOND".into()),
        ..request(1001)
    };
    cat.respell(&mut edit).unwrap();
    assert_eq!(edit.artist.as_deref(), Some("Survivor"));
    assert_eq!(edit.genre.as_deref(), Some("Pop"));
    // "Second" belongs to the track's album artist 20.
    assert_eq!(edit.album.as_deref(), Some("Second"));
    assert_eq!(
        schedule(&b, &[edit], 8, &mut counter()).unwrap(),
        [vec![1001]]
    );
    // A new name stays as sent.
    let mut new = EditRequest {
        artist: Some("Brand New".into()),
        ..request(1001)
    };
    cat.respell(&mut new).unwrap();
    assert_eq!(new.artist.as_deref(), Some("Brand New"));
}

#[test]
fn nothing_left_to_send_schedules_no_session() {
    let b = before();
    let requests = [EditRequest {
        title: Some("Track 1".into()),
        ..request(1001)
    }];
    assert!(
        schedule(&b, &requests, 8, &mut counter())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_session_carries_at_most_its_share_of_updates() {
    let b = before();
    let requests = [1001, 1002, 1003].map(|track| EditRequest {
        title: Some(format!("New {track}")),
        ..request(track)
    });
    let (kept, waiting) = pack_within(&Catalogue::read(&b).unwrap(), &requests, 2, &mut counter());
    assert_eq!(
        kept.iter().map(|r| r.track).collect::<Vec<_>>(),
        [1001, 1002]
    );
    assert!(waiting.is_empty());
    // What a full session leaves out goes in the next.
    assert_eq!(
        schedule_within(&b, &requests, 8, 2, &mut counter()).unwrap(),
        [vec![1001, 1002], vec![1003]]
    );
    // A request too big for any session is refused with its reason, not
    // dropped.
    let error = format!(
        "{:#}",
        schedule_within(&b, &requests[..1], 8, 0, &mut counter()).unwrap_err()
    );
    assert!(
        error.contains("000003e9: it needs 1 track updates, more than the 0 one session holds"),
        "{error}"
    );
    assert_eq!(SESSION_UPDATES, 499);
}
