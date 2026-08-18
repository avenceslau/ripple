use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::graph::normalize_path;
use crate::workspace::Package;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    name: String,
    path: Option<PathBuf>,
}

pub fn affected_packages(
    root: &Path,
    changed_paths: &[PathBuf],
    packages: &[Package],
    selected: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    if !root.join("Cargo.toml").is_file() {
        return Ok(BTreeSet::new());
    }

    let metadata = cargo_metadata(root)?;
    let root = normalize_path(root);
    let crate_dirs: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| {
            let dir = package
                .manifest_path
                .parent()
                .map(normalize_path)
                .unwrap_or_else(|| root.clone());
            (package.name.clone(), dir)
        })
        .collect();
    let mut reverse: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();

    for package in &metadata.packages {
        let Some(consumer) = package.manifest_path.parent().map(normalize_path) else {
            continue;
        };
        for dependency in &package.dependencies {
            let Some(path) = &dependency.path else {
                continue;
            };
            let path = normalize_path(path);
            let dependency_dir = crate_dirs
                .get(&dependency.name)
                .filter(|dir| **dir == path)
                .cloned()
                .unwrap_or(path);
            reverse
                .entry(dependency_dir)
                .or_default()
                .insert(consumer.clone());
        }
    }

    let changed_paths: Vec<_> = changed_paths.iter().map(normalize_path).collect();
    let root_manifest_changed = changed_paths.iter().any(|path| {
        path.parent() == Some(root.as_path())
            && matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("Cargo.toml" | "Cargo.lock")
            )
    });
    let mut reached: BTreeSet<PathBuf> = if root_manifest_changed {
        crate_dirs.values().cloned().collect()
    } else {
        changed_paths
            .iter()
            .filter_map(|path| {
                crate_dirs
                    .values()
                    .filter(|dir| path.starts_with(dir))
                    .max_by_key(|dir| dir.components().count())
                    .cloned()
            })
            .collect()
    };
    let mut pending: VecDeque<_> = reached.iter().cloned().collect();
    while let Some(dependency) = pending.pop_front() {
        for consumer in reverse.get(&dependency).into_iter().flatten() {
            if reached.insert(consumer.clone()) {
                pending.push_back(consumer.clone());
            }
        }
    }

    Ok(packages
        .iter()
        .filter(|package| selected.contains(&package.name))
        .filter(|package| reached.contains(&normalize_path(&package.dir)))
        .map(|package| package.name.clone())
        .collect())
}

fn cargo_metadata(root: &Path) -> Result<Metadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("cargo metadata returned invalid JSON")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn propagates_changes_through_path_dependencies() {
        let root = tempdir().unwrap();
        let shared = root.path().join("packages/shared");
        let worker = root.path().join("apps/worker");
        fs::create_dir_all(shared.join("src")).unwrap();
        fs::create_dir_all(worker.join("src")).unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nresolver = '2'\nmembers = ['packages/shared', 'apps/worker']\n",
        )
        .unwrap();
        fs::write(
            shared.join("Cargo.toml"),
            "[package]\nname = 'shared'\nversion = '0.1.0'\nedition = '2021'\n",
        )
        .unwrap();
        fs::write(shared.join("src/lib.rs"), "pub fn shared() {}\n").unwrap();
        fs::write(
            worker.join("Cargo.toml"),
            "[package]\nname = 'worker'\nversion = '0.1.0'\nedition = '2021'\n[dependencies]\nshared = { path = '../../packages/shared' }\n",
        )
        .unwrap();
        fs::write(worker.join("src/lib.rs"), "pub fn worker() {}\n").unwrap();

        let packages = vec![Package {
            name: "worker-app".to_string(),
            dir: worker,
            scripts: BTreeMap::from([("rust:build".to_string(), "cargo build".to_string())]),
            entrypoint: None,
            exports: BTreeMap::new(),
            dependencies: BTreeSet::new(),
        }];
        let selected = BTreeSet::from(["worker-app".to_string()]);
        let affected = affected_packages(
            root.path(),
            &[shared.join("src/lib.rs")],
            &packages,
            &selected,
        )
        .unwrap();

        assert_eq!(affected, selected);
    }
}
