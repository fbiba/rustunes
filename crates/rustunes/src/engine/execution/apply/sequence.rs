//! Sequential application and aggregate receipt publication. The step runner
//! owns all codec/proof policy; an error stops before another step is entered.
use crate::{
    engine::planning::additive::{Decision, SourceDecision},
    engine::state::current::hex,
};
use anyhow::Result;
use music_protocol::protocol::trace::private_write;
use std::{fs::File, future::Future, path::Path};

pub(super) struct StepResult<S> {
    pub state: S,
    pub receipt: serde_json::Value,
}
pub(super) fn step_name(number: usize) -> String {
    format!("step-{number:04}")
}

pub(super) async fn run<'a, S, F, Fut, Summary>(
    rows: &'a [SourceDecision],
    mut state: S,
    trace: &Path,
    plan_sha256: [u8; 32],
    mut step: F,
    summary: Summary,
) -> Result<()>
where
    F: FnMut(S, &'a SourceDecision, usize) -> Fut,
    Fut: Future<Output = Result<StepResult<S>>>,
    Summary: FnOnce(&S) -> (u32, usize),
{
    let mut completed = Vec::new();
    let mut entered = 0;
    for row in rows
        .iter()
        .filter(|r| matches!(r.decision, Decision::Add { .. }))
    {
        entered += 1;
        let result = step(state, row, entered).await?;
        state = result.state;
        completed.push(result.receipt);
    }
    let (anchor, tracks) = summary(&state);
    let receipt = serde_json::json!({"schema":"rustunes.applied-additions","plan_sha256":hex(&plan_sha256),
        "verified_additions":completed,"phone_deletions":0,"final_media_anchor":anchor,
        "final_track_count":tracks,"artwork_updates_complete":false});
    private_write(
        &trace.join("applied.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    File::open(trace)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::planning::additive::{ArtworkDecision, Group};
    use std::{
        os::unix::fs::DirBuilderExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    fn root() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "apply-sequence-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }
    fn row(name: &str, add: bool) -> SourceDecision {
        let group = || Group {
            id: "0000000000001234".into(),
            wire_id: "4660".into(),
            status: "new",
        };
        SourceDecision {
            relative_path: name.into(),
            managed_audio_replacement: None,
            source_file_bytes: None,
            source_file_sha256: None,
            encoded_audio_sha256: None,
            projected: None,
            mp3: None,
            mp3_insertion: None,
            flac_to_alac: false,
            validation_issues: vec![],
            matches: vec![],
            artwork: ArtworkDecision::NoSourceCoverPreserveExisting,
            native_composer: None,
            decision: if add {
                Decision::Add {
                    track_id: "0000000000001235".into(),
                    wire_id: "4661".into(),
                    album: group(),
                    artist: group(),
                    path: "iTunes_Control/Music/F00/TEST.m4a".into(),
                    pending_collection_ids: vec![],
                    fresh_state_required_before_execution: true,
                }
            } else {
                Decision::Keep
            },
        }
    }

    // The injected step owns no journal or native permit.

    #[tokio::test]
    async fn steps_run_in_order_and_publish_one_complete_receipt() {
        let root = root();
        let rows = vec![
            row("one.m4a", true),
            row("keep.m4a", false),
            row("two.m4a", true),
            row("three.m4a", true),
        ];
        run(
            &rows,
            (10u32, 20usize),
            &root,
            [7; 32],
            |state, r, number| {
                let root = &root;
                async move {
                    assert_eq!(number as u32, state.0 - 9);
                    let dir = root.join(step_name(number));
                    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
                    let receipt =
                        serde_json::json!({"source":r.relative_path,"step":step_name(number)});
                    private_write(&dir.join("verified.json"), &serde_json::to_vec(&receipt)?)?;
                    assert!(!root.join("applied.json").exists());
                    Ok(StepResult {
                        state: (state.0 + 1, state.1 + 1),
                        receipt,
                    })
                }
            },
            |state| (state.0, state.1),
        )
        .await
        .unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("applied.json")).unwrap()).unwrap();
        assert_eq!(receipt["verified_additions"].as_array().unwrap().len(), 3);
        assert_eq!(receipt["final_media_anchor"], 13);
        assert_eq!(receipt["final_track_count"], 23);
        assert_eq!(receipt["verified_additions"][2]["step"], step_name(3));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 4);
    }
    #[tokio::test]
    async fn a_failed_step_keeps_the_prefix_and_never_runs_or_publishes_the_tail() {
        let root = root();
        let rows = vec![
            row("one.m4a", true),
            row("two.m4a", true),
            row("three.m4a", true),
        ];
        let entered = std::cell::Cell::new(0);
        let result = run(
            &rows,
            (),
            &root,
            [7; 32],
            |(), _, n| {
                let root = &root;
                let entered = &entered;
                async move {
                    entered.set(entered.get() + 1);
                    assert_eq!(n, entered.get());
                    let dir = root.join(step_name(n));
                    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
                    if n == 2 {
                        private_write(&dir.join("failure.txt"), b"retained partial evidence")?;
                        anyhow::bail!("step refused");
                    }
                    Ok(StepResult {
                        state: (),
                        receipt: serde_json::json!({"step":n}),
                    })
                }
            },
            |_| (13, 23),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(entered.get(), 2);
        assert!(root.join("step-0001").is_dir());
        assert!(root.join("step-0002/failure.txt").is_file());
        assert!(!root.join("step-0003").exists());
        assert!(!root.join("applied.json").exists());
    }
}
