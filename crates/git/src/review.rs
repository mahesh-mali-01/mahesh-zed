use crate::{
    Oid,
    repository::{GitBinary, RepoPath},
};
use anyhow::{Context as _, Result, ensure};
use futures::AsyncWriteExt as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr as _,
};
use util::command::Stdio;
use util::rel_path::RelPath;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSide {
    pub object: Oid,
    pub mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewFile {
    pub path: RepoPath,
    pub old: Option<ReviewSide>,
    pub new: Option<ReviewSide>,
}

#[derive(Clone, Debug)]
pub struct ReviewComparison {
    pub left: Oid,
    pub right: Oid,
    pub files: Vec<ReviewFile>,
}

#[derive(Clone, Debug)]
pub struct ReviewCommit {
    pub commit: Oid,
    pub parents: Vec<Oid>,
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct ReviewCommitSelection {
    pub commit: Oid,
    pub parent: Option<Oid>,
}

pub(crate) async fn history(
    git: GitBinary,
    base: Option<Oid>,
    head: Oid,
) -> Result<Vec<ReviewCommit>> {
    let head = resolve(&git, &head.to_string()).await?;
    let mut arguments = vec![
        "rev-list".to_owned(),
        "--topo-order".into(),
        head.to_string(),
    ];
    if let Some(base) = base {
        resolve(&git, &base.to_string()).await?;
        arguments.extend(["--not".into(), base.to_string()]);
    }
    // rev-list deliberately stops at shallow grafts. Never present that prefix
    // as a complete PR history: check reachable grafts before reading objects.
    ensure_complete_history(&git, base, head).await?;
    let refs = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let bytes = output(&git, &refs).await?;
    let ids = std::str::from_utf8(&bytes)?
        .lines()
        .map(Oid::from_str)
        .collect::<Result<Vec<_>, _>>()?;
    commit_metadata_batch(&git, &ids).await
}

async fn ensure_complete_history(git: &GitBinary, base: Option<Oid>, head: Oid) -> Result<()> {
    let shallow_path = output(
        git,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shallow",
        ],
    )
    .await?;
    let shallow_path = std::str::from_utf8(&shallow_path)?.trim();
    let shallow = match smol::fs::read_to_string(shallow_path).await {
        Ok(shallow) => shallow,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading shallow history boundaries"),
    };
    let boundaries: BTreeSet<_> = shallow.lines().collect();
    if boundaries.is_empty() {
        return Ok(());
    }
    let mut args = vec!["rev-list".to_owned(), head.to_string()];
    if let Some(base) = base {
        args.push(base.to_string());
    }
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let reachable = output(git, &args).await?;
    ensure!(
        !std::str::from_utf8(&reachable)?
            .lines()
            .any(|id| boundaries.contains(id)),
        "review history is missing at a shallow boundary; fetch/deepen the repository"
    );
    Ok(())
}

/// One cat-file process for all requested raw objects. Formatting log parents
/// would lose true parents at a shallow graft; parsing raw headers retains them.
async fn commit_metadata_batch(git: &GitBinary, ids: &[Oid]) -> Result<Vec<ReviewCommit>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut process = git
        .build_command(&["cat-file", "--batch"])
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting review commit metadata batch")?;
    let mut stdin = process
        .stdin
        .take()
        .context("missing commit metadata input")?;
    let input = ids.iter().map(|id| format!("{id}\n")).collect::<String>();
    // Concurrent IO matters once the request exceeds an OS pipe buffer: reading
    // only after all writes would deadlock on thousands of commit objects.
    let write = async move {
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
        Ok::<_, anyhow::Error>(())
    };
    let read = async move {
        process
            .output()
            .await
            .context("reading commit metadata batch")
    };
    let (_, output) = futures::try_join!(write, read)?;
    ensure!(
        output.status.success(),
        "Git review commit metadata failed; fetch/deepen missing history"
    );
    parse_commit_metadata_batch(&output.stdout, ids)
}
fn parse_commit_metadata_batch(mut bytes: &[u8], ids: &[Oid]) -> Result<Vec<ReviewCommit>> {
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        let end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .context("incomplete commit metadata header")?;
        let header = std::str::from_utf8(&bytes[..end])?;
        let parts: Vec<_> = header.split(' ').collect();
        ensure!(
            parts.len() == 3 && parts[1] == "commit",
            "commit history is missing or is not a commit; fetch/deepen the repository"
        );
        ensure!(
            Oid::from_str(parts[0])? == *id,
            "commit metadata identity mismatch"
        );
        let size: usize = parts[2].parse().context("invalid commit metadata size")?;
        bytes = &bytes[end + 1..];
        ensure!(
            size < bytes.len() && bytes[size] == b'\n',
            "incomplete commit metadata body"
        );
        let text = String::from_utf8_lossy(&bytes[..size]);
        let (headers, message) = text.split_once("\n\n").context("invalid commit object")?;
        let parents = headers
            .lines()
            .filter_map(|line| line.strip_prefix("parent "))
            .map(Oid::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        result.push(ReviewCommit {
            commit: *id,
            parents,
            subject: message.lines().next().unwrap_or_default().to_owned(),
        });
        bytes = &bytes[size + 1..];
    }
    ensure!(bytes.is_empty(), "unexpected trailing commit metadata");
    Ok(result)
}

pub(crate) async fn commits(
    git: GitBinary,
    selections: Vec<ReviewCommitSelection>,
    range: bool,
) -> Result<Vec<ReviewComparison>> {
    ensure!(!selections.is_empty(), "select at least one commit");
    let mut seen = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for selection in &selections {
        ensure!(
            seen.insert(selection.commit.to_string()),
            "duplicate selected commit"
        );
        ids.insert(selection.commit.to_string());
        if let Some(parent) = selection.parent {
            ids.insert(parent.to_string());
        }
    }
    let ids = ids
        .iter()
        .map(|id| Oid::from_str(id))
        .collect::<Result<Vec<_>, _>>()?;
    let metadata: BTreeMap<_, _> = commit_metadata_batch(&git, &ids)
        .await?
        .into_iter()
        .map(|commit| (commit.commit.to_string(), commit))
        .collect();
    let mut empty_tree = None;
    let mut endpoints = Vec::new();
    for selection in selections {
        let metadata = metadata
            .get(&selection.commit.to_string())
            .context("missing selected commit metadata")?;
        if range {
            ensure!(
                metadata.parents.len() <= 1,
                "ranges require a linear chain without merge commits; use separate groups"
            );
        }
        let left = match selection.parent {
            Some(parent) => {
                ensure!(
                    metadata.parents.contains(&parent),
                    "chosen parent is not a parent of this commit"
                );
                // The parent's raw object was validated in the same batch.
                parent
            }
            None => {
                ensure!(
                    metadata.parents.is_empty(),
                    "choose a parent explicitly for this commit"
                );
                match empty_tree {
                    Some(tree) => tree,
                    None => {
                        let bytes = output(&git, &["hash-object", "-t", "tree", "--stdin"]).await?;
                        let tree = Oid::from_str(std::str::from_utf8(&bytes)?.trim())?;
                        empty_tree = Some(tree);
                        tree
                    }
                }
            }
        };
        if range {
            if let Some((_, previous)) = endpoints.last() {
                ensure!(
                    selection.parent.as_ref() == Some(previous),
                    "range selection must contain every commit in oldest-to-newest parent order"
                );
            }
        }
        endpoints.push((left, selection.commit));
    }
    if range {
        endpoints = vec![(
            endpoints.first().context("range start")?.0,
            endpoints.last().context("range end")?.1,
        )];
    }
    let mut comparisons = Vec::new();
    for (left, right) in endpoints {
        comparisons.push(diff(&git, left, right).await?);
    }
    Ok(comparisons)
}

async fn output(git: &GitBinary, arguments: &[&str]) -> Result<Vec<u8>> {
    let result = git.build_command(arguments).output().await?;
    ensure!(
        result.status.success(),
        "Git review comparison failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}

async fn resolve(git: &GitBinary, selector: &str) -> Result<Oid> {
    ensure!(
        !selector.is_empty() && !selector.starts_with('-') && !selector.contains('\0'),
        "invalid review ref"
    );
    let is_oid =
        matches!(selector.len(), 40 | 64) && selector.bytes().all(|byte| byte.is_ascii_hexdigit());
    if selector != "HEAD" && !is_oid {
        output(git, &["check-ref-format", "--allow-onelevel", selector]).await?;
    }
    let bytes = output(
        git,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{selector}^{{commit}}"),
        ],
    )
    .await?;
    let object = Oid::from_str(std::str::from_utf8(&bytes)?.trim())?;
    ensure!(!object.is_zero(), "invalid zero commit object");
    Ok(object)
}

pub(crate) async fn load(
    git: GitBinary,
    base: String,
    head: String,
    merge_base: bool,
) -> Result<ReviewComparison> {
    let mut left = resolve(&git, &base).await?;
    let right = resolve(&git, &head).await?;
    if merge_base {
        let bytes = output(
            &git,
            &["merge-base", "--all", &left.to_string(), &right.to_string()],
        )
        .await?;
        let bases = std::str::from_utf8(&bytes)?.lines().collect::<Vec<_>>();
        ensure!(
            bases.len() == 1,
            "review requires a unique merge base; choose a direct comparison or obtain the missing history"
        );
        left = Oid::from_str(bases.first().context("missing merge base")?)?;
    }
    diff(&git, left, right).await
}

async fn diff(git: &GitBinary, left: Oid, right: Oid) -> Result<ReviewComparison> {
    let bytes = output(
        git,
        &[
            "diff-tree",
            "-r",
            "--raw",
            "-z",
            "--no-abbrev",
            "--no-renames",
            &left.to_string(),
            &right.to_string(),
            "--",
        ],
    )
    .await?;
    let mut files = parse(&bytes)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(ReviewComparison { left, right, files })
}

fn side(mode: &str, object: &str) -> Result<Option<ReviewSide>> {
    let mode = u32::from_str_radix(mode, 8)?;
    let object = Oid::from_str(object)?;
    if mode == 0 {
        ensure!(object.is_zero(), "absent review side has a nonzero object");
        return Ok(None);
    }
    ensure!(
        !object.is_zero() && matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000),
        "unsupported review entry mode/object"
    );
    Ok(Some(ReviewSide { object, mode }))
}

fn parse(bytes: &[u8]) -> Result<Vec<ReviewFile>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(bytes.last() == Some(&0), "truncated review metadata");
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut files = Vec::new();
    while let Some(header) = fields.next() {
        if header.is_empty() {
            ensure!(fields.next().is_none(), "unexpected empty review record");
            break;
        }
        let header = std::str::from_utf8(header)?
            .strip_prefix(':')
            .context("invalid review header")?;
        let parts = header.split_ascii_whitespace().collect::<Vec<_>>();
        let [old_mode, new_mode, old_object, new_object, status] = parts.as_slice() else {
            anyhow::bail!("invalid review metadata field count");
        };
        let path = std::str::from_utf8(fields.next().context("missing review path")?)
            .context("unsupported non-UTF-8 review path")?;
        ensure!(!path.is_empty(), "empty review path");
        let old = side(old_mode, old_object)?;
        let new = side(new_mode, new_object)?;
        ensure!(
            match *status {
                "A" => old.is_none() && new.is_some(),
                "D" => old.is_some() && new.is_none(),
                "M" | "T" => old.is_some() && new.is_some(),
                _ => false,
            },
            "unsupported or inconsistent review status"
        );
        files.push(ReviewFile {
            path: RepoPath::from_rel_path(RelPath::from_unix_str(path)?),
            old,
            new,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_commit_batch_preserves_raw_parents_and_rejects_partial_responses() -> Result<()> {
        let id = Oid::from_str(&"1".repeat(40))?;
        let parent = Oid::from_str(&"2".repeat(40))?;
        let body = format!(
            "tree {}\nparent {parent}\ngpgsig signature\n parent is a continuation\n\nsubject\nbody\n",
            "3".repeat(40)
        );
        let raw = format!("{id} commit {}\n{body}\n", body.len());
        let parsed = parse_commit_metadata_batch(raw.as_bytes(), &[id])?;
        assert_eq!(parsed[0].parents, vec![parent]);
        assert_eq!(parsed[0].subject, "subject");
        assert!(parse_commit_metadata_batch(format!("{id} missing\n").as_bytes(), &[id]).is_err());
        assert!(parse_commit_metadata_batch(&raw.as_bytes()[..raw.len() - 2], &[id]).is_err());
        assert!(parse_commit_metadata_batch(raw.as_bytes(), &[parent]).is_err());
        assert!(parse_commit_metadata_batch(raw.as_bytes(), &[id, id]).is_err());
        Ok(())
    }

    #[test]
    fn review_metadata_preserves_modes_paths_and_absence() -> Result<()> {
        let old = "1".repeat(40);
        let new = "2".repeat(40);
        let raw = format!(
            ":100644 100755 {old} {old} M\0path\twith\nlines\0:000000 100644 {} {new} A\0empty\0",
            "0".repeat(40)
        );
        let files = parse(raw.as_bytes())?;
        assert_eq!(files.len(), 2);
        let first = files.first().context("first file")?;
        assert_eq!(first.path.as_unix_str(), "path\twith\nlines");
        assert_eq!(first.old.as_ref().context("old")?.mode, 0o100644);
        assert_eq!(first.new.as_ref().context("new")?.mode, 0o100755);
        assert!(files.get(1).context("added")?.old.is_none());
        assert!(parse(raw.trim_end_matches('\0').as_bytes()).is_err());
        assert!(parse(format!(":100644 100644 {old} {new} M\0").as_bytes()).is_err());
        Ok(())
    }
}
