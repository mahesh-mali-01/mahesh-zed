//! Content transitions reviewed in immutable PR comparisons.
//!
//! Receipts are scoped to a PR/account and verified Git history by the store.
//! This module only answers whether those transitions cover a full file change.
use crate::review::{FileComparison, FileSide};
use anyhow::{Context as _, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Reachability over exact path/blob/mode states. An absent state belongs to the
/// requested path, so deleting another file cannot approve an unrelated addition.
/// Renames are conservatively excluded: resolving identity across rename chains
/// requires a separate provenance proof.
pub fn covers_file(canonical: &FileComparison, receipts: &[FileComparison]) -> Result<bool> {
    let path = canonical.file_key()?;
    let format = canonical
        .new
        .as_ref()
        .or(canonical.old.as_ref())
        .context("Coverage comparison has no file identity")?
        .object
        .format;
    canonical.validate(format)?;
    fn belongs(side: &Option<FileSide>, path: &str) -> bool {
        side.as_ref().is_none_or(|side| side.path == path)
    }
    if !belongs(&canonical.old, path) || !belongs(&canonical.new, path) {
        return Ok(false);
    }
    // Index each edge once: large, reverse-order commit selections must not
    // repeatedly validate and scan every receipt for each newly reached state.
    type State = Option<(String, String, u32)>;
    let key = |side: &Option<FileSide>| {
        side.as_ref()
            .map(|side| (side.path.clone(), side.object.hex.clone(), side.mode))
    };
    let mut edges: BTreeMap<State, Vec<State>> = BTreeMap::new();
    for receipt in receipts {
        receipt.validate(format)?;
        if receipt.file_key()? == path && belongs(&receipt.old, path) && belongs(&receipt.new, path)
        {
            edges
                .entry(key(&receipt.old))
                .or_default()
                .push(key(&receipt.new));
        }
    }
    let start = key(&canonical.old);
    let goal = key(&canonical.new);
    let mut reached = BTreeSet::from([start.clone()]);
    let mut pending = VecDeque::from([start]);
    while let Some(state) = pending.pop_front() {
        if state == goal {
            return Ok(true);
        }
        if let Some(next) = edges.remove(&state) {
            for state in next {
                if reached.insert(state.clone()) {
                    pending.push_back(state);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
#[path = "review_coverage_tests.rs"]
mod tests;
