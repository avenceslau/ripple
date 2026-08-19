use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_yaml::Value as YamlValue;

use crate::git::{ChangeKind, ChangedFile, file_at};
use crate::graph::normalize_path;
use crate::package_manager::{LockfileImpact, PackageManager};
use crate::workspace::Package;

pub struct Pnpm;

impl PackageManager for Pnpm {
    fn lockfile_names(&self) -> &'static [&'static str] {
        &["pnpm-lock.yaml"]
    }

    fn modeled_detail(&self) -> &'static str {
        "changed pnpm dependency resolution"
    }

    fn lockfile_impact(
        &self,
        root: &Path,
        base: &str,
        change: &ChangedFile,
        packages: &[Package],
    ) -> Result<LockfileImpact> {
        pnpm_lockfile_impact_for_change(root, base, change, packages)
    }
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

fn pnpm_lockfile_impact_for_change(
    root: &Path,
    base: &str,
    change: &ChangedFile,
    packages: &[Package],
) -> Result<LockfileImpact> {
    let old_relative = match &change.kind {
        ChangeKind::Renamed { old_path } => old_path.as_path(),
        _ => change.path.strip_prefix(root).unwrap_or(&change.path),
    };
    let old = file_at(root, base, &root.join(old_relative))?;
    let current = fs::read_to_string(&change.path).ok();
    let (Some(old), Some(current)) = (old, current) else {
        return Ok(LockfileImpact::Fallback(
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
) -> LockfileImpact {
    let old = match serde_yaml::from_str::<PnpmLockfile>(old) {
        Ok(lockfile) => lockfile,
        Err(error) => {
            return LockfileImpact::Fallback(format!(
                "the base pnpm lockfile could not be parsed: {error}"
            ));
        }
    };
    let current = match serde_yaml::from_str::<PnpmLockfile>(current) {
        Ok(lockfile) => lockfile,
        Err(error) => {
            return LockfileImpact::Fallback(format!(
                "the current pnpm lockfile could not be parsed: {error}"
            ));
        }
    };

    if !is_pnpm_v9(&old.lockfile_version) || !is_pnpm_v9(&current.lockfile_version) {
        return LockfileImpact::Fallback("only pnpm lockfile version 9 is modeled".to_string());
    }
    if old.global != current.global {
        return LockfileImpact::Fallback(
            "pnpm root settings, overrides, patches, catalogs, or other global data changed"
                .to_string(),
        );
    }
    if old.importers.is_empty() || current.importers.is_empty() {
        return LockfileImpact::Fallback(
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
            return LockfileImpact::Fallback(format!(
                "unmodeled importer metadata changed for `{importer_name}`"
            ));
        }

        let old_resolution = match effective_pnpm_resolution(&old, &old_importer) {
            Ok(resolution) => resolution,
            Err(reason) => return LockfileImpact::Fallback(reason),
        };
        let current_resolution = match effective_pnpm_resolution(&current, &current_importer) {
            Ok(resolution) => resolution,
            Err(reason) => return LockfileImpact::Fallback(reason),
        };
        if old_resolution == current_resolution {
            continue;
        }
        if importer_name == "." {
            return LockfileImpact::Fallback(
                "the root pnpm importer dependency resolution changed".to_string(),
            );
        }
        let Some(package_name) = package_by_importer.get(&importer_name) else {
            return LockfileImpact::Fallback(format!(
                "changed importer `{importer_name}` does not map to a current workspace package"
            ));
        };
        changed_packages.insert(package_name.clone());
    }

    LockfileImpact::Modeled(changed_packages)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;
    use crate::analysis::{ChangeSeeds, seed_package_input};
    use crate::graph::DependencyGraph;
    use crate::workspace::Target;

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

    fn modeled_packages(impact: LockfileImpact) -> BTreeSet<String> {
        match impact {
            LockfileImpact::Modeled(packages) => packages,
            LockfileImpact::Fallback(reason) => panic!("unexpected fallback: {reason}"),
        }
    }

    fn assert_fallback(impact: LockfileImpact) {
        assert!(matches!(impact, LockfileImpact::Fallback(_)));
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
}
