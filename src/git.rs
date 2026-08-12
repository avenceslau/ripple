use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed { old_path: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

pub fn changed_files(root: &Path, base: &str) -> Result<Vec<ChangedFile>> {
    let output = git(root, &["diff", "--name-status", "--find-renames", base])?;
    let mut changes = Vec::new();
    let mut known = BTreeSet::new();

    for line in output.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        let Some(status) = fields.first() else {
            continue;
        };

        let change = match status.chars().next() {
            Some('A') if fields.len() >= 2 => ChangedFile {
                path: root.join(fields[1]),
                kind: ChangeKind::Added,
            },
            Some('M' | 'T') if fields.len() >= 2 => ChangedFile {
                path: root.join(fields[1]),
                kind: ChangeKind::Modified,
            },
            Some('D') if fields.len() >= 2 => ChangedFile {
                path: root.join(fields[1]),
                kind: ChangeKind::Deleted,
            },
            Some('R') if fields.len() >= 3 => ChangedFile {
                path: root.join(fields[2]),
                kind: ChangeKind::Renamed {
                    old_path: PathBuf::from(fields[1]),
                },
            },
            _ => continue,
        };
        known.insert(change.path.clone());
        changes.push(change);
    }

    let untracked = git(root, &["ls-files", "--others", "--exclude-standard"])?;
    for path in untracked.lines().filter(|path| !path.is_empty()) {
        let path = root.join(path);
        if known.insert(path.clone()) {
            changes.push(ChangedFile {
                path,
                kind: ChangeKind::Added,
            });
        }
    }

    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

pub fn file_at(root: &Path, revision: &str, path: &Path) -> Result<Option<String>> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let object = format!("{revision}:{}", relative.to_string_lossy());
    let output = Command::new("git")
        .args(["show", &object])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git show for {}", relative.display()))?;

    if output.status.success() {
        return String::from_utf8(output.stdout)
            .context("git returned non-UTF-8 source")
            .map(Some);
    }

    Ok(None)
}

pub fn extract_revision(root: &Path, revision: &str) -> Result<TempDir> {
    let output = Command::new("git")
        .args(["archive", "--format=tar", revision])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to archive git revision {revision}"))?;
    if !output.status.success() {
        bail!(
            "git archive failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let destination = tempfile::tempdir()?;
    let mut archive = tar::Archive::new(Cursor::new(output.stdout));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let contains_env_file = path.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| name.starts_with(".env"))
        });
        if !contains_env_file {
            entry.unpack_in(destination.path())?;
        }
    }
    Ok(destination)
}

pub fn repository_root(start: &Path) -> Result<PathBuf> {
    let output = git(start, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(output.trim()))
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout).context("git returned non-UTF-8 output")
}
