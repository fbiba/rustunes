//! Output pins for the MPEG insert preparation. Saved plans store the
//! assessment, and the journal and recovery rebuild the insert operation byte
//! for byte, so every fixture here is prepared the way a plan prepares a
//! source and each output is pinned by a test of its own, named for the
//! output and the fixture (`pins::<output>::<fixture>`):
//!
//! - `assessment`: the serde JSON of `assess` (metadata, coded timing and
//!   gapless proposal, artwork counts, issues); `issues`: the issue codes and
//!   exact details alone;
//! - `operation`: the SHA-256 of `operation_with_metadata` against
//!   `collection_catalog(13)` and `ids()`; `operation_joined`: against a
//!   catalogue that holds the groups and composer with sort names of their
//!   own; `operation_unsorted_composer`: against one whose composer has no
//!   sort name;
//! - `fields`: the serde JSON of `coded_insert_fields`; `expected`: that of
//!   `expected` (its tags and display time); `projected`: that of `projected`.
//!
//! Some fixtures share an operation hash: the tags they differ in are not
//! projected, or no wire field carries the difference (the assessment,
//! `fields` and `projected` pins tell them apart). A SHA-256 pin prints the
//! output it hashed when it fails.
use super::*;
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::mpeg::read;
use music_protocol::test_support::catalogue::{collection_catalog, modified_collection_catalog};
use sha2::{Digest, Sha256};
use std::io::Cursor;

/// What one output of one fixture must equal.
#[derive(Clone, Copy)]
enum Pin {
    /// The SHA-256 (hex) of the output.
    Hash(&'static str),
    /// The output itself.
    Json(&'static str),
    /// The step refuses this fixture.
    Refused,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// An output as the failure message shows it: text as it is, the binary
/// operation as its decoded property list.
fn shown(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => format!("{:?}", plist::Value::from_reader(Cursor::new(bytes))),
    }
}

fn check<T: AsRef<[u8]>>(result: Result<T>, pin: Pin) {
    match (pin, result) {
        (Pin::Refused, result) => {
            let accepted = result.ok().map(|output| shown(output.as_ref()));
            assert!(accepted.is_none(), "now accepted: {}", accepted.unwrap());
        }
        (Pin::Hash(pinned), Ok(output)) => {
            let output = output.as_ref();
            assert_eq!(sha256(output), pinned, "moved: {}", shown(output));
        }
        (Pin::Json(pinned), Ok(output)) => assert_eq!(shown(output.as_ref()), pinned),
        (_, Err(error)) => panic!("now refused: {error:#}"),
    }
}

/// A source file, kept as long as its directory.
struct Fixture {
    _temp: Option<Temp>,
    path: PathBuf,
    source: Source,
}
impl Fixture {
    fn tagged(tags: &[(&str, &str)], audio: Vec<u8>) -> Self {
        let temp = Temp::new();
        let (path, source) = temp.source(tags, audio);
        Self {
            _temp: Some(temp),
            path,
            source,
        }
    }
    /// An ID3v2.3 tag of text frames and one front or other cover.
    fn with_picture(tags: &[(&str, &str)], picture_type: u8) -> Self {
        let temp = Temp::new();
        let mut frames = Vec::new();
        for (id, value) in tags {
            frames.extend_from_slice(id.as_bytes());
            frames.extend_from_slice(&((value.len() + 1) as u32).to_be_bytes());
            frames.extend_from_slice(&[0, 0, 0]);
            frames.extend_from_slice(value.as_bytes());
        }
        // One 1 x 1 gray JPEG: frame, scan and end markers.
        let jpeg = [
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0,
            0, 0x3f, 0, 0x12, 0x34, 0xff, 0xd9,
        ];
        let picture = [
            b"\x00image/jpeg\x00".as_slice(),
            &[picture_type],
            b"\x00",
            &jpeg,
        ]
        .concat();
        frames.extend_from_slice(b"APIC");
        frames.extend_from_slice(&(picture.len() as u32).to_be_bytes());
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(&picture);
        let n = frames.len();
        let size = [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ];
        let path = temp.0.join("source.mp3");
        fs::write(
            &path,
            [b"ID3\x03\0\0".as_slice(), &size, &frames, &audio()].concat(),
        )
        .unwrap();
        let source = read(&path).unwrap();
        Self {
            _temp: Some(temp),
            path,
            source,
        }
    }
    /// A file of `tests/fixtures`.
    fn committed(name: &str) -> Self {
        let path = crate::test_fixtures::workspace_root()
            .join("tests/fixtures")
            .join(name);
        let source = read(&path).unwrap();
        Self {
            _temp: None,
            path,
            source,
        }
    }
    /// An inspection report edited after the read: it can be assessed but
    /// no longer prepared.
    fn forged(mut self, edit: impl FnOnce(&mut Source)) -> Self {
        edit(&mut self.source);
        self
    }
    fn prepared(&self) -> PreparedSource {
        PreparedSource::read_expected(&self.path, &self.source).unwrap()
    }
}

fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut values = tags();
    values.extend_from_slice(extra);
    values
}

/// Every rich scalar tag, a written comment and every sort name.
fn rich(composer: &'static str) -> Vec<(&'static str, &'static str)> {
    with(&[
        ("TPE2", "Ensemble"),
        ("TCMP", "1"),
        ("TCON", "Jazz"),
        ("TCOM", composer),
        ("TCOP", "Copyright Owner"),
        ("TYER", "2026"),
        ("TRCK", "2/3"),
        ("TPOS", "1/2"),
        ("COMM", "eng\0a comment\non two lines"),
        ("TSOT", "Song, Independent"),
        ("TSOP", "Artist, Source"),
        ("TSOA", "Album, Source"),
        ("TSO2", "Ensemble, The"),
        ("TSOC", "Composer, A"),
    ])
}

mod fixtures {
    use super::*;

    pub(super) fn cbr_mp3() -> Fixture {
        Fixture::tagged(&tags(), audio())
    }
    pub(super) fn info_header_mp3() -> Fixture {
        let mut frames = audio();
        frames[36..40].copy_from_slice(b"Info");
        Fixture::tagged(&tags(), frames)
    }
    pub(super) fn xing_header_mp3() -> Fixture {
        let mut frames = audio();
        frames[36..40].copy_from_slice(b"Xing");
        Fixture::tagged(&tags(), frames)
    }
    pub(super) fn vbri_header_mp3() -> Fixture {
        let mut frames = audio();
        let bytes = frames.len() as u32;
        frames[36..40].copy_from_slice(b"VBRI");
        frames[40..42].copy_from_slice(&1u16.to_be_bytes());
        frames[46..50].copy_from_slice(&bytes.to_be_bytes());
        frames[50..54].copy_from_slice(&2u32.to_be_bytes());
        frames[54..56].copy_from_slice(&1u16.to_be_bytes());
        frames[56..58].copy_from_slice(&1u16.to_be_bytes());
        frames[58..60].copy_from_slice(&2u16.to_be_bytes());
        frames[60..62].copy_from_slice(&2u16.to_be_bytes());
        frames[62..64].copy_from_slice(&(bytes as u16).to_be_bytes());
        Fixture::tagged(&tags(), frames)
    }
    /// No seek header; a second frame at a higher bit rate.
    pub(super) fn vbr_without_header_mp3() -> Fixture {
        let mut frames = audio();
        frames.truncate(417);
        let mut higher = vec![0; 522];
        higher[..4].copy_from_slice(&[0xff, 0xfb, 0xa0, 0]);
        frames.extend(higher);
        Fixture::tagged(&tags(), frames)
    }
    pub(super) fn id3v1_trailer_mp3() -> Fixture {
        let mut frames = audio();
        let mut trailer = vec![0; 128];
        trailer[..3].copy_from_slice(b"TAG");
        frames.extend(trailer);
        Fixture::tagged(&tags(), frames)
    }
    pub(super) fn comment_empty_mp3() -> Fixture {
        Fixture::tagged(&with(&[("COMM", "eng\0")]), audio())
    }
    pub(super) fn comment_written_mp3() -> Fixture {
        Fixture::tagged(&with(&[("COMM", "eng\0a comment\non two lines")]), audio())
    }
    /// An iTunNORM comment is the file's, not the catalogue's.
    pub(super) fn comment_described_mp3() -> Fixture {
        Fixture::tagged(&with(&[("COMM", "engiTunNORM\0 00000A1B")]), audio())
    }
    pub(super) fn unmapped_frame_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TIT3", "a subtitle")]), audio())
    }
    /// Every rich tag, with a composer the catalogue does not hold.
    pub(super) fn rich_mp3() -> Fixture {
        Fixture::tagged(&rich("New Composer"), audio())
    }
    /// The catalogue's composer, with its own sort name.
    pub(super) fn rich_mp3_existing_composer() -> Fixture {
        Fixture::tagged(&rich("Synthetic Composer"), audio())
    }
    /// The catalogue spells the composer another way; it wins.
    pub(super) fn rich_mp3_existing_composer_other_case() -> Fixture {
        Fixture::tagged(&rich("SYNTHETIC composer"), audio())
    }
    /// Artist, album, album artist and composer the catalogue holds, each
    /// with a sort name of its own (`joined_catalog`).
    pub(super) fn rich_mp3_joined_groups() -> Fixture {
        let mut values = rich("Synthetic Composer");
        values
            .retain(|(id, _)| !matches!(*id, "TPE1" | "TALB" | "TPE2" | "TSOP" | "TSOA" | "TSO2"));
        values.extend([
            ("TPE1", "SYNTHETIC ARTIST"),
            ("TALB", "Synthetic Album"),
            ("TPE2", "Synthetic Artist"),
            ("TSOA", "Ignored"),
        ]);
        Fixture::tagged(&values, audio())
    }
    /// No sort tags: sort names drop a leading article.
    pub(super) fn default_sorts_mp3() -> Fixture {
        Fixture::tagged(
            &[
                ("TIT2", "The Title"),
                ("TPE1", "The Artist"),
                ("TALB", "An Album"),
                ("TPE2", "A Band"),
                ("TCOM", "The Composer"),
            ],
            audio(),
        )
    }
    pub(super) fn cover_mp3() -> Fixture {
        Fixture::with_picture(&tags(), 3)
    }
    /// A lone back cover: the file's only picture, so sent as its cover.
    pub(super) fn back_cover_mp3() -> Fixture {
        Fixture::with_picture(&tags(), 4)
    }
    /// A compilation without an album artist: assessed, not insertable.
    pub(super) fn compilation_without_album_artist_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TCMP", "1")]), audio())
    }

    // Projections the assessment refuses.
    pub(super) fn missing_title_mp3() -> Fixture {
        Fixture::tagged(&[("TPE1", "Artist"), ("TALB", "Album")], audio())
    }
    pub(super) fn second_title_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TIT2", "second title")]), audio())
    }
    /// A second, empty artist is no artist: the source projects as the plain one.
    pub(super) fn second_artist_empty_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TPE1", "")]), audio())
    }
    pub(super) fn empty_album_mp3() -> Fixture {
        Fixture::tagged(
            &[("TIT2", "Title"), ("TPE1", "Artist"), ("TALB", "")],
            audio(),
        )
    }
    pub(super) fn bad_release_date_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TDRC", "24-05-01")]), audio())
    }
    pub(super) fn zero_release_year_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TDRC", "0000")]), audio())
    }
    pub(super) fn numeric_genre_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TCON", "(17)")]), audio())
    }
    pub(super) fn zero_track_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TRCK", "0/2")]), audio())
    }
    pub(super) fn disc_exceeds_total_mp3() -> Fixture {
        Fixture::tagged(&with(&[("TPOS", "3/2")]), audio())
    }

    // Formats of `tests/fixtures`.
    pub(super) fn aac_m4a() -> Fixture {
        Fixture::committed("tone-aac.m4a")
    }
    pub(super) fn flac() -> Fixture {
        Fixture::committed("tone-flac.flac")
    }
    pub(super) fn wav() -> Fixture {
        Fixture::committed("tone-wav.wav")
    }
    pub(super) fn aiff() -> Fixture {
        Fixture::committed("tone-aiff.aif")
    }
    pub(super) fn mpeg2_mp3() -> Fixture {
        Fixture::committed("tone-mpeg2.mp3")
    }
    pub(super) fn vbr_mpeg1_mp3() -> Fixture {
        Fixture::committed("tone-mp3-vbr.mp3")
    }

    // Assessed only: no preparation reads them.
    pub(super) fn oversize_mp3() -> Fixture {
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let frames = (music_protocol::protocol::transfer::MAX_AUDIO / 417 + 1) as usize;
        Fixture::tagged(&tags(), frame.repeat(frames))
    }
    pub(super) fn forged_without_apple_timing_mp3() -> Fixture {
        cbr_mp3().forged(|source| source.apple_timing = None)
    }
    pub(super) fn forged_wav_at_12_bits() -> Fixture {
        wav().forged(|source| source.stream.as_mut().unwrap().bits_per_sample = 12)
    }
    pub(super) fn forged_flac_disagreeing_with_its_identity() -> Fixture {
        flac().forged(|source| source.stream.as_mut().unwrap().encoded.sample_rate += 1)
    }
}

/// The groups and album artist `rich_mp3_joined_groups` joins, each with
/// the sort name the phone keeps.
fn joined_catalog() -> Catalog {
    modified_collection_catalog(
        13,
        "UPDATE album SET sort_album='Album, The' WHERE album_pid=31;
        UPDATE album_artist SET sort_album_artist='Artist, The' WHERE album_artist_pid=21;
        UPDATE item_artist SET sort_item_artist='Track Artist, The' WHERE item_artist_pid=41;
        UPDATE composer SET sort_composer='Composer, The' WHERE composer_pid=71;",
    )
}
fn joined_ids() -> Ids {
    Ids {
        track: u64::MAX - 1,
        album: (-31i64) as u64,
        artist: (-21i64) as u64,
    }
}

// Outputs.

fn assessment(fixture: &Fixture) -> Result<String> {
    Ok(serde_json::to_string(&assess(&fixture.source))?)
}
fn issues(fixture: &Fixture) -> Result<String> {
    Ok(serde_json::to_string(
        &assess(&fixture.source).compatibility_issues,
    )?)
}
fn operation(fixture: &Fixture) -> Result<Vec<u8>> {
    fixture
        .prepared()
        .operation_with_metadata(envelope(), ids(), &collection_catalog(13))
}
fn operation_joined(fixture: &Fixture) -> Result<Vec<u8>> {
    fixture
        .prepared()
        .operation_with_metadata(envelope(), joined_ids(), &joined_catalog())
}
fn operation_unsorted_composer(fixture: &Fixture) -> Result<Vec<u8>> {
    let catalog = modified_collection_catalog(13, "UPDATE composer SET sort_composer=NULL;");
    fixture
        .prepared()
        .operation_with_metadata(envelope(), ids(), &catalog)
}
fn fields(fixture: &Fixture) -> Result<String> {
    let prepared = fixture.prepared();
    let (metadata, timing) = prepared.coded_insert_fields()?;
    Ok(serde_json::to_string(&(metadata, timing))?)
}
fn expected(fixture: &Fixture) -> Result<String> {
    let prepared = fixture.prepared();
    let expected = prepared.expected()?;
    Ok(serde_json::to_string(&(
        expected.projected()?,
        expected.total_time_ms()?,
    ))?)
}
fn projected(fixture: &Fixture) -> Result<String> {
    Ok(serde_json::to_string(&fixture.prepared().projected()?)?)
}

/// One test per fixture, in a module named for the output.
macro_rules! pins {
    ($module:ident, $output:path, { $($fixture:ident => $pin:expr,)* }) => {
        mod $module {
            use super::*;
            $(
                #[test]
                fn $fixture() {
                    check($output(&fixtures::$fixture()), $pin);
                }
            )*
        }
    };
}

pins!(assessment, assessment, {
    cbr_mp3 => Pin::Json(r#"{"schema":"rustunes.mpeg-source-preparation","source_file_sha256":"8d44fa06b92ffd903d90214aaa7eb40407418e5413c0c43fe88c3bbe47e755f6","encoded_audio_sha256":"0e9ea8a196febd7d8097c3eff0ef4f4bce3f681ebadb0851b390d8117494adba","metadata":{"title":"Independent Song","artist":"Source Artist","album":"Source Album","album_artist":null,"genre":null,"composer":null,"copyright":null,"year":null,"track":null,"track_total":null,"disc":null,"disc_total":null,"compilation":null},"coded_timing_proposal":{"basis":"apple_counted_frames_and_played_samples","sample_rate":44100,"channels":2,"bit_rate_kbps":128,"coded_samples":1176,"total_time_ms_floor":52,"native_mapping_qualified":false,"gapless":{"delay":528,"drain":600,"heuristic":33554435,"resynch":0}},"artwork":{"embedded_picture_count":0,"front_cover_count":0,"embedded_bytes_preserved":true,"native_artwork_action_qualified":false},"compatibility_issues":[{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}],"required_qualification":["mpeg_positive_timing_receiver","mpeg_source_metadata_and_collection_mapping","mpeg_public_insertion_completion_and_recovery"],"execution_authorized":false}"#),
    info_header_mp3 => Pin::Hash("4fe85b625f0ca4449e6ea5ffc65d50c09b15227424980c4f2d4831eff584fddf"),
    xing_header_mp3 => Pin::Hash("2ec37eec3a6578009c42c8cc6b8e1f7dd2f38dd09f5b1fad8e813351a387e25c"),
    vbri_header_mp3 => Pin::Hash("ede5123186ed77e2a4e16e55859cc69d588a393d0e65cb67eda7d7aa8622b5f5"),
    vbr_without_header_mp3 => Pin::Hash("eb7a0935e929e0525e05ac12da2d5293a89669eab646aed106333213b9cca50b"),
    id3v1_trailer_mp3 => Pin::Hash("affe7f09535f09d5500d56c3453e01d9420cad1ae525ca9afa75c29415eeebc3"),
    comment_empty_mp3 => Pin::Hash("dc5875723117cafc3a440fa145ce20f01b13f58cb6bacd5785436e18bde6677b"),
    comment_written_mp3 => Pin::Hash("7b01df370bc205a1a28ad8f56819e905a76c15b0cb21cc88574152a575711de4"),
    comment_described_mp3 => Pin::Hash("78924f43cb5e71bbfb6d1cc64e341caa3d11cd91a08531082b31aa976bcf0828"),
    unmapped_frame_mp3 => Pin::Hash("c8a118c3268b7b964e0533d3b5961aa9e60072dd554a9448f0a7ba59a16316cc"),
    rich_mp3 => Pin::Hash("5154ecda545243c5b603d164f8453b48f4b4e67b99947fa450bca422b45aee55"),
    rich_mp3_existing_composer => Pin::Hash("0708e37c8af2842932ee3935a359e9a69b291b623538645a9342901bf29535c4"),
    rich_mp3_existing_composer_other_case => Pin::Hash("1457d73e6ea13fd20cfcf66b5937832ca27abd34c9433a46d3756246846d8466"),
    rich_mp3_joined_groups => Pin::Hash("2322d5060b564da026d9b848d07870cf59b556b38ffd845fdfd27c43570d5e2c"),
    default_sorts_mp3 => Pin::Hash("7f3aae5f61dc634fbff84784a70d56639a93d893e51fa9e15971e37352f43a52"),
    cover_mp3 => Pin::Hash("f4b24919ba85363215012ea5e012334a34c2568eef8a77f39fe659b11a3ac37a"),
    aac_m4a => Pin::Json(r#"{"schema":"rustunes.mpeg-source-preparation","source_file_sha256":"4f4c1b43a5fd9e3f28cfde7dc9fcbc79220f9e167b21407e790e228186bd8397","encoded_audio_sha256":"049ab4ad258a5ca51fc01482021ae15e4402877442c56eba47151d0a544a0e39","metadata":{"title":"Format title","artist":"Format artist","album":"Format album","album_artist":null,"genre":"Test","composer":null,"copyright":null,"year":2024,"track":3,"track_total":9,"disc":null,"disc_total":null,"compilation":null},"coded_timing_proposal":{"basis":"edit_list_samples_with_declared_encoder_trim","sample_rate":44100,"channels":2,"bit_rate_kbps":117,"coded_samples":4410,"total_time_ms_floor":100,"native_mapping_qualified":false,"format":"aac-lc","variable_bit_rate":true,"gapless":{"delay":1024,"drain":0,"heuristic":1,"resynch":0}},"artwork":{"embedded_picture_count":0,"front_cover_count":0,"embedded_bytes_preserved":true,"native_artwork_action_qualified":false},"compatibility_issues":[{"code":"mp3_other_format_batch_only","detail":"AAC-LC goes only in an insert batch"},{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"}],"required_qualification":["mpeg_positive_timing_receiver","mpeg_source_metadata_and_collection_mapping","mpeg_public_insertion_completion_and_recovery"],"execution_authorized":false}"#),
    flac => Pin::Hash("afd2d10cd2345f7a3cd87df8c1d6ece69cd34bcd3a058e623649773d795f7908"),
    wav => Pin::Hash("6e1c0e6ae3f8c03a06937fba520a29f719859d033530fc788e8f37ff15b4261e"),
    aiff => Pin::Hash("2deda8b669581266da101874ef24bf76e9c88abb84fc197f02e8346c2e3213c1"),
    mpeg2_mp3 => Pin::Hash("ace06e142a49d076ccad6a1a18c501ded8f2fb3a99a8820577ecf070752cb98c"),
    vbr_mpeg1_mp3 => Pin::Hash("1198f79e3683d62a46c0849b72ad23d3270c7ba2bb30fd71bf724e9a9b35a83a"),
    compilation_without_album_artist_mp3 => Pin::Hash("2481e0f1aed6f1fc896f5b8b40d3bef8f13f1ddaec8b3be53904baf87caba212"),
    back_cover_mp3 => Pin::Hash("71c450e77516c02a29200804314e6e93db81fab6b5ae42ca7f26ed47307df27b"),
    missing_title_mp3 => Pin::Hash("8b9e00ea33c5403a5031f8d88ed70c18068e278860dcfb623e05d56f1ee53cf4"),
    second_title_mp3 => Pin::Hash("4a5616f9d8c94677ec423ef2761092c824e512aeb7bccd7a44518203e967034f"),
    second_artist_empty_mp3 => Pin::Hash("54f678d7bf091bed2030d6eed35e1320659416fe27ef6e7a3c8f70b9c9f41797"),
    empty_album_mp3 => Pin::Hash("e6c1406155593dad645dc55a175b5694bdf31eaf77b42235add7103b77d47b42"),
    bad_release_date_mp3 => Pin::Hash("7bc415e07e4cff51df9a1496a2bdb09f7a60a73d67a0896eb6fd2383bf7569b5"),
    zero_release_year_mp3 => Pin::Hash("f5ac590ae96a26983451bf9412617bf576fdf0ed6c5ee9dc06cef33d2d604962"),
    numeric_genre_mp3 => Pin::Hash("89cb2ef65d537f11b35d3bb1034700da7d3afc1165ac553d98e07a5d64b6e832"),
    zero_track_mp3 => Pin::Hash("c55ace98af69f6c2a0df97766f1c556b094f65406d3e74cccb58780b11c97218"),
    disc_exceeds_total_mp3 => Pin::Hash("5bc68bb4dcc72a39c6da2faa93f4da5f59748d602eb28c5d17472d76ac07467e"),
    oversize_mp3 => Pin::Hash("ac403d801e8b2592ce5aa4e617308c03eead11b80df5ddd23f8fc50a565b4645"),
    forged_without_apple_timing_mp3 => Pin::Hash("453db7bd9d846cb643a25b4e75897c6e242863ab255fda3b1af42e0a471a8910"),
    forged_wav_at_12_bits => Pin::Hash("b8dcde7209e5b103f8f5f286be6b50b9c9bd03d5e55371337c90044e2e64faf2"),
    forged_flac_disagreeing_with_its_identity => Pin::Hash("b12e0af12190f5afea4aafa88b8ff0491c3b14e2953aa87a9b04ab7c8cd7d5fb"),
});

pins!(issues, issues, {
    cbr_mp3 => Pin::Json(r#"[{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    rich_mp3 => Pin::Json(r#"[{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    id3v1_trailer_mp3 => Pin::Json(r#"[{"code":"mp3_trailing_tag_not_projected","detail":"A trailing ID3v1 tag stays in the file; its fields are not projected"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    comment_described_mp3 => Pin::Json(r#"[{"code":"mp3_unmapped_metadata_frames","detail":"Tag fields stay in the file but do not reach the phone's catalogue: COMM"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    unmapped_frame_mp3 => Pin::Json(r#"[{"code":"mp3_unmapped_metadata_frames","detail":"Tag fields stay in the file but do not reach the phone's catalogue: TIT3"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    cover_mp3 => Pin::Json(r#"[{"code":"mp3_artwork_transfer_unqualified","detail":"Embedded pictures are inspected; the cover goes only in an insert batch"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    aac_m4a => Pin::Json(r#"[{"code":"mp3_other_format_batch_only","detail":"AAC-LC goes only in an insert batch"},{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"}]"#),
    mpeg2_mp3 => Pin::Json(r#"[{"code":"mp3_other_format_batch_only","detail":"MPEG-2 Layer III goes only in an insert batch"},{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"}]"#),
    compilation_without_album_artist_mp3 => Pin::Json(r#"[{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    missing_title_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"missing title"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    second_title_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"title has 2 values in the file's tag; edit the tag to keep one"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    second_artist_empty_mp3 => Pin::Json(r#"[{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    empty_album_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"missing album"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    bad_release_date_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"a release date must start with a four-digit year"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    zero_release_year_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"zero release year"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    numeric_genre_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"numeric ID3 genres require an explicit label mapping"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    zero_track_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"zero track numbering"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    disc_exceeds_total_mp3 => Pin::Json(r#"[{"code":"mp3_metadata_policy_required","detail":"disc number exceeds total"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    oversize_mp3 => Pin::Json(r#"[{"code":"mp3_native_transfer_size_limit","detail":"Source has 67109169 bytes; the existing native audio transfer limit is 67108864 bytes (64 MiB)"},{"code":"mp3_gapless_batch_only","detail":"The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch"}]"#),
    forged_without_apple_timing_mp3 => Pin::Json(r#"[{"code":"mp3_timing_policy_required","detail":"Apple's MPEG timing unavailable"}]"#),
    forged_wav_at_12_bits => Pin::Json(r#"[{"code":"mp3_other_format_batch_only","detail":"WAV (PCM) goes only in an insert batch"},{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"},{"code":"mp3_timing_policy_required","detail":"the phone plays WAV (PCM) at 8, 16 or 24 bits; this is 12-bit"}]"#),
    forged_flac_disagreeing_with_its_identity => Pin::Json(r#"[{"code":"mp3_other_format_batch_only","detail":"FLAC goes only in an insert batch"},{"code":"mp3_extended_metadata_mapping_required","detail":"Additional tags are projected without loss; they go only in an insert batch"},{"code":"mp3_timing_policy_required","detail":"audio report and identity differ"}]"#),
});

pins!(operation, operation, {
    cbr_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    info_header_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    xing_header_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    vbri_header_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    vbr_without_header_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    id3v1_trailer_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    comment_empty_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    comment_written_mp3 => Pin::Hash("5d2ffdae1390159e5889246648fca47f0233b1e8f757d98f3315504ac6bce28f"),
    comment_described_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    unmapped_frame_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    rich_mp3 => Pin::Hash("90c859fb1265de6f817f2799327c49448a97f5142b5a3f79c33b70e1b7cd7f55"),
    rich_mp3_existing_composer => Pin::Hash("2e82dddf30bd159dbc3f3bf0713da4272af111e696ddf649e975edfe57604d0b"),
    rich_mp3_existing_composer_other_case => Pin::Hash("2e82dddf30bd159dbc3f3bf0713da4272af111e696ddf649e975edfe57604d0b"),
    rich_mp3_joined_groups => Pin::Hash("1f44bdeba41ac08522861fd6ee0f48b5def4cb9e25de3b994907fbcb09513c38"),
    default_sorts_mp3 => Pin::Hash("a1e98ae935d631c35fcda4fa41cb95ba36921c392af99344f2d17e1425be339b"),
    cover_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    aac_m4a => Pin::Hash("a7bfb87c234f9e0301e5fb7b1677daf7f2d24104fc6ae946722a3b63ed28d0e2"),
    flac => Pin::Hash("36080482e5ea6f5f89cfad6d9d3d9af02d19756981f9b602e7007203a1fb48c3"),
    wav => Pin::Hash("f52e2939a8facdccaff2f0340b6fd441980b222f52358c269a7c4d2d8af7c72b"),
    aiff => Pin::Hash("f784665b8d00efe977b49305692518ad71a8d8b2ece5729aa678b28ffd19bdac"),
    mpeg2_mp3 => Pin::Hash("a1bb23d78000e9805c96b7088df908da70427f4766ed87335cd76b80f11c25a6"),
    vbr_mpeg1_mp3 => Pin::Hash("8bf56633ce647e654897446406c21fa1287255eb0eea603da5d4327bff9035c6"),
    compilation_without_album_artist_mp3 => Pin::Refused,
    back_cover_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    missing_title_mp3 => Pin::Refused,
    second_title_mp3 => Pin::Refused,
    second_artist_empty_mp3 => Pin::Hash("f0e58f51e84c4e4773446169b2a71c1a35b283a4f0c99006530ba2d5dbd046df"),
    empty_album_mp3 => Pin::Refused,
    bad_release_date_mp3 => Pin::Refused,
    zero_release_year_mp3 => Pin::Refused,
    numeric_genre_mp3 => Pin::Refused,
    zero_track_mp3 => Pin::Refused,
    disc_exceeds_total_mp3 => Pin::Refused,
});

pins!(operation_joined, operation_joined, {
    rich_mp3 => Pin::Hash("50ef3214d90191fc574ca9ca5bba2f713c5d8d763e93499787a8e448cfe155b5"),
    rich_mp3_existing_composer => Pin::Hash("24cf002b465862400e40072d6b73841546ae42a8783f126164d42c346d3964f4"),
    rich_mp3_joined_groups => Pin::Hash("ed75e8d52a3e35e10d423ad971e47897609f39fdcd2911333a3eb856c2a7e439"),
});

pins!(operation_unsorted_composer, operation_unsorted_composer, {
    rich_mp3 => Pin::Hash("90c859fb1265de6f817f2799327c49448a97f5142b5a3f79c33b70e1b7cd7f55"),
    rich_mp3_existing_composer => Pin::Refused,
    rich_mp3_existing_composer_other_case => Pin::Refused,
});

pins!(fields, fields, {
    cbr_mp3 => Pin::Json(r#"[{"title":"Independent Song","artist":"Source Artist","album":"Source Album","album_artist":null,"genre":null,"composer":null,"copyright":null,"year":null,"track":null,"track_total":null,"disc":null,"disc_total":null,"compilation":null},{"basis":"apple_counted_frames_and_played_samples","sample_rate":44100,"channels":2,"bit_rate_kbps":128,"coded_samples":1176,"total_time_ms_floor":52,"native_mapping_qualified":false,"gapless":{"delay":528,"drain":600,"heuristic":33554435,"resynch":0}}]"#),
    info_header_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    xing_header_mp3 => Pin::Hash("2e2c007f2e051133696c353928088708fe42c81aed7030769e59d21be7b61c26"),
    vbri_header_mp3 => Pin::Hash("2e2c007f2e051133696c353928088708fe42c81aed7030769e59d21be7b61c26"),
    vbr_without_header_mp3 => Pin::Hash("2e2c007f2e051133696c353928088708fe42c81aed7030769e59d21be7b61c26"),
    id3v1_trailer_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    comment_empty_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    comment_written_mp3 => Pin::Hash("d3eb37dd42fcc2cc210d87ca4d36afef4262f224f7f66cd7699fd3811a8e8f73"),
    comment_described_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    unmapped_frame_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    rich_mp3 => Pin::Json(r#"[{"title":"Independent Song","artist":"Source Artist","album":"Source Album","album_artist":"Ensemble","genre":"Jazz","composer":"New Composer","copyright":"Copyright Owner","year":2026,"track":2,"track_total":3,"disc":1,"disc_total":2,"compilation":true,"comment":"a comment\non two lines","sort_title":"Song, Independent","sort_artist":"Artist, Source","sort_album":"Album, Source","sort_album_artist":"Ensemble, The","sort_composer":"Composer, A"},{"basis":"apple_counted_frames_and_played_samples","sample_rate":44100,"channels":2,"bit_rate_kbps":128,"coded_samples":1176,"total_time_ms_floor":52,"native_mapping_qualified":false,"gapless":{"delay":528,"drain":600,"heuristic":33554435,"resynch":0}}]"#),
    rich_mp3_existing_composer => Pin::Hash("fae188a742c9390d874e87b45c5468e334a73b2179707feb920f64deae23c730"),
    rich_mp3_existing_composer_other_case => Pin::Hash("d274176c36139db32d2a34cbfc2b3789360a218230c11de512c13d65d567018b"),
    rich_mp3_joined_groups => Pin::Hash("72ab3d3a6b3aec7348b200e182ba86a5c53776356038db555e481ec9acf485d8"),
    default_sorts_mp3 => Pin::Hash("a7dc811c0cc30f45ae4f3af13c2d0b0315cb7b5611ff7a2753c95ac5b1d1726a"),
    cover_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    aac_m4a => Pin::Json(r#"[{"title":"Format title","artist":"Format artist","album":"Format album","album_artist":null,"genre":"Test","composer":null,"copyright":null,"year":2024,"track":3,"track_total":9,"disc":null,"disc_total":null,"compilation":null},{"basis":"edit_list_samples_with_declared_encoder_trim","sample_rate":44100,"channels":2,"bit_rate_kbps":117,"coded_samples":4410,"total_time_ms_floor":100,"native_mapping_qualified":false,"format":"aac-lc","variable_bit_rate":true,"gapless":{"delay":1024,"drain":0,"heuristic":1,"resynch":0}}]"#),
    flac => Pin::Hash("93eaa3c5e43617ed9ab50c8a451f6cc6378c1ce4fc9d40e8af8c7ad90a5a9e23"),
    wav => Pin::Hash("a48a0b37a8bb707dc9af135936a9ccccbe3525d963afe183e79623dd99406ab7"),
    aiff => Pin::Hash("653565b598752e26d8d60db80b51f8f1aa89168d07aeb8705d5f69a603d0f924"),
    mpeg2_mp3 => Pin::Hash("1b21775ecc75b6f4c94bc3722c87e40989f0d9721b3c009e685a9d6747c40e84"),
    vbr_mpeg1_mp3 => Pin::Hash("0d62b140ce82bbfb5337852877d5673832b4ba6a62c172d6078b1c5dd7f21b4f"),
    compilation_without_album_artist_mp3 => Pin::Refused,
    back_cover_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    missing_title_mp3 => Pin::Refused,
    second_title_mp3 => Pin::Refused,
    second_artist_empty_mp3 => Pin::Hash("04afb48d6678d575eb7498ea56f25f6979425ce8566c04bb476ebc8e315b86f7"),
    empty_album_mp3 => Pin::Refused,
    bad_release_date_mp3 => Pin::Refused,
    zero_release_year_mp3 => Pin::Refused,
    numeric_genre_mp3 => Pin::Refused,
    zero_track_mp3 => Pin::Refused,
    disc_exceeds_total_mp3 => Pin::Refused,
});

pins!(expected, expected, {
    cbr_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    info_header_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    xing_header_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    vbri_header_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    vbr_without_header_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    id3v1_trailer_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    comment_empty_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    comment_written_mp3 => Pin::Hash("7f75990a1cf71ce32ad0792332cbbc3002e7d386ed5564153ff28fcb67a52c2f"),
    comment_described_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    unmapped_frame_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    rich_mp3 => Pin::Json(r#"[{"title":"Independent Song","artist":"Source Artist","album":"Source Album","album_artist":"Ensemble","genre":"Jazz","composer":"New Composer","copyright":"Copyright Owner","year":2026,"content_rating":null,"compilation":true,"track":2,"track_total":3,"disc":1,"disc_total":2,"comment":"a comment\non two lines","sort_title":"Song, Independent","sort_artist":"Artist, Source","sort_album":"Album, Source","sort_album_artist":"Ensemble, The","sort_composer":"Composer, A"},52]"#),
    rich_mp3_existing_composer => Pin::Hash("2478aef0301052a8be581481ae350ed1130cf165b09757043bc69880600887f0"),
    rich_mp3_existing_composer_other_case => Pin::Hash("50ac4f3b4c6bd5a90bc6d3547bd54371bfb84a57185b62b4be6275be5c7b1972"),
    rich_mp3_joined_groups => Pin::Hash("1bc80f840c2e37a756a7a1b1df7bb0255db621ec93ee529d82ea60fc1b1dc661"),
    default_sorts_mp3 => Pin::Hash("c36dda965bd0a2cbe770ccd61f7adc120a47c9108c8db2301b3314be0ff5c768"),
    cover_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    aac_m4a => Pin::Hash("ad0755b0186599ab98375091fccdffe1cbc4931ffad79ad1ebdc8858e0153ad1"),
    flac => Pin::Hash("ad0755b0186599ab98375091fccdffe1cbc4931ffad79ad1ebdc8858e0153ad1"),
    wav => Pin::Hash("ad0755b0186599ab98375091fccdffe1cbc4931ffad79ad1ebdc8858e0153ad1"),
    aiff => Pin::Hash("ad0755b0186599ab98375091fccdffe1cbc4931ffad79ad1ebdc8858e0153ad1"),
    mpeg2_mp3 => Pin::Hash("2d9d3948a51d2559caaba1fdbf0b0e290fd05f4950ebcff27c2a11264459adf8"),
    vbr_mpeg1_mp3 => Pin::Hash("5a1a3b543114bda2fb07815e660022f1de8490dbf4ccef86247a442fa7a21f4a"),
    compilation_without_album_artist_mp3 => Pin::Refused,
    back_cover_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    missing_title_mp3 => Pin::Refused,
    second_title_mp3 => Pin::Refused,
    second_artist_empty_mp3 => Pin::Hash("65b79660777d755bbf5127c47260394430104d0a93e95e5702f61b11c303332f"),
    empty_album_mp3 => Pin::Refused,
    bad_release_date_mp3 => Pin::Refused,
    zero_release_year_mp3 => Pin::Refused,
    numeric_genre_mp3 => Pin::Refused,
    zero_track_mp3 => Pin::Refused,
    disc_exceeds_total_mp3 => Pin::Refused,
});

pins!(projected, projected, {
    cbr_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    info_header_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    xing_header_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    vbri_header_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    vbr_without_header_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    id3v1_trailer_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    comment_empty_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    comment_written_mp3 => Pin::Hash("1ce2fe083ea0afabe3fa2e4016be6b8ce3adc9be1639242c55bbaf5440406866"),
    comment_described_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    unmapped_frame_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    rich_mp3 => Pin::Json(r#"{"title":"Independent Song","sort_name":"Song, Independent","comment":"a comment\non two lines","artist":"Source Artist","album":"Source Album","album_artist":"Ensemble","grouping_artist":"Ensemble","genre":"Jazz","composer":"New Composer","copyright":"Copyright Owner","year":2026,"content_rating":null,"compilation":true,"track":2,"track_total":3,"disc":1,"disc_total":2,"sample_rate":44100,"bit_depth":0,"channels":2,"duration_ticks":1176,"total_time_ms":52}"#),
    rich_mp3_existing_composer => Pin::Hash("675bc69eb353cb1eb26f272e3469fbc9ef6c22fedeffcffb4d83318281ce47f1"),
    rich_mp3_existing_composer_other_case => Pin::Hash("7ab9fd4d44493bce7b78a1dc9fe9e4b008774faa568a5d2320504ceaa2e26d96"),
    rich_mp3_joined_groups => Pin::Hash("052c2b32f0d53b3cf248d09a875b555ff291e9bc2188bdedcd26af491eccb6c5"),
    default_sorts_mp3 => Pin::Hash("a6dd6cbd64d2d9896d4b5d0c19f8ca5a4347d189033a4b87365e447dbea55185"),
    cover_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    aac_m4a => Pin::Json(r#"{"title":"Format title","sort_name":"Format title","artist":"Format artist","album":"Format album","album_artist":null,"grouping_artist":"Format artist","genre":"Test","composer":null,"copyright":null,"year":2024,"content_rating":null,"compilation":null,"track":3,"track_total":9,"disc":null,"disc_total":null,"sample_rate":44100,"bit_depth":0,"channels":2,"duration_ticks":4410,"total_time_ms":100}"#),
    flac => Pin::Hash("9704846afaf4e0108848f05e0ec264f69e3bc7543d13cd5d6f10a6ccd527f670"),
    wav => Pin::Hash("9704846afaf4e0108848f05e0ec264f69e3bc7543d13cd5d6f10a6ccd527f670"),
    aiff => Pin::Hash("0f6879e9d2cfce695a27741ae53db7822ff82e09ec69a0010caf1d33b7694f84"),
    mpeg2_mp3 => Pin::Hash("70e49183e0673c57fd4e39c15416f07e012fdeef6b88b4728ba118eefb860b1e"),
    vbr_mpeg1_mp3 => Pin::Hash("30ed61598bd3b6da5fe90d97d3ed72fb15c2aa286beb59d6d27149c4b3e9fff4"),
    compilation_without_album_artist_mp3 => Pin::Refused,
    back_cover_mp3 => Pin::Json(r#"{"title":"Independent Song","sort_name":"Independent Song","artist":"Source Artist","album":"Source Album","album_artist":null,"grouping_artist":"Source Artist","genre":null,"composer":null,"copyright":null,"year":null,"content_rating":null,"compilation":null,"track":null,"track_total":null,"disc":null,"disc_total":null,"sample_rate":44100,"bit_depth":0,"channels":2,"duration_ticks":1176,"total_time_ms":52}"#),
    missing_title_mp3 => Pin::Refused,
    second_title_mp3 => Pin::Refused,
    second_artist_empty_mp3 => Pin::Hash("2469aca3efc8a6fa164c2a0312bef50ee51565ce0f6b6aa8a55e12a6111c354a"),
    empty_album_mp3 => Pin::Refused,
    bad_release_date_mp3 => Pin::Refused,
    zero_release_year_mp3 => Pin::Refused,
    numeric_genre_mp3 => Pin::Refused,
    zero_track_mp3 => Pin::Refused,
    disc_exceeds_total_mp3 => Pin::Refused,
});
