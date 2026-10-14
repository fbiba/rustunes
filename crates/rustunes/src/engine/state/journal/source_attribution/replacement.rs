//! Descriptive ALAC replacement ancestry. No physical/Original/native authority.
use super::{Binding, Identity, InitialFacts, Journal};
use crate::{
    engine::execution::native_insert::Control,
    engine::state::journal::row_ancestry::{self as ancestry, RowOrigin},
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// An exact native origin; a relative source path is not yet established.
pub(crate) struct Origin {
    native: RowOrigin,
    initial: Binding,
    attempt: super::Attempt,
}
impl Origin {
    pub(crate) fn input_digest(&self) -> [u8; 32] {
        self.native.input_digest()
    }
}
pub(crate) fn origin(journal: &Journal, binding: &Binding, identity: &Identity) -> Result<Origin> {
    ensure!(
        journal.outstanding()?.is_none(),
        "replacement ancestry requires all-kind idle journal"
    );
    ensure!(
        binding.content.source.is_alac(),
        "MPEG replacement ancestry unsupported"
    );
    let native = ancestry::origin(journal, binding, identity)?;
    let (initial, attempt, _) = super::native_origin(journal, native.reference(), identity, true)?;
    ensure!(
        initial.content.source == binding.content.source
            && initial.content.track_pid == binding.content.track_pid
            && initial.content.path == binding.content.path
            && initial.content.collection == binding.content.collection,
        "active physical Content differs from immutable native insertion"
    );
    Ok(Origin {
        native,
        initial,
        attempt,
    })
}

/// Hash-bound producer path claim. Full plan/selection checks still required.
pub(crate) struct PathClaim {
    origin: Origin,
    relative_path: String,
}
impl PathClaim {
    pub(crate) fn relative_path(&self) -> &str {
        &self.relative_path
    }
    pub(crate) fn prior_digests(
        &self,
        journal: &Journal,
        identity: &Identity,
        plan: &[u8],
    ) -> Result<Vec<[u8; 32]>> {
        ancestry::prior_digests(journal, identity, &self.origin.native, plan)
    }
}
pub(crate) fn path_claim(origin: Origin, identity: &Identity, input: &[u8]) -> Result<PathClaim> {
    let value = super::producer(&origin.initial, &origin.attempt, identity, input, None)?;
    let relative_path = super::text(&value, "source")?.to_owned();
    super::relative(&relative_path)?;
    Ok(PathClaim {
        origin,
        relative_path,
    })
}

/// The insertion's exact historical row: the producer input and plan the
/// journal pins name the source's root, selection and relative path, so its
/// absolute path. Later tag, cover or number edits of the track do not
/// matter: replacement removes the track and adds the source again.
pub(crate) struct Association {
    facts: InitialFacts,
    projected: Value,
    receipt: Value,
}
impl Association {
    pub(crate) fn content(&self) -> &super::super::Content {
        &self.facts.content
    }
    pub(crate) fn root(&self) -> &Path {
        &self.facts.root
    }
    pub(crate) fn base(&self) -> &Path {
        &self.facts.base
    }
    pub(crate) fn relative_path(&self) -> &str {
        &self.facts.relative_path
    }
    pub(crate) fn options(&self) -> &crate::engine::planning::source_collection::SelectionOptions {
        &self.facts.options
    }
    pub(crate) fn historical_path(&self) -> PathBuf {
        self.facts.base.join(&self.facts.relative_path)
    }
    /// The tags the insertion projected, before any later edit.
    pub(crate) fn projected(&self) -> &Value {
        &self.projected
    }
    pub(crate) fn evidence(&self) -> &Value {
        &self.receipt
    }
    pub(crate) fn reference(&self) -> super::BindingRef {
        self.facts.reference
    }
    pub(crate) fn insertion(&self) -> i64 {
        self.facts.insertion.as_i64()
    }
}
pub(crate) fn check(
    journal: &Journal,
    identity: &Identity,
    claim: &PathClaim,
    input: &[u8],
    plan: &[u8],
    prior: &[Vec<u8>],
    control: &Control,
) -> Result<Association> {
    control.check()?;
    ensure!(
        journal.outstanding()?.is_none(),
        "replacement journal no longer idle"
    );
    let row = super::descriptive_row(
        journal,
        claim.origin.native.reference(),
        identity,
        input,
        plan,
        prior,
    )?;
    ensure!(
        row.facts.relative_path == claim.relative_path,
        "checked path differs from producer claim"
    );
    journal.recheck_read_only_source()?;
    control.check()?;
    Ok(Association {
        facts: row.facts,
        projected: row.projected,
        receipt: row.receipt,
    })
}
