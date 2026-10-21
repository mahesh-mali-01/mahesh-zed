use super::*;

fn side(path: &str, bytes: &[u8], mode: u32) -> FileSide {
    FileSide {
        path: path.into(),
        object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
        mode,
    }
}

fn file() -> FileComparison {
    FileComparison {
        kind: ComparisonKind::Full,
        old: Some(side("auth.ts", b"admin\n", 0o100644)),
        new: Some(side("auth.ts", b"admin reviewer\n", 0o100644)),
    }
}

fn snapshot() -> ReviewSnapshot {
    ReviewSnapshot {
        generation: 1,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: "branch".into(),
                left: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "1".repeat(40),
                },
                right: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "2".repeat(40),
                },
            },
            files: vec![file()],
        }],
    }
}

#[test]
fn fingerprints_track_both_sides_paths_modes_and_kind() -> Result<()> {
    let original = file();
    let fingerprint = original.fingerprint()?;
    let mut mutations = Vec::new();
    let mut changed = original.clone();
    changed.old = Some(side("auth.ts", b"base advanced\n", 0o100644));
    mutations.push(changed);
    let mut changed = original.clone();
    changed.new = Some(side("auth.ts", b"maintainer\n", 0o100644));
    mutations.push(changed);
    let mut changed = original.clone();
    changed.new = Some(side("moved.ts", b"admin reviewer\n", 0o100644));
    mutations.push(changed);
    let mut changed = original.clone();
    changed.new = Some(side("auth.ts", b"admin reviewer\n", 0o100755));
    mutations.push(changed);
    let mut changed = original.clone();
    changed.new = Some(side("auth.ts", b"admin reviewer\n", 0o120000));
    mutations.push(changed);
    let mut changed = original.clone();
    changed.new = None;
    mutations.push(changed);
    let mut changed = original.clone();
    changed.kind = ComparisonKind::SinceReview;
    mutations.push(changed);
    for changed in mutations {
        assert_ne!(fingerprint, changed.fingerprint()?);
    }
    assert_eq!(
        fingerprint,
        serde_json::from_str::<FileComparison>(&serde_json::to_string(&original)?)?
            .fingerprint()?
    );
    Ok(())
}

#[test]
fn freshness_ignores_commit_provenance_but_scopes_groups() -> Result<()> {
    let mut current = snapshot();
    let checkpoint = ReviewedCheckpoint {
        group: "branch".into(),
        comparison: file(),
        generation: 1,
        reviewed_at_unix_seconds: 1,
    };
    current.generation = 2;
    current.groups[0].endpoints.right.hex = "3".repeat(40);
    current.groups[0].files.push(FileComparison {
        kind: ComparisonKind::Full,
        old: None,
        new: Some(side("billing.ts", b"12\n", 0o100644)),
    });
    assert!(checkpoint.matches("branch", current.file("branch", "auth.ts")?)?);
    assert!(!checkpoint.matches("other commit group", current.file("branch", "auth.ts")?)?);
    assert!(current.file("branch", "removed.ts").is_err());
    let mut latest = checkpoint.clone();
    latest.comparison.new = Some(side("auth.ts", b"latest\n", 0o100644));
    assert!(!latest.matches("branch", &checkpoint.comparison)?);
    Ok(())
}

#[test]
fn retention_distinguishes_absent_empty_binary_and_gitlink() -> Result<()> {
    assert_eq!(
        ObjectId::for_blob(ObjectFormat::Sha1, b"").hex,
        "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
    );
    for bytes in [b"".as_slice(), b"\0binary\xff", b"a\r\nb\r\n"] {
        let added = FileComparison {
            kind: ComparisonKind::Full,
            old: None,
            new: Some(side("new\tfile\n", bytes, 0o100644)),
        };
        let contents = RetainedContents {
            old: None,
            new: Some(bytes.to_vec()),
        };
        added.validate_retained(&contents, bytes.len())?;
        assert!(
            added
                .validate_retained(
                    &RetainedContents {
                        old: None,
                        new: None
                    },
                    usize::MAX
                )
                .is_err()
        );
    }
    let deleted = FileComparison {
        kind: ComparisonKind::Full,
        old: Some(side("gone", b"", 0o100644)),
        new: None,
    };
    deleted.validate_retained(
        &RetainedContents {
            old: Some(vec![]),
            new: None,
        },
        0,
    )?;
    let link = FileComparison {
        kind: ComparisonKind::Full,
        old: None,
        new: Some(side("module", b"commit identity only", 0o160000)),
    };
    link.validate_retained(
        &RetainedContents {
            old: None,
            new: None,
        },
        0,
    )?;
    assert!(
        link.validate_retained(
            &RetainedContents {
                old: None,
                new: Some(vec![])
            },
            0
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn retention_rejects_wrong_bytes_partial_content_and_budget_overflow() -> Result<()> {
    let file = file();
    let correct = RetainedContents {
        old: Some(b"admin\n".to_vec()),
        new: Some(b"admin reviewer\n".to_vec()),
    };
    file.validate_retained(&correct, 21)?;
    assert!(file.validate_retained(&correct, 1).is_err());
    assert!(
        file.validate_retained(
            &RetainedContents {
                old: correct.old.clone(),
                new: Some(b"admin reviewer\r\n".to_vec())
            },
            usize::MAX
        )
        .is_err()
    );
    assert!(
        file.validate_retained(
            &RetainedContents {
                old: None,
                new: correct.new.clone()
            },
            usize::MAX
        )
        .is_err()
    );
    let mut incremental = file;
    incremental.kind = ComparisonKind::SinceReview;
    assert!(incremental.validate_retained(&correct, usize::MAX).is_err());
    Ok(())
}

#[test]
fn sha256_and_invalid_metadata_are_explicit() -> Result<()> {
    let object = ObjectId::for_blob(ObjectFormat::Sha256, b"hello\n");
    object.validate()?;
    let added = FileComparison {
        kind: ComparisonKind::Full,
        old: None,
        new: Some(FileSide {
            path: "file".into(),
            object,
            mode: 0o100644,
        }),
    };
    added.validate(ObjectFormat::Sha256)?;
    added.validate_retained(
        &RetainedContents {
            old: None,
            new: Some(b"hello\n".to_vec()),
        },
        100,
    )?;
    assert!(added.validate(ObjectFormat::Sha1).is_err());
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "./a",
        "a//b",
        "a\0b",
        "a/",
    ] {
        assert!(validate_path(path).is_err());
    }
    for hex in [
        "0".repeat(40),
        "F".repeat(40),
        "a".repeat(39),
        "a;".repeat(20),
    ] {
        assert!(
            ObjectId {
                format: ObjectFormat::Sha1,
                hex
            }
            .validate()
            .is_err()
        );
    }
    let mut unchanged = file();
    unchanged.old = unchanged.new.clone();
    assert!(unchanged.fingerprint().is_err());
    let mut invalid_mode = file();
    if let Some(side) = invalid_mode.new.as_mut() {
        side.mode = 0o100600;
    }
    assert!(invalid_mode.fingerprint().is_err());
    Ok(())
}

#[test]
fn snapshots_reject_duplicate_groups_files_and_mixed_formats() {
    let mut candidate = snapshot();
    candidate.groups.push(candidate.groups[0].clone());
    assert!(candidate.validate(ObjectFormat::Sha1).is_err());
    let mut candidate = snapshot();
    candidate.groups[0].files.push(file());
    assert!(candidate.validate(ObjectFormat::Sha1).is_err());
    let mut candidate = snapshot();
    candidate.groups[0].endpoints.right.format = ObjectFormat::Sha256;
    assert!(candidate.validate(ObjectFormat::Sha1).is_err());
    let mut candidate = snapshot();
    candidate.generation = 0;
    assert!(candidate.validate(ObjectFormat::Sha1).is_err());
}

#[test]
fn stale_async_results_cannot_override_newer_selection_or_cancellation() -> Result<()> {
    let snapshot = snapshot();
    let endpoints = snapshot
        .groups
        .iter()
        .map(|group| group.endpoints.clone())
        .collect::<Vec<_>>();
    let mut gate = RefreshGate::default();
    let first = gate.begin("session".into(), 1, endpoints.clone())?;
    let second = gate.begin("session".into(), 1, endpoints.clone())?;
    assert!(!gate.accepts(&first, &snapshot));
    assert!(gate.accepts(&second, &snapshot));
    let mut mismatched = snapshot.clone();
    mismatched.groups[0].endpoints.right.hex = "4".repeat(40);
    assert!(!gate.accepts(&second, &mismatched));
    let other = gate.begin("other session".into(), 1, endpoints)?;
    assert!(!gate.accepts(&second, &snapshot));
    assert!(gate.accepts(&other, &snapshot));
    let mut wrong_generation = snapshot.clone();
    wrong_generation.generation = 2;
    assert!(!gate.accepts(&other, &wrong_generation));
    gate.cancel();
    assert!(!gate.accepts(&other, &snapshot));
    Ok(())
}

#[test]
fn commit_selectors_validate_identity_and_parent_format() -> Result<()> {
    let commit = ObjectId {
        format: ObjectFormat::Sha1,
        hex: "1".repeat(40),
    };
    let selection = CommitSelection {
        commit,
        parent: None,
    };
    ComparisonSelector::Commits {
        selections: vec![selection.clone()],
    }
    .validate(ObjectFormat::Sha1)?;
    assert!(
        ComparisonSelector::Commits { selections: vec![] }
            .validate(ObjectFormat::Sha1)
            .is_err()
    );
    assert!(
        ComparisonSelector::Commits {
            selections: vec![selection.clone(), selection.clone()]
        }
        .validate(ObjectFormat::Sha1)
        .is_err()
    );
    let mut mixed = selection;
    mixed.parent = Some(ObjectId::for_blob(ObjectFormat::Sha256, b"parent"));
    assert!(
        ComparisonSelector::Commits {
            selections: vec![mixed]
        }
        .validate(ObjectFormat::Sha1)
        .is_err()
    );
    Ok(())
}
