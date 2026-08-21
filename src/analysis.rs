use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::diagnostics::{Diagnostic, Severity};
use crate::git::{ChangeKind, ChangedFile, file_at};
use crate::graph::{DependencyGraph, Node, normalize_path};
use crate::package_manager::{LockfileImpact, package_manager_for};
use crate::parser::{
    ParsedModule, is_source_file, is_test_file, parse_module, registry_entry_symbol,
};
use crate::workspace::{Package, package_for_path};

#[derive(Clone, Debug, Default)]
pub struct ChangeSeeds {
    pub nodes: BTreeSet<Node>,
    pub type_nodes: BTreeSet<Node>,
    pub direct_packages: BTreeMap<String, BTreeMap<PathBuf, Vec<String>>>,
    pub diagnostics: Vec<Diagnostic>,
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
            let is_root_lockfile = change.path.parent() == Some(root);
            if is_root_lockfile && let Some(manager) = package_manager_for(file_name) {
                match manager.lockfile_impact(root, base, change, packages)? {
                    LockfileImpact::Modeled(package_names) => {
                        for package_name in package_names {
                            let Some(package) =
                                packages.iter().find(|package| package.name == package_name)
                            else {
                                apply_lockfile_fallback(
                                    &mut result,
                                    graph,
                                    change,
                                    "a changed importer does not map to a current workspace package",
                                );
                                break;
                            };
                            seed_package_input(
                                &mut result,
                                graph,
                                packages,
                                package,
                                &change.path,
                                vec![manager.modeled_detail().to_string()],
                            );
                        }
                    }
                    LockfileImpact::Fallback(reason) => {
                        apply_lockfile_fallback(&mut result, graph, change, &reason);
                    }
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
                    "package.json"
                        | "wrangler.json"
                        | "wrangler.jsonc"
                        | "tsconfig.json"
                        | "Cargo.toml"
                        | "build.rs"
                ) || file_name.starts_with("vite.config.");

                if is_rust_source || is_build_input {
                    let details = describe_direct_input(root, base, change)?;
                    seed_package_input(
                        &mut result,
                        graph,
                        packages,
                        package,
                        &change.path,
                        details,
                    );
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
            let is_runtime_source = !is_test_file(relative)
                && (relative
                    .components()
                    .any(|component| component.as_os_str() == "src")
                    || package.entrypoint.as_ref() == Some(&change.path));
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

pub(crate) fn seed_package_input(
    result: &mut ChangeSeeds,
    graph: &DependencyGraph,
    packages: &[Package],
    package: &Package,
    path: &Path,
    details: Vec<String>,
) {
    result
        .direct_packages
        .entry(package.name.clone())
        .or_default()
        .insert(path.to_path_buf(), details);
    for (path, module) in graph.modules.iter().filter(|(path, _)| {
        package_for_path(packages, path).is_some_and(|owner| owner.dir == package.dir)
    }) {
        add_all_symbols(&mut result.nodes, &mut result.type_nodes, path, module);
    }
}

fn apply_lockfile_fallback(
    result: &mut ChangeSeeds,
    graph: &DependencyGraph,
    change: &ChangedFile,
    reason: &str,
) {
    let details = vec!["changed lockfile with unmodeled impact".to_string()];
    for target in &graph.targets {
        result
            .direct_packages
            .entry(target.package.clone())
            .or_default()
            .insert(change.path.clone(), details.clone());
    }
    result.diagnostics.push(Diagnostic {
        code: "MONORIPPLE_LOCKFILE_CHANGE_UNMODELED",
        severity: Severity::Warning,
        message: format!("lockfile changes conservatively affect every target because {reason}"),
        path: Some(change.path.clone()),
        members: Vec::new(),
    });
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
        let old_symbol = old.symbols.get(name);
        let registries = old.registries.get(name).zip(current.registries.get(name));
        let runtime_changed = old_symbol
            .is_none_or(|old_symbol| old_symbol.fingerprint != current_symbol.fingerprint);
        let type_changed = registries.map_or(runtime_changed, |(old, current)| {
            old.full_fingerprint != current.full_fingerprint
        });
        if type_changed {
            type_seeds.insert(Node::new(current_path, name));
        }
        if runtime_changed && current_symbol.runtime {
            runtime_seeds.insert(Node::new(current_path, name));
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

    let registry_names: BTreeSet<_> = old
        .registries
        .keys()
        .chain(current.registries.keys())
        .cloned()
        .collect();
    for name in registry_names {
        match (old.registries.get(&name), current.registries.get(&name)) {
            (Some(old_registry), Some(current_registry)) => {
                let old_common_order: Vec<_> = old_registry
                    .entry_order
                    .iter()
                    .filter(|key| current_registry.entries.contains_key(*key))
                    .collect();
                let current_common_order: Vec<_> = current_registry
                    .entry_order
                    .iter()
                    .filter(|key| old_registry.entries.contains_key(*key))
                    .collect();
                if old_common_order != current_common_order {
                    runtime_seeds.insert(Node::new(current_path, &name));
                }

                for (key, entry) in &current_registry.entries {
                    let changed = old_registry
                        .entries
                        .get(key)
                        .is_none_or(|old_entry| old_entry.fingerprint != entry.fingerprint);
                    if changed {
                        runtime_seeds
                            .insert(Node::new(current_path, registry_entry_symbol(&name, key)));
                    }
                }
                for key in old_registry.entries.keys() {
                    if !current_registry.entries.contains_key(key) {
                        runtime_seeds
                            .insert(Node::new(old_path, registry_entry_symbol(&name, key)));
                    }
                }
            }
            (None, Some(registry)) => {
                add_registry_entries(runtime_seeds, current_path, &name, registry.entries.keys());
            }
            (Some(registry), None) => {
                add_registry_entries(runtime_seeds, old_path, &name, registry.entries.keys());
            }
            (None, None) => {}
        }
    }
}

fn add_registry_entries<'a>(
    seeds: &mut BTreeSet<Node>,
    path: &Path,
    registry: &str,
    keys: impl Iterator<Item = &'a String>,
) {
    seeds.extend(keys.map(|key| Node::new(path, registry_entry_symbol(registry, key))));
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
    for (name, registry) in &module.registries {
        add_registry_entries(runtime_seeds, path, name, registry.entries.keys());
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

    fn registry_impact(
        old_registry: &str,
        current_registry: &str,
        consumers: &[(&str, &str)],
    ) -> BTreeSet<String> {
        let root = tempdir().unwrap();
        let registry = root.path().join("registry.ts");
        fs::write(&registry, current_registry).unwrap();
        let registry = normalize_path(registry);

        let mut files = vec![registry.clone()];
        let mut targets = Vec::new();
        for (name, source) in consumers {
            let path = root.path().join(format!("{name}.ts"));
            fs::write(&path, source).unwrap();
            files.push(path.clone());
            targets.push(Target {
                package: (*name).to_string(),
                entrypoint: path,
            });
        }
        let graph = DependencyGraph::build(&files, &targets, &[]).unwrap();
        let old = parse_module(&registry, old_registry).unwrap();
        let current = parse_module(&registry, current_registry).unwrap();
        let mut seeds = BTreeSet::new();
        let mut type_seeds = BTreeSet::new();
        add_changed_symbols(
            &mut seeds,
            &mut type_seeds,
            &registry,
            &registry,
            &old,
            &current,
        );
        let reached = graph.affected(&seeds).reached;

        targets
            .iter()
            .filter(|target| reached.contains(&DependencyGraph::target_node(&target.package)))
            .map(|target| target.package.clone())
            .collect()
    }

    #[test]
    fn additive_registry_entry_only_reaches_its_literal_consumer() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, added: 3 };",
            &[
                (
                    "alpha",
                    "import { registry } from './registry'; const result = registry.alpha; export default result;",
                ),
                (
                    "beta",
                    "import { registry } from './registry'; const result = registry['beta']; export default result;",
                ),
                (
                    "added",
                    "import { registry } from './registry'; const result = registry.added; export default result;",
                ),
            ],
        );

        assert_eq!(affected, BTreeSet::from(["added".to_string()]));
    }

    #[test]
    fn dynamic_registry_access_remains_broad() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, added: 3 };",
            &[(
                "dynamic",
                "import { registry } from './registry'; const key = process.env.KEY!; const result = registry[key]; export default result;",
            )],
        );

        assert_eq!(affected, BTreeSet::from(["dynamic".to_string()]));
    }

    #[test]
    fn registry_enumeration_remains_broad() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, added: 3 };",
            &[(
                "enumerates",
                "import { registry } from './registry'; const result = Object.keys(registry); export default result;",
            )],
        );

        assert_eq!(affected, BTreeSet::from(["enumerates".to_string()]));
    }

    #[test]
    fn registry_reordering_affects_enumeration_but_not_literal_access() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { beta: 2, alpha: 1 };",
            &[
                (
                    "alpha",
                    "import { registry } from './registry'; const result = registry.alpha; export default result;",
                ),
                (
                    "enumerates",
                    "import { registry } from './registry'; const result = Object.keys(registry); export default result;",
                ),
            ],
        );

        assert_eq!(affected, BTreeSet::from(["enumerates".to_string()]));
    }

    #[test]
    fn registry_mutation_and_escape_remain_broad() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, added: 3 };",
            &[
                (
                    "mutates",
                    "import { registry } from './registry'; registry.alpha = 4; export default registry.alpha;",
                ),
                (
                    "escapes",
                    "import { registry } from './registry'; const escaped = registry; export default escaped;",
                ),
            ],
        );

        assert_eq!(
            affected,
            BTreeSet::from(["escapes".to_string(), "mutates".to_string()])
        );
    }

    #[test]
    fn non_literal_forwarding_remains_broad() {
        let affected = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, added: 3 };",
            &[(
                "wrapper",
                "import { registry } from './registry'; function read(key: string) { return registry[key]; } const result = read('added'); export default result;",
            )],
        );

        assert_eq!(affected, BTreeSet::from(["wrapper".to_string()]));
    }

    #[test]
    fn effectful_or_computed_registry_entries_fall_back_to_the_whole_registry() {
        let effectful = registry_impact(
            "declare function create(): number; export const registry = { alpha: 1, beta: 2 };",
            "declare function create(): number; export const registry = { alpha: 1, beta: 2, added: create() };",
            &[(
                "alpha",
                "import { registry } from './registry'; const result = registry.alpha; export default result;",
            )],
        );
        let computed = registry_impact(
            "const added = 'added'; export const registry = { alpha: 1, beta: 2 };",
            "const added = 'added'; export const registry = { alpha: 1, beta: 2, [added]: 3 };",
            &[(
                "alpha",
                "import { registry } from './registry'; const result = registry.alpha; export default result;",
            )],
        );
        let duplicate = registry_impact(
            "export const registry = { alpha: 1, beta: 2 };",
            "export const registry = { alpha: 1, beta: 2, alpha: 3 };",
            &[(
                "beta",
                "import { registry } from './registry'; const result = registry.beta; export default result;",
            )],
        );
        let function_valued = registry_impact(
            "export const registry = { alpha: () => 1 };",
            "export const registry = { alpha: () => 1, added: () => registry.alpha() };",
            &[(
                "alpha",
                "import { registry } from './registry'; const result = registry.alpha(); export default result;",
            )],
        );

        assert_eq!(effectful, BTreeSet::from(["alpha".to_string()]));
        assert_eq!(computed, BTreeSet::from(["alpha".to_string()]));
        assert_eq!(duplicate, BTreeSet::from(["beta".to_string()]));
        assert_eq!(function_valued, BTreeSet::from(["alpha".to_string()]));
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

    fn test_package(root: &Path, name: &str, importer: &str) -> Package {
        let dir = root.join(importer);
        fs::create_dir_all(&dir).unwrap();
        Package {
            name: name.to_string(),
            dir: normalize_path(dir),
            scripts: BTreeMap::new(),
            entrypoint: None,
            exports: BTreeMap::new(),
            dependencies: BTreeSet::new(),
        }
    }

    #[test]
    fn package_input_seeding_excludes_nested_workspaces() {
        let root = tempdir().unwrap();
        let app_source = root.path().join("apps/app/index.ts");
        fs::create_dir_all(app_source.parent().unwrap()).unwrap();
        fs::write(&app_source, "export const value = true;").unwrap();

        let root_package = test_package(root.path(), "root", "");
        let mut app = test_package(root.path(), "app", "apps/app");
        app.entrypoint = Some(app_source.clone());
        let packages = [root_package.clone(), app.clone()];
        let target = Target {
            package: app.name.clone(),
            entrypoint: app_source.clone(),
        };
        let graph = DependencyGraph::build(&[app_source], &[target], &packages).unwrap();
        let mut seeds = ChangeSeeds::default();

        seed_package_input(
            &mut seeds,
            &graph,
            &packages,
            &root_package,
            &root.path().join("package.json"),
            vec!["changed package manifest".to_string()],
        );

        assert!(seeds.nodes.is_empty());
        assert!(seeds.type_nodes.is_empty());
        assert!(
            !graph
                .affected(&seeds.nodes)
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn added_pnpm_lockfile_falls_back_to_every_target_with_diagnostic() {
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
        assert_eq!(
            seeds.diagnostics[0].code,
            "MONORIPPLE_LOCKFILE_CHANGE_UNMODELED"
        );
    }
}
