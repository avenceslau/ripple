use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_yaml::Value as YamlValue;

use crate::diagnostics::{Diagnostic, Severity};
use crate::git::{ChangeKind, ChangedFile, file_at};
use crate::graph::{DependencyGraph, Node, normalize_path};
use crate::parser::{ParsedModule, is_source_file, is_test_file, parse_module};
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
            let is_root_js_lockfile = change.path.parent() == Some(root)
                && matches!(
                    file_name,
                    "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "bun.lock" | "bun.lockb"
                );
            if is_root_js_lockfile {
                if file_name == "pnpm-lock.yaml" {
                    match pnpm_lockfile_impact_for_change(root, base, change, packages)? {
                        PnpmLockfileImpact::Modeled(package_names) => {
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
                                    vec!["changed pnpm dependency resolution".to_string()],
                                );
                            }
                        }
                        PnpmLockfileImpact::Fallback(reason) => {
                            apply_lockfile_fallback(&mut result, graph, change, &reason);
                        }
                    }
                } else {
                    apply_lockfile_fallback(
                        &mut result,
                        graph,
                        change,
                        "exact runtime consumers are not modeled for this lockfile format",
                    );
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PnpmLockfile {
    lockfile_version: YamlValue,
    #[serde(default)]
    importers: BTreeMap<String, PnpmImporter>,
    #[serde(default)]
    packages: BTreeMap<String, YamlValue>,
    #[serde(default)]
    snapshots: BTreeMap<String, PnpmSnapshot>,
    #[serde(flatten)]
    global: BTreeMap<String, YamlValue>,
}

#[derive(Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PnpmImporter {
    #[serde(default)]
    dependencies: BTreeMap<String, YamlValue>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, YamlValue>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, YamlValue>,
    #[serde(flatten)]
    other: BTreeMap<String, YamlValue>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PnpmSnapshot {
    #[serde(default)]
    dependencies: BTreeMap<String, YamlValue>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, YamlValue>,
    #[serde(flatten)]
    metadata: BTreeMap<String, YamlValue>,
}

#[derive(PartialEq)]
struct EffectivePnpmResolution {
    direct: BTreeMap<String, String>,
    packages: BTreeMap<String, YamlValue>,
    snapshots: BTreeMap<String, PnpmSnapshot>,
}

enum PnpmLockfileImpact {
    Modeled(BTreeSet<String>),
    Fallback(String),
}

fn pnpm_lockfile_impact_for_change(
    root: &Path,
    base: &str,
    change: &ChangedFile,
    packages: &[Package],
) -> Result<PnpmLockfileImpact> {
    let old_relative = match &change.kind {
        ChangeKind::Renamed { old_path } => old_path.as_path(),
        _ => change.path.strip_prefix(root).unwrap_or(&change.path),
    };
    let old = file_at(root, base, &root.join(old_relative))?;
    let current = fs::read_to_string(&change.path).ok();
    let (Some(old), Some(current)) = (old, current) else {
        return Ok(PnpmLockfileImpact::Fallback(
            "the pnpm lockfile was added, deleted, or could not be read at both revisions"
                .to_string(),
        ));
    };

    Ok(pnpm_lockfile_impact(root, &old, &current, packages))
}

fn pnpm_lockfile_impact(
    root: &Path,
    old: &str,
    current: &str,
    packages: &[Package],
) -> PnpmLockfileImpact {
    let old = match serde_yaml::from_str::<PnpmLockfile>(old) {
        Ok(lockfile) => lockfile,
        Err(error) => {
            return PnpmLockfileImpact::Fallback(format!(
                "the base pnpm lockfile could not be parsed: {error}"
            ));
        }
    };
    let current = match serde_yaml::from_str::<PnpmLockfile>(current) {
        Ok(lockfile) => lockfile,
        Err(error) => {
            return PnpmLockfileImpact::Fallback(format!(
                "the current pnpm lockfile could not be parsed: {error}"
            ));
        }
    };

    if !is_pnpm_v9(&old.lockfile_version) || !is_pnpm_v9(&current.lockfile_version) {
        return PnpmLockfileImpact::Fallback("only pnpm lockfile version 9 is modeled".to_string());
    }
    if old.global != current.global {
        return PnpmLockfileImpact::Fallback(
            "pnpm root settings, overrides, patches, catalogs, or other global data changed"
                .to_string(),
        );
    }
    if old.importers.is_empty() || current.importers.is_empty() {
        return PnpmLockfileImpact::Fallback(
            "the pnpm lockfile does not contain v9 importer data".to_string(),
        );
    }

    let package_by_importer = package_importers(root, packages);
    let importer_names: BTreeSet<_> = old
        .importers
        .keys()
        .chain(current.importers.keys())
        .cloned()
        .collect();
    let mut changed_packages = BTreeSet::new();

    for importer_name in importer_names {
        let old_importer = old
            .importers
            .get(&importer_name)
            .cloned()
            .unwrap_or_default();
        let current_importer = current
            .importers
            .get(&importer_name)
            .cloned()
            .unwrap_or_default();
        if old_importer.other != current_importer.other {
            return PnpmLockfileImpact::Fallback(format!(
                "unmodeled importer metadata changed for `{importer_name}`"
            ));
        }

        let old_resolution = match effective_pnpm_resolution(&old, &old_importer) {
            Ok(resolution) => resolution,
            Err(reason) => return PnpmLockfileImpact::Fallback(reason),
        };
        let current_resolution = match effective_pnpm_resolution(&current, &current_importer) {
            Ok(resolution) => resolution,
            Err(reason) => return PnpmLockfileImpact::Fallback(reason),
        };
        if old_resolution == current_resolution {
            continue;
        }
        if importer_name == "." {
            return PnpmLockfileImpact::Fallback(
                "the root pnpm importer dependency resolution changed".to_string(),
            );
        }
        let Some(package_name) = package_by_importer.get(&importer_name) else {
            return PnpmLockfileImpact::Fallback(format!(
                "changed importer `{importer_name}` does not map to a current workspace package"
            ));
        };
        changed_packages.insert(package_name.clone());
    }

    PnpmLockfileImpact::Modeled(changed_packages)
}

fn is_pnpm_v9(version: &YamlValue) -> bool {
    version
        .as_str()
        .is_some_and(|version| version == "9" || version.starts_with("9."))
        || version.as_u64() == Some(9)
        || version
            .as_f64()
            .is_some_and(|version| (9.0..10.0).contains(&version))
}

fn package_importers(root: &Path, packages: &[Package]) -> BTreeMap<String, String> {
    let root = normalize_path(root);
    packages
        .iter()
        .filter_map(|package| {
            let relative = normalize_path(&package.dir)
                .strip_prefix(&root)
                .ok()?
                .to_path_buf();
            let importer = if relative.as_os_str().is_empty() {
                ".".to_string()
            } else {
                relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            };
            Some((importer, package.name.clone()))
        })
        .collect()
}

fn effective_pnpm_resolution(
    lockfile: &PnpmLockfile,
    importer: &PnpmImporter,
) -> std::result::Result<EffectivePnpmResolution, String> {
    let mut direct = BTreeMap::new();
    for dependencies in [
        &importer.dependencies,
        &importer.dev_dependencies,
        &importer.optional_dependencies,
    ] {
        for (name, value) in dependencies {
            let reference = pnpm_dependency_reference(value)?;
            if let Some(previous) = direct.insert(name.clone(), reference.clone())
                && previous != reference
            {
                return Err(format!(
                    "importer resolves `{name}` to conflicting dependency versions"
                ));
            }
        }
    }

    let mut packages = BTreeMap::new();
    let mut snapshots = BTreeMap::new();
    let mut pending: Vec<_> = direct
        .iter()
        .map(|(name, reference)| (name.clone(), reference.clone()))
        .collect();
    let mut visited = BTreeSet::new();

    while let Some((name, reference)) = pending.pop() {
        let Some(key) = pnpm_resolution_key(lockfile, &name, &reference)? else {
            continue;
        };
        if !visited.insert(key.clone()) {
            continue;
        }
        let package_key = key.split_once('(').map_or(key.as_str(), |(key, _)| key);
        if let Some(package) = lockfile.packages.get(package_key) {
            packages.insert(package_key.to_string(), package.clone());
        }
        if package_key != key
            && let Some(package) = lockfile.packages.get(&key)
        {
            packages.insert(key.clone(), package.clone());
        }
        if let Some(snapshot) = lockfile.snapshots.get(&key) {
            snapshots.insert(key.clone(), snapshot.clone());
            for dependencies in [&snapshot.dependencies, &snapshot.optional_dependencies] {
                for (dependency, value) in dependencies {
                    pending.push((dependency.clone(), pnpm_dependency_reference(value)?));
                }
            }
        }
    }

    Ok(EffectivePnpmResolution {
        direct,
        packages,
        snapshots,
    })
}

fn pnpm_dependency_reference(value: &YamlValue) -> std::result::Result<String, String> {
    if let Some(reference) = value.as_str() {
        return Ok(reference.to_string());
    }
    let Some(mapping) = value.as_mapping() else {
        return Err("a pnpm dependency reference has an unsupported shape".to_string());
    };
    for key in mapping.keys() {
        let Some(key) = key.as_str() else {
            return Err("a pnpm dependency reference has a non-string field".to_string());
        };
        if !matches!(key, "specifier" | "version") {
            return Err(format!(
                "a pnpm dependency reference contains unmodeled field `{key}`"
            ));
        }
    }
    mapping
        .get("version")
        .and_then(YamlValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| "a pnpm dependency reference has no resolved version".to_string())
}

fn pnpm_resolution_key(
    lockfile: &PnpmLockfile,
    dependency_name: &str,
    reference: &str,
) -> std::result::Result<Option<String>, String> {
    if reference.starts_with("link:") || reference.starts_with("workspace:") {
        return Ok(None);
    }
    if reference.starts_with("file:") || reference.starts_with("portal:") {
        return Err(format!(
            "local dependency reference `{reference}` is not modeled"
        ));
    }

    let (name, version) = if let Some(alias) = reference.strip_prefix("npm:") {
        let Some(separator) = alias.rfind('@').filter(|separator| *separator > 0) else {
            return Err(format!("pnpm alias `{reference}` is ambiguous"));
        };
        (&alias[..separator], &alias[separator + 1..])
    } else {
        (dependency_name, reference)
    };
    let candidates = BTreeSet::from([reference.to_string(), format!("{name}@{version}")]);
    let mut matching = BTreeSet::new();
    for candidate in candidates {
        if lockfile.snapshots.contains_key(&candidate) {
            matching.insert(candidate);
            continue;
        }
        let peer_prefix = format!("{candidate}(");
        let peer_snapshots: Vec<_> = lockfile
            .snapshots
            .keys()
            .filter(|key| key.starts_with(&peer_prefix))
            .cloned()
            .collect();
        if !peer_snapshots.is_empty() {
            matching.extend(peer_snapshots);
        } else if lockfile.packages.contains_key(&candidate) {
            matching.insert(candidate);
        }
    }
    let matching: Vec<_> = matching.into_iter().collect();

    match matching.as_slice() {
        [key] => Ok(Some(key.clone())),
        [] => Err(format!(
            "could not resolve pnpm dependency `{dependency_name}` at `{reference}`"
        )),
        _ => Err(format!(
            "pnpm dependency `{dependency_name}` at `{reference}` resolves ambiguously"
        )),
    }
}

fn seed_package_input(
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

    fn modeled_packages(impact: PnpmLockfileImpact) -> BTreeSet<String> {
        match impact {
            PnpmLockfileImpact::Modeled(packages) => packages,
            PnpmLockfileImpact::Fallback(reason) => panic!("unexpected fallback: {reason}"),
        }
    }

    fn assert_fallback(impact: PnpmLockfileImpact) {
        assert!(matches!(impact, PnpmLockfileImpact::Fallback(_)));
    }

    #[test]
    fn pnpm_dependency_removal_is_scoped_to_its_importer() {
        let root = tempdir().unwrap();
        let packages = [
            test_package(root.path(), "app", "apps/app"),
            test_package(root.path(), "tools", "packages/tools"),
        ];
        let old = r#"
lockfileVersion: '9.0'
importers:
  apps/app: {}
  packages/tools:
    devDependencies:
      oxc-parser:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  oxc-parser@1.0.0:
    resolution: {integrity: old}
snapshots:
  oxc-parser@1.0.0: {}
"#;
        let current = r#"
lockfileVersion: '9.0'
importers:
  apps/app: {}
  packages/tools: {}
packages:
  oxc-parser@1.0.0:
    resolution: {integrity: old}
snapshots:
  oxc-parser@1.0.0: {}
"#;

        let changed = modeled_packages(pnpm_lockfile_impact(root.path(), old, current, &packages));

        assert_eq!(changed, BTreeSet::from(["tools".to_string()]));
    }

    #[test]
    fn pnpm_application_resolution_change_seeds_the_application() {
        let root = tempdir().unwrap();
        let app_dir = root.path().join("apps/app");
        fs::create_dir_all(&app_dir).unwrap();
        let app_source = app_dir.join("index.ts");
        fs::write(&app_source, "export const value = true;").unwrap();
        let mut app = test_package(root.path(), "app", "apps/app");
        app.entrypoint = Some(app_source.clone());
        let target = Target {
            package: "app".to_string(),
            entrypoint: app_source.clone(),
        };
        let graph = DependencyGraph::build(
            std::slice::from_ref(&app_source),
            &[target],
            std::slice::from_ref(&app),
        )
        .unwrap();
        let old = r#"
lockfileVersion: '9.0'
importers:
  apps/app:
    dependencies:
      external: {specifier: ^1.0.0, version: 1.0.0}
packages:
  external@1.0.0: {resolution: {integrity: one}}
  external@2.0.0: {resolution: {integrity: two}}
snapshots:
  external@1.0.0: {}
  external@2.0.0: {}
"#;
        let current = old.replace("version: 1.0.0", "version: 2.0.0");
        let changed = modeled_packages(pnpm_lockfile_impact(
            root.path(),
            old,
            &current,
            std::slice::from_ref(&app),
        ));
        let mut seeds = ChangeSeeds::default();
        for package_name in changed {
            assert_eq!(package_name, app.name);
            seed_package_input(
                &mut seeds,
                &graph,
                std::slice::from_ref(&app),
                &app,
                &root.path().join("pnpm-lock.yaml"),
                vec!["changed pnpm dependency resolution".to_string()],
            );
        }

        assert!(seeds.direct_packages.contains_key("app"));
    }

    #[test]
    fn pnpm_shared_library_resolution_change_reaches_runtime_consumer() {
        let root = tempdir().unwrap();
        let app_dir = root.path().join("apps/app");
        let shared_dir = root.path().join("packages/shared");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&shared_dir).unwrap();
        let app_source = app_dir.join("index.ts");
        let shared_source = shared_dir.join("index.ts");
        fs::write(
            &app_source,
            "import { value } from '@repo/shared'; export const result = value;",
        )
        .unwrap();
        fs::write(&shared_source, "export const value = true;").unwrap();
        let mut app = test_package(root.path(), "app", "apps/app");
        app.entrypoint = Some(app_source.clone());
        let mut shared = test_package(root.path(), "@repo/shared", "packages/shared");
        shared.entrypoint = Some(shared_source.clone());
        let packages = [app.clone(), shared.clone()];
        let target = Target {
            package: app.name.clone(),
            entrypoint: app_source.clone(),
        };
        let graph =
            DependencyGraph::build(&[app_source, shared_source], &[target], &packages).unwrap();
        let old = r#"
lockfileVersion: '9.0'
importers:
  apps/app:
    dependencies:
      '@repo/shared': {specifier: workspace:*, version: 'link:../../packages/shared'}
  packages/shared:
    dependencies:
      external: {specifier: ^1.0.0, version: 1.0.0}
packages:
  external@1.0.0: {resolution: {integrity: one}}
  external@2.0.0: {resolution: {integrity: two}}
snapshots:
  external@1.0.0: {}
  external@2.0.0: {}
"#;
        let current = old.replace("version: 1.0.0", "version: 2.0.0");
        let changed = modeled_packages(pnpm_lockfile_impact(root.path(), old, &current, &packages));
        assert_eq!(changed, BTreeSet::from([shared.name.clone()]));
        let mut seeds = ChangeSeeds::default();
        seed_package_input(
            &mut seeds,
            &graph,
            &packages,
            &shared,
            &root.path().join("pnpm-lock.yaml"),
            vec!["changed pnpm dependency resolution".to_string()],
        );

        let reached = graph.affected(&seeds.nodes);
        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
        assert!(!seeds.direct_packages.contains_key("app"));
    }

    #[test]
    fn pnpm_ignores_changes_outside_every_importer_resolution() {
        let root = tempdir().unwrap();
        let packages = [test_package(root.path(), "app", "apps/app")];
        let old = r#"
lockfileVersion: '9.0'
importers:
  apps/app:
    dependencies:
      used: {specifier: ^1.0.0, version: 1.0.0}
packages:
  used@1.0.0: {resolution: {integrity: used}}
  unused@1.0.0: {resolution: {integrity: old}}
snapshots:
  used@1.0.0: {}
  unused@1.0.0: {}
"#;
        let current = old.replace("integrity: old", "integrity: new");

        let changed = modeled_packages(pnpm_lockfile_impact(root.path(), old, &current, &packages));

        assert!(changed.is_empty());
    }

    #[test]
    fn malformed_and_unsupported_pnpm_lockfiles_fall_back() {
        let root = tempdir().unwrap();
        let packages = [test_package(root.path(), "app", "apps/app")];
        let valid = "lockfileVersion: '9.0'\nimporters:\n  apps/app: {}\n";
        assert_fallback(pnpm_lockfile_impact(
            root.path(),
            valid,
            "lockfileVersion: [",
            &packages,
        ));
        assert_fallback(pnpm_lockfile_impact(
            root.path(),
            "lockfileVersion: '8.0'\nimporters:\n  apps/app: {}\n",
            valid,
            &packages,
        ));
    }

    #[test]
    fn pnpm_global_and_ambiguous_changes_fall_back() {
        let root = tempdir().unwrap();
        let packages = [test_package(root.path(), "app", "apps/app")];
        let old = "lockfileVersion: '9.0'\nimporters:\n  apps/app: {}\noverrides: {}\n";
        let current =
            "lockfileVersion: '9.0'\nimporters:\n  apps/app: {}\noverrides:\n  dep: 2.0.0\n";
        assert_fallback(pnpm_lockfile_impact(root.path(), old, current, &packages));

        let ambiguous = r#"
lockfileVersion: '9.0'
importers:
  apps/app:
    dependencies:
      dep: {specifier: ^1.0.0, version: 1.0.0}
packages:
  1.0.0: {}
  dep@1.0.0: {}
snapshots: {}
"#;
        assert_fallback(pnpm_lockfile_impact(
            root.path(),
            ambiguous,
            ambiguous,
            &packages,
        ));
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
