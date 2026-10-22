use super::*;
use crate::review::{ComparisonKind, ObjectFormat, ObjectId};

fn side(path: &str, bytes: &[u8]) -> FileSide {
    FileSide {
        path: path.into(),
        object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
        mode: 0o100644,
    }
}
fn edge(old: Option<FileSide>, new: Option<FileSide>) -> FileComparison {
    FileComparison {
        kind: ComparisonKind::Full,
        old,
        new,
    }
}
#[test]
fn out_of_order_edges_cover_the_whole_change_but_gaps_do_not() -> Result<()> {
    let a = Some(side("file.rs", b"a"));
    let b = Some(side("file.rs", b"b"));
    let c = Some(side("file.rs", b"c"));
    let d = Some(side("file.rs", b"d"));
    let canonical = edge(a.clone(), d.clone());
    let mut receipts = vec![edge(c.clone(), d), edge(a, b.clone())];
    assert!(!covers_file(&canonical, &receipts)?);
    receipts.push(edge(b, c));
    assert!(covers_file(&canonical, &receipts)?);
    Ok(())
}
#[test]
fn absent_sides_are_path_scoped_and_renames_fail_closed() -> Result<()> {
    let a = Some(side("file.rs", b"a"));
    let b = Some(side("file.rs", b"b"));
    assert!(covers_file(
        &edge(None, b.clone()),
        &[edge(None, a.clone()), edge(a.clone(), b.clone())]
    )?);
    assert!(!covers_file(
        &edge(a.clone(), b),
        &[
            edge(a.clone(), None),
            edge(None, Some(side("other.rs", b"b")))
        ]
    )?);
    assert!(covers_file(
        &edge(a.clone(), None),
        &[edge(a.clone(), None)]
    )?);
    assert!(!covers_file(
        &edge(a.clone(), Some(side("renamed.rs", b"a"))),
        &[edge(a, Some(side("renamed.rs", b"a")))]
    )?);
    Ok(())
}
#[test]
fn modes_and_original_sides_must_match_exactly() -> Result<()> {
    let a = Some(side("file.rs", b"a"));
    let b = Some(side("file.rs", b"b"));
    let mut executable = b.clone();
    executable.as_mut().unwrap().mode = 0o100755;
    assert!(!covers_file(
        &edge(a.clone(), executable.clone()),
        &[edge(a.clone(), b.clone())]
    )?);
    assert!(covers_file(
        &edge(a.clone(), executable.clone()),
        &[edge(a.clone(), b.clone()), edge(b, executable)]
    )?);
    assert!(!covers_file(
        &edge(Some(side("file.rs", b"different base")), a.clone()),
        &[edge(None, a)]
    )?);
    Ok(())
}

#[test]
fn two_thousand_reverse_order_transitions_cover_without_skipping_a_gap() -> Result<()> {
    let sides = (0..=2000)
        .map(|index| Some(side("file.rs", index.to_string().as_bytes())))
        .collect::<Vec<_>>();
    let canonical = edge(sides[0].clone(), sides[2000].clone());
    let receipts = (0..2000)
        .rev()
        .map(|index| edge(sides[index].clone(), sides[index + 1].clone()))
        .collect::<Vec<_>>();
    assert!(covers_file(&canonical, &receipts)?);
    let gap = receipts
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1000)
        .map(|(_, edge)| edge.clone())
        .collect::<Vec<_>>();
    assert!(!covers_file(&canonical, &gap)?);
    Ok(())
}
