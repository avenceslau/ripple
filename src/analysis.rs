use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::git::{ChangeKind, ChangedFile, file_at};
use crate::graph::{DependencyGraph, Node, normalize_path};
use crate::parser::{ParsedModule, is_source_file, parse_module};
use crate::workspace::{Package, package_for_path};

#[derive(Clone, Debug, Default)]
pub struct ChangeSeeds {
    pub nodes: BTreeSet<Node>,
    pub type_nodes: BTreeSet<Node>,
    pub direct_packages: BTreeMap<String, BTreeMap<PathBuf, Vec<String>>>,
}

pub fn find_change_seeds(
    root: &Path,
    base: &str,
    changes: &[ChangedFile],
    packages: &[Package],
    graph: &DependencyGraph,
    base_graph: Option<&DependencyGraph>,
) -> Result<ChangeSeeds> {
    let mut result = ChangeSeeds::default();

    for change in changes {
        if !is_source_file(&change.path) {
            let file_name = change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let mut input_paths = vec![change.path.clone()];
            if let ChangeKind::Renamed { old_path } = &change.kind {
                input_paths.push(root.join(old_path));
            }
            for path in input_paths {
                let input = Node::new(normalize_path(path), crate::parser::MODULE_INIT);
                let referenced = graph
                    .edges
                    .values()
                    .chain(graph.type_edges.values())
                    .any(|dependencies| dependencies.contains(&input));
                if referenced {
                    result.nodes.insert(input.clone());
                    result.type_nodes.insert(input);
                }
            }

            let package_owner = package_for_path(packages, &change.path);
            let is_rust_source = change
                .path
                .extension()
                .is_some_and(|extension| extension == "rs");
            let is_root_js_lockfile = change.path.parent() == Some(root)
                && matches!(
                    file_name,
                    "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "bun.lock" | "bun.lockb"
                );
            let is_workspace_config =
                change.path.parent() == Some(root) && file_name == "pnpm-workspace.yaml";
            let is_root_tsconfig = change.path.parent() == Some(root)
                && file_name.starts_with("tsconfig")
                && file_name.ends_with(".json");
            if is_root_js_lockfile || is_workspace_config || is_root_tsconfig {
                let details = describe_direct_input(root, base, change)?;
                for target in &graph.targets {
                    result
                        .direct_packages
                        .entry(target.package.clone())
                        .or_default()
                        .insert(change.path.clone(), details.clone());
                }
                continue;
            }

            let was_package_manifest = file_name == "package.json"
                || matches!(
                    &change.kind,
                    ChangeKind::Renamed { old_path }
                        if old_path.file_name().is_some_and(|name| name == "package.json")
                );
            let has_current_manifest_owner = package_owner.is_some_and(|package| {
                change
                    .path
                    .parent()
                    .is_some_and(|parent| parent == package.dir)
            });
            if was_package_manifest && !has_current_manifest_owner {
                let details = describe_direct_input(root, base, change)?;
                for target in &graph.targets {
                    result
                        .direct_packages
                        .entry(target.package.clone())
                        .or_default()
                        .insert(change.path.clone(), details.clone());
                }
                continue;
            }

            let is_root_cargo_input = change.path.parent() == Some(root)
                && matches!(file_name, "Cargo.toml" | "Cargo.lock");
            let is_unowned_cargo_input = package_owner.is_some_and(|package| package.dir == root)
                && (is_rust_source || matches!(file_name, "Cargo.toml" | "build.rs"));
            if is_root_cargo_input || is_unowned_cargo_input {
                let details = describe_direct_input(root, base, change)?;
                for package in packages.iter().filter(|package| {
                    package.dir.join("Cargo.toml").is_file()
                        && graph
                            .targets
                            .iter()
                            .any(|target| target.package == package.name)
                }) {
                    result
                        .direct_packages
                        .entry(package.name.clone())
                        .or_default()
                        .insert(change.path.clone(), details.clone());
                    for (path, module) in graph
                        .modules
                        .iter()
                        .filter(|(path, _)| path.starts_with(&package.dir))
                    {
                        add_all_symbols(&mut result.nodes, &mut result.type_nodes, path, module);
                    }
                }
                continue;
            }

            if let Some(package) = package_owner {
                let relative = change
                    .path
                    .strip_prefix(&package.dir)
                    .unwrap_or(&change.path);
                let is_rust_source = is_rust_source
                    && relative
                        .components()
                        .any(|component| component.as_os_str() == "src");
                let is_build_input = matches!(
                    file_name,
                    "package.json" | "wrangler.json" | "wrangler.jsonc" | "Cargo.toml" | "build.rs"
                ) || file_name.starts_with("vite.config.")
                    || (file_name.starts_with("tsconfig") && file_name.ends_with(".json"));

                if is_rust_source || is_build_input {
                    let details = describe_direct_input(root, base, change)?;
                    result
                        .direct_packages
                        .entry(package.name.clone())
                        .or_default()
                        .insert(change.path.clone(), details);
                    for (path, module) in graph
                        .modules
                        .iter()
                        .filter(|(path, _)| path.starts_with(&package.dir))
                    {
                        add_all_symbols(&mut result.nodes, &mut result.type_nodes, path, module);
                    }
                }
            }
            continue;
        }

        if let Some(package) = package_for_path(packages, &change.path) {
            let target_node = DependencyGraph::target_node(&package.name);
            let relative = change
                .path
                .strip_prefix(&package.dir)
                .unwrap_or(&change.path);
            let is_runtime_source = relative
                .components()
                .any(|component| component.as_os_str() == "src")
                || package.entrypoint.as_ref() == Some(&change.path);
            let is_unlinked_target = graph
                .targets
                .iter()
                .any(|target| target.package == package.name)
                && !graph.edges.contains_key(&target_node);
            if is_runtime_source && is_unlinked_target {
                let details = describe_direct_input(root, base, change)?;
                result
                    .direct_packages
                    .entry(package.name.clone())
                    .or_default()
                    .insert(change.path.clone(), details);
            }
        }

        let current_path = normalize_path(&change.path);
        let current = graph.modules.get(&current_path);
        let old_relative = match &change.kind {
            ChangeKind::Renamed { old_path } => old_path.as_path(),
            _ => change.path.strip_prefix(root).unwrap_or(&change.path),
        };
        let old_path = root.join(old_relative);
        let old_graph_module = base_graph.and_then(|graph| graph.modules.get(&old_path));
        let old_source = match change.kind {
            ChangeKind::Added => None,
            _ => file_at(root, base, &old_path)?,
        };
        let old = old_source
            .as_deref()
            .map(|source| parse_module(&old_path, source))
            .transpose()?;

        match (old.as_ref(), current) {
            (None, Some(current)) => add_all_symbols(
                &mut result.nodes,
                &mut result.type_nodes,
                &current_path,
                current,
            ),
            (Some(_), Some(current)) if matches!(change.kind, ChangeKind::Renamed { .. }) => {
                add_all_symbols(
                    &mut result.nodes,
                    &mut result.type_nodes,
                    &current_path,
                    current,
                );
                if let Some(old_module) = old_graph_module {
                    add_all_symbols(
                        &mut result.nodes,
                        &mut result.type_nodes,
                        &old_path,
                        old_module,
                    );
                }
            }
            (Some(old), Some(current)) => {
                add_changed_symbols(
                    &mut result.nodes,
                    &mut result.type_nodes,
                    &old_path,
                    &current_path,
                    old,
                    current,
                );
            }
            (Some(old), None) => add_all_symbols(
                &mut result.nodes,
                &mut result.type_nodes,
                &old_path,
                old_graph_module.unwrap_or(old),
            ),
            (None, None) => {}
        }
    }

    Ok(result)
}

fn describe_direct_input(root: &Path, base: &str, change: &ChangedFile) -> Result<Vec<String>> {
    let file_name = change.path.file_name().and_then(|name| name.to_str());
    if file_name != Some("package.json") {
        let action = match &change.kind {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed { .. } => "renamed",
        };
        return Ok(vec![format!("{action} build input")]);
    }

    let old_relative = match &change.kind {
        ChangeKind::Renamed { old_path } => old_path.as_path(),
        _ => change.path.strip_prefix(root).unwrap_or(&change.path),
    };
    let old = file_at(root, base, &root.join(old_relative))?
        .and_then(|source| serde_json::from_str::<Value>(&source).ok());
    let current = fs::read_to_string(&change.path)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok());
    let (Some(old), Some(current)) = (old, current) else {
        return Ok(vec![match change.kind {
            ChangeKind::Added => "added package manifest".to_string(),
            ChangeKind::Deleted => "deleted package manifest".to_string(),
            ChangeKind::Modified | ChangeKind::Renamed { .. } => {
                "changed package manifest".to_string()
            }
        }]);
    };

    let mut details = Vec::new();
    for (section, noun) in [
        ("dependencies", "dependency"),
        ("devDependencies", "development dependency"),
        ("peerDependencies", "peer dependency"),
        ("optionalDependencies", "optional dependency"),
        ("scripts", "script"),
    ] {
        let old_values = old
            .get(section)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let current_values = current
            .get(section)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let names: BTreeSet<_> = old_values
            .keys()
            .chain(current_values.keys())
            .cloned()
            .collect();
        for name in names {
            match (old_values.get(&name), current_values.get(&name)) {
                (None, Some(value)) => {
                    details.push(format!("added {noun} `{name}` = {}", display_json(value)))
                }
                (Some(_), None) => details.push(format!("removed {noun} `{name}`")),
                (Some(before), Some(after)) if before != after => details.push(format!(
                    "changed {noun} `{name}` from {} to {}",
                    display_json(before),
                    display_json(after)
                )),
                _ => {}
            }
        }
    }

    let ignored = BTreeSet::from([
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
        "scripts",
    ]);
    let keys: BTreeSet<_> = old
        .as_object()
        .into_iter()
        .flat_map(|object| object.keys())
        .chain(
            current
                .as_object()
                .into_iter()
                .flat_map(|object| object.keys()),
        )
        .filter(|key| !ignored.contains(key.as_str()))
        .cloned()
        .collect();
    for key in keys {
        let before = old.get(&key);
        let after = current.get(&key);
        if before != after {
            match (before, after) {
                (None, Some(value)) => {
                    details.push(format!("added field `{key}` = {}", display_json(value)))
                }
                (Some(_), None) => details.push(format!("removed field `{key}`")),
                (Some(before), Some(after)) if before.is_string() && after.is_string() => details
                    .push(format!(
                        "changed field `{key}` from {} to {}",
                        display_json(before),
                        display_json(after)
                    )),
                _ => details.push(format!("changed field `{key}`")),
            }
        }
    }

    if details.is_empty() {
        details.push("changed package manifest".to_string());
    }
    Ok(details)
}

fn display_json(value: &Value) -> String {
    let value = match value {
        Value::String(value) => value.clone(),
        value => value.to_string(),
    };
    if value.chars().count() > 100 {
        format!("{}…", value.chars().take(99).collect::<String>())
    } else {
        value
    }
}

fn add_changed_symbols(
    runtime_seeds: &mut BTreeSet<Node>,
    type_seeds: &mut BTreeSet<Node>,
    old_path: &Path,
    current_path: &Path,
    old: &ParsedModule,
    current: &ParsedModule,
) {
    for (name, current_symbol) in &current.symbols {
        let changed = old
            .symbols
            .get(name)
            .is_none_or(|old_symbol| old_symbol.fingerprint != current_symbol.fingerprint);
        if changed {
            type_seeds.insert(Node::new(current_path, name));
            if current_symbol.runtime {
                runtime_seeds.insert(Node::new(current_path, name));
            }
        }
    }

    for (name, old_symbol) in &old.symbols {
        if !current.symbols.contains_key(name) {
            type_seeds.insert(Node::new(old_path, name));
            if old_symbol.runtime {
                runtime_seeds.insert(Node::new(old_path, name));
            }
        }
    }
}

fn add_all_symbols(
    runtime_seeds: &mut BTreeSet<Node>,
    type_seeds: &mut BTreeSet<Node>,
    path: &Path,
    module: &ParsedModule,
) {
    for (name, symbol) in &module.symbols {
        type_seeds.insert(Node::new(path, name));
        if symbol.runtime {
            runtime_seeds.insert(Node::new(path, name));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::parser::MODULE_INIT;
    use crate::workspace::Target;

    #[test]
    fn seeds_only_changed_declaration() {
        let old = parse_module(
            Path::new("shared.ts"),
            "export const used = 1; export const untouched = 2;",
        )
        .unwrap();
        let current = parse_module(
            Path::new("shared.ts"),
            "export const used = 3; export const untouched = 2;",
        )
        .unwrap();
        let mut seeds = BTreeSet::new();
        let mut type_seeds = BTreeSet::new();
        add_changed_symbols(
            &mut seeds,
            &mut type_seeds,
            Path::new("shared.ts"),
            Path::new("shared.ts"),
            &old,
            &current,
        );

        assert!(seeds.contains(&Node::new("shared.ts", "used")));
        assert!(!seeds.contains(&Node::new("shared.ts", "untouched")));
    }

    #[test]
    fn seeds_imported_non_source_inputs() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        let stylesheet = root.path().join("style.css");
        fs::write(&app, "import './style.css'; export const value = true;").unwrap();
        fs::write(&stylesheet, "body { color: red; }").unwrap();
        let target = Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        };
        let graph = DependencyGraph::build(&[app], &[target], &[]).unwrap();
        let changes = vec![ChangedFile {
            path: stylesheet.clone(),
            kind: ChangeKind::Modified,
        }];

        let seeds = find_change_seeds(root.path(), "HEAD", &changes, &[], &graph, None).unwrap();

        assert!(
            seeds
                .nodes
                .contains(&Node::new(normalize_path(stylesheet), MODULE_INIT))
        );
    }

    #[test]
    fn package_manifest_changes_seed_library_symbols() {
        let root = tempdir().unwrap();
        let shared_dir = root.path().join("shared");
        fs::create_dir_all(&shared_dir).unwrap();
        let shared_dir = normalize_path(shared_dir);
        let shared = shared_dir.join("index.ts");
        let manifest = shared_dir.join("package.json");
        fs::write(&shared, "export const value = true;").unwrap();
        fs::write(&manifest, r#"{"name":"shared"}"#).unwrap();
        let package = Package {
            name: "shared".to_string(),
            dir: shared_dir,
            scripts: BTreeMap::new(),
            entrypoint: Some(shared.clone()),
            exports: BTreeMap::new(),
            dependencies: BTreeSet::new(),
        };
        let graph = DependencyGraph::build(
            std::slice::from_ref(&shared),
            &[],
            std::slice::from_ref(&package),
        )
        .unwrap();
        let changes = vec![ChangedFile {
            path: manifest,
            kind: ChangeKind::Added,
        }];

        let seeds =
            find_change_seeds(root.path(), "HEAD", &changes, &[package], &graph, None).unwrap();

        assert!(
            seeds
                .nodes
                .contains(&Node::new(normalize_path(shared), "value"))
        );
    }

    #[test]
    fn deleted_unowned_manifest_affects_every_target() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        let deleted_manifest = root.path().join("removed/package.json");
        fs::write(&app, "export const value = true;").unwrap();
        let target = Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        };
        let graph = DependencyGraph::build(&[app], &[target], &[]).unwrap();
        let changes = vec![ChangedFile {
            path: deleted_manifest,
            kind: ChangeKind::Deleted,
        }];

        let seeds = find_change_seeds(root.path(), "HEAD", &changes, &[], &graph, None).unwrap();

        assert!(seeds.direct_packages.contains_key("app"));
    }

    #[test]
    fn root_lockfile_changes_affect_every_target() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        let lockfile = root.path().join("pnpm-lock.yaml");
        fs::write(&app, "export const value = true;").unwrap();
        fs::write(&lockfile, "lockfileVersion: '9.0'").unwrap();
        let target = Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        };
        let graph = DependencyGraph::build(&[app], &[target], &[]).unwrap();
        let changes = vec![ChangedFile {
            path: lockfile,
            kind: ChangeKind::Added,
        }];

        let seeds = find_change_seeds(root.path(), "HEAD", &changes, &[], &graph, None).unwrap();

        assert!(seeds.direct_packages.contains_key("app"));
    }
}
