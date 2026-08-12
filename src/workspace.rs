use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;

use crate::parser::is_source_file;

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub dir: PathBuf,
    pub scripts: BTreeMap<String, String>,
    pub entrypoint: Option<PathBuf>,
    pub exports: BTreeMap<String, PathBuf>,
    pub dependencies: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub package: String,
    pub entrypoint: PathBuf,
}

#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    main: Option<String>,
    module: Option<String>,
    source: Option<String>,
    exports: Option<Value>,
    workspaces: Option<WorkspaceConfig>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkspaceConfig {
    Patterns(Vec<String>),
    Object { packages: Vec<String> },
}

#[derive(Deserialize)]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Vec<String>,
}

pub fn discover_packages(root: &Path) -> Result<Vec<Package>> {
    let workspace_matchers = workspace_matchers(root)?;
    let mut packages = Vec::new();
    let mut package_names = BTreeMap::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | "target" | "dist" | "build")
            )
        })
        .build();

    for entry in walker {
        let entry = entry?;
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            && entry.file_name() == "package.json"
        {
            let dir = entry.path().parent().unwrap_or(root).to_path_buf();
            if dir != root
                && let Some(matchers) = &workspace_matchers
            {
                let relative = dir.strip_prefix(root).unwrap_or(&dir);
                let included = matchers
                    .iter()
                    .filter(|(excluded, _)| !excluded)
                    .any(|(_, matcher)| matcher.is_match(relative));
                let excluded = matchers
                    .iter()
                    .filter(|(excluded, _)| *excluded)
                    .any(|(_, matcher)| matcher.is_match(relative));
                if !included || excluded {
                    continue;
                }
            }

            let source = fs::read_to_string(entry.path())
                .with_context(|| format!("failed to read {}", entry.path().display()))?;
            let manifest: PackageJson = serde_json::from_str(&source)
                .with_context(|| format!("failed to parse {}", entry.path().display()))?;
            let name = manifest.name.unwrap_or_else(|| {
                dir.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("root")
                    .to_string()
            });
            if let Some(previous) = package_names.insert(name.clone(), dir.clone()) {
                bail!(
                    "duplicate workspace package name `{name}` in {} and {}",
                    previous.display(),
                    dir.display()
                );
            }

            let entrypoint = find_entrypoint(
                &dir,
                manifest.source.as_deref(),
                manifest.module.as_deref(),
                manifest.main.as_deref(),
                manifest.exports.as_ref(),
            );
            let exports = package_exports(&dir, manifest.exports.as_ref());
            let dependencies = manifest
                .dependencies
                .keys()
                .chain(manifest.dev_dependencies.keys())
                .chain(manifest.peer_dependencies.keys())
                .chain(manifest.optional_dependencies.keys())
                .cloned()
                .collect();
            packages.push(Package {
                name,
                dir,
                scripts: manifest.scripts,
                entrypoint,
                exports,
                dependencies,
            });
        }
    }

    packages.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(packages)
}

fn workspace_matchers(root: &Path) -> Result<Option<Vec<(bool, GlobMatcher)>>> {
    let root_manifest = root.join("package.json");
    let mut patterns = if root_manifest.is_file() {
        let source = fs::read_to_string(&root_manifest)
            .with_context(|| format!("failed to read {}", root_manifest.display()))?;
        let manifest: PackageJson = serde_json::from_str(&source)
            .with_context(|| format!("failed to parse {}", root_manifest.display()))?;
        manifest.workspaces.map(|workspaces| match workspaces {
            WorkspaceConfig::Patterns(patterns) => patterns,
            WorkspaceConfig::Object { packages } => packages,
        })
    } else {
        None
    };

    let pnpm_workspace = root.join("pnpm-workspace.yaml");
    if pnpm_workspace.is_file() {
        let source = fs::read_to_string(&pnpm_workspace)
            .with_context(|| format!("failed to read {}", pnpm_workspace.display()))?;
        let workspace: PnpmWorkspace = serde_yaml::from_str(&source)
            .with_context(|| format!("failed to parse {}", pnpm_workspace.display()))?;
        patterns.get_or_insert_default().extend(workspace.packages);
    }

    patterns
        .map(|patterns| {
            patterns
                .into_iter()
                .map(|pattern| {
                    let (excluded, pattern) = pattern
                        .strip_prefix('!')
                        .map_or((false, pattern.as_str()), |pattern| (true, pattern));
                    let matcher = GlobBuilder::new(pattern.trim_end_matches('/'))
                        .literal_separator(true)
                        .build()
                        .with_context(|| format!("invalid workspace pattern `{pattern}`"))?
                        .compile_matcher();
                    Ok((excluded, matcher))
                })
                .collect()
        })
        .transpose()
}

pub fn discover_source_files(root: &Path, packages: &[Package]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let root_path = root.to_path_buf();
    let package_dirs: BTreeSet<_> = packages.iter().map(|package| package.dir.clone()).collect();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(move |entry| {
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "node_modules" | "target" | "dist" | "build" | "tests" | "test")
            ) {
                return false;
            }

            entry.path() == root_path
                || !entry.path().join("package.json").is_file()
                || package_dirs.contains(entry.path())
        })
        .build();

    for entry in walker.flatten() {
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            && is_source_file(entry.path())
            && package_for_path(packages, entry.path()).is_some()
        {
            files.push(entry.into_path());
        }
    }

    files.sort();
    files
}

pub fn targets_for(packages: &[Package], script: &str) -> Vec<Target> {
    packages
        .iter()
        .filter(|package| script == "all" || package.scripts.contains_key(script))
        .filter_map(|package| {
            package.entrypoint.as_ref().map(|entrypoint| Target {
                package: package.name.clone(),
                entrypoint: entrypoint.clone(),
            })
        })
        .collect()
}

pub fn package_for_path<'a>(packages: &'a [Package], path: &Path) -> Option<&'a Package> {
    let package = packages
        .iter()
        .filter(|package| path.starts_with(&package.dir))
        .max_by_key(|package| package.dir.components().count())?;
    let manifest_owner = path
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|directory| directory.join("package.json").is_file());

    manifest_owner
        .is_none_or(|directory| directory == package.dir)
        .then_some(package)
}

fn find_entrypoint(
    dir: &Path,
    source: Option<&str>,
    module: Option<&str>,
    main: Option<&str>,
    exports: Option<&Value>,
) -> Option<PathBuf> {
    if let Some(path) = wrangler_entrypoint(dir) {
        return Some(path);
    }

    let export_entry = exports.and_then(root_export);
    let declared = [source, module, main, export_entry]
        .into_iter()
        .flatten()
        .map(|candidate| dir.join(candidate.trim_start_matches("./")))
        .find(|candidate| candidate.is_file());

    declared.or_else(|| {
        [
            "src/index.ts",
            "src/index.tsx",
            "index.ts",
            "worker.ts",
            "src/worker.ts",
            "src/main.ts",
        ]
        .into_iter()
        .map(|candidate| dir.join(candidate))
        .find(|candidate| candidate.is_file())
    })
}

fn wrangler_entrypoint(dir: &Path) -> Option<PathBuf> {
    ["wrangler.jsonc", "wrangler.json"]
        .into_iter()
        .map(|name| dir.join(name))
        .find_map(|path| {
            let source = fs::read_to_string(path).ok()?;
            let value: Value = json5::from_str(&source).ok()?;
            let main = value.get("main")?.as_str()?;
            Some(dir.join(main))
        })
}

fn root_export(exports: &Value) -> Option<&str> {
    match exports {
        Value::Object(map) if map.contains_key(".") => export_path(&map["."]),
        value => export_path(value),
    }
}

fn package_exports(dir: &Path, exports: Option<&Value>) -> BTreeMap<String, PathBuf> {
    let mut result = BTreeMap::new();
    let Some(exports) = exports else {
        return result;
    };

    match exports {
        Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
            for (key, value) in map {
                if let Some(path) = export_path(value) {
                    result.insert(key.clone(), dir.join(path.trim_start_matches("./")));
                }
            }
        }
        value => {
            if let Some(path) = export_path(value) {
                result.insert(".".to_string(), dir.join(path.trim_start_matches("./")));
            }
        }
    }

    result
}

fn export_path(value: &Value) -> Option<&str> {
    match value {
        Value::String(path) => Some(path),
        Value::Object(conditions) => [
            "source", "worker", "workerd", "browser", "import", "module", "default", "types",
            "require",
        ]
        .into_iter()
        .find_map(|condition| conditions.get(condition).and_then(export_path)),
        Value::Array(options) => options.iter().find_map(export_path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_wrangler_target() {
        let root = tempdir().unwrap();
        let app = root.path().join("apps/worker");
        fs::create_dir_all(app.join("src")).unwrap();
        fs::write(
            app.join("package.json"),
            r#"{"name":"worker","scripts":{"deploy":"wrangler deploy"}}"#,
        )
        .unwrap();
        fs::write(
            app.join("wrangler.jsonc"),
            "{ // worker config\n main: 'src/index.ts', }",
        )
        .unwrap();
        fs::write(app.join("src/index.ts"), "export default {};").unwrap();

        let packages = discover_packages(root.path()).unwrap();
        let targets = targets_for(&packages, "deploy");
        assert_eq!(targets[0].package, "worker");
        assert!(targets[0].entrypoint.ends_with("apps/worker/src/index.ts"));
    }

    #[test]
    fn honors_package_json_workspace_patterns() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("apps/web/src")).unwrap();
        fs::create_dir_all(root.path().join("fixtures/example/src")).unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"root","workspaces":["apps/*"]}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("apps/web/package.json"),
            r#"{"name":"web"}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("fixtures/example/package.json"),
            r#"{"name":"example"}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("apps/web/src/index.ts"),
            "export const web = true;",
        )
        .unwrap();
        fs::write(
            root.path().join("fixtures/example/src/index.ts"),
            "export const fixture = true;",
        )
        .unwrap();

        let packages = discover_packages(root.path()).unwrap();
        let names: Vec<_> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();

        assert_eq!(names, vec!["root", "web"]);
        let files = discover_source_files(root.path(), &packages);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("apps/web/src/index.ts"));
        assert!(
            package_for_path(
                &packages,
                &root.path().join("fixtures/example/src/index.ts")
            )
            .is_none()
        );
    }

    #[test]
    fn honors_pnpm_workspace_exclusions() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("packages/included")).unwrap();
        fs::create_dir_all(root.path().join("packages/excluded")).unwrap();
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/excluded'\n",
        )
        .unwrap();
        fs::write(
            root.path().join("packages/included/package.json"),
            r#"{"name":"included"}"#,
        )
        .unwrap();
        fs::write(
            root.path().join("packages/excluded/package.json"),
            r#"{"name":"excluded"}"#,
        )
        .unwrap();

        let packages = discover_packages(root.path()).unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "included");
    }

    #[test]
    fn rejects_duplicate_workspace_package_names() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("packages/one")).unwrap();
        fs::create_dir_all(root.path().join("packages/two")).unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        for package in ["one", "two"] {
            fs::write(
                root.path().join(format!("packages/{package}/package.json")),
                r#"{"name":"duplicate"}"#,
            )
            .unwrap();
        }

        let error = discover_packages(root.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("duplicate workspace package name")
        );
    }

    #[test]
    fn selects_source_conditions_for_wildcard_exports() {
        let dir = Path::new("package");
        let exports = serde_json::json!({
            "./*": {
                "worker": "./src/*.ts",
                "default": "./dist/*.js"
            }
        });

        let exports = package_exports(dir, Some(&exports));

        assert_eq!(exports["./*"], Path::new("package/src/*.ts"));
    }
}
