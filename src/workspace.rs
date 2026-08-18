use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;

use crate::parser::{is_source_file, is_test_file};

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
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

pub fn discover_packages(root: &Path) -> Result<Vec<Package>> {
    let mut packages = Vec::new();
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

pub fn discover_source_files(
    root: &Path,
    packages: &[Package],
    include_tests: bool,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(move |entry| {
            let name = entry.file_name().to_str();
            let excluded = matches!(
                name,
                Some(".git" | "node_modules" | "target" | "dist" | "build")
            );
            let test_directory = matches!(name, Some("test" | "tests" | "__tests__"));
            !excluded && (include_tests || !test_directory)
        })
        .build();

    for entry in walker.flatten() {
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            || !is_source_file(entry.path())
        {
            continue;
        }
        let Some(package) = package_for_path(packages, entry.path()) else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(&package.dir)
            .unwrap_or(entry.path());
        if include_tests || !is_test_file(relative) {
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

pub fn test_roots_for(
    packages: &[Package],
    script: &str,
    files: &[PathBuf],
) -> BTreeMap<String, Vec<PathBuf>> {
    let selected: BTreeSet<_> = packages
        .iter()
        .filter(|package| package.scripts.contains_key(script))
        .map(|package| package.name.as_str())
        .collect();
    let mut roots: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();

    for file in files {
        let Some(package) = package_for_path(packages, file) else {
            continue;
        };
        let relative = file.strip_prefix(&package.dir).unwrap_or(file);
        if selected.contains(package.name.as_str()) && is_test_root(relative) {
            roots
                .entry(package.name.clone())
                .or_default()
                .insert(file.clone());
        }
    }

    let mut workers = BTreeMap::new();
    for package in packages {
        let Some(entrypoint) = &package.entrypoint else {
            continue;
        };
        let Some(config) = wrangler_config(&package.dir) else {
            continue;
        };
        for name in worker_names(&config) {
            workers.insert(name, (package.name.as_str(), entrypoint));
        }
    }

    for package in packages
        .iter()
        .filter(|package| selected.contains(package.name.as_str()))
    {
        let Some(config) = wrangler_config(&package.dir) else {
            continue;
        };
        for reference in worker_references(&config) {
            let Some((dependency, entrypoint)) = workers.get(&reference) else {
                continue;
            };
            if *dependency != package.name {
                roots
                    .entry(package.name.clone())
                    .or_default()
                    .insert((*entrypoint).clone());
            }
        }
    }

    roots
        .into_iter()
        .map(|(package, roots)| (package, roots.into_iter().collect()))
        .collect()
}

pub fn package_for_path<'a>(packages: &'a [Package], path: &Path) -> Option<&'a Package> {
    packages
        .iter()
        .filter(|package| path.starts_with(&package.dir))
        .max_by_key(|package| package.dir.components().count())
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
    let value = wrangler_config(dir)?;
    let main = value.get("main")?.as_str()?;
    Some(dir.join(main))
}

fn wrangler_config(dir: &Path) -> Option<Value> {
    ["wrangler.jsonc", "wrangler.json"]
        .into_iter()
        .map(|name| dir.join(name))
        .find_map(|path| {
            let source = fs::read_to_string(path).ok()?;
            json5::from_str(&source).ok()
        })
}

fn worker_names(config: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(name) = config.get("name").and_then(Value::as_str) {
        names.insert(name.to_string());
    }
    if let Some(environments) = config.get("env").and_then(Value::as_object) {
        names.extend(environments.values().filter_map(|environment| {
            environment
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        }));
    }
    names
}

fn worker_references(config: &Value) -> BTreeSet<String> {
    fn visit(value: &Value, references: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "service" | "script_name" | "scriptName")
                        && let Some(name) = value.as_str()
                    {
                        references.insert(name.to_string());
                    }
                    visit(value, references);
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, references);
                }
            }
            _ => {}
        }
    }

    let mut references = BTreeSet::new();
    visit(config, &mut references);
    references
}

fn is_test_root(path: &Path) -> bool {
    if is_test_file(path) {
        return true;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("vitest.config.")
                || name.starts_with("vitest.workspace.")
                || name.starts_with("jest.config.")
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
        Value::Object(conditions) => ["source", "import", "default", "types"]
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
    fn discovers_test_files_and_worker_dependencies() {
        let root = tempdir().unwrap();
        let engine = root.path().join("apps/engine");
        let config = root.path().join("apps/config-service");
        let tools = root.path().join("packages/tools");
        fs::create_dir_all(engine.join("src")).unwrap();
        fs::create_dir_all(config.join("src")).unwrap();
        fs::create_dir_all(config.join("tests")).unwrap();
        fs::create_dir_all(tools.join("src")).unwrap();
        fs::write(
            engine.join("package.json"),
            r#"{"name":"engine","module":"src/index.ts"}"#,
        )
        .unwrap();
        fs::write(
            engine.join("wrangler.jsonc"),
            "{ main: 'src/index.ts', env: { production: { name: 'engine-production' } } }",
        )
        .unwrap();
        fs::write(engine.join("src/index.ts"), "export const run = 1;").unwrap();
        fs::write(
            config.join("package.json"),
            r#"{"name":"config-service","module":"src/index.ts","scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        fs::write(
            config.join("wrangler.jsonc"),
            "{ main: 'src/index.ts', durable_objects: { bindings: [{ script_name: 'engine-production' }] } }",
        )
        .unwrap();
        fs::write(config.join("src/index.ts"), "export const api = 1;").unwrap();
        fs::write(
            config.join("tests/index.test.ts"),
            "import { api } from '../src'; test('api', () => api);",
        )
        .unwrap();
        fs::write(
            config.join("vitest.config.ts"),
            "export default { test: {} };",
        )
        .unwrap();
        fs::write(
            tools.join("package.json"),
            r#"{"name":"tools","scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        fs::write(
            tools.join("src/tool.test.ts"),
            "export const verifies = true;",
        )
        .unwrap();

        let packages = discover_packages(root.path()).unwrap();
        let production_files = discover_source_files(root.path(), &packages, false);
        assert!(
            !production_files
                .iter()
                .any(|path| path.ends_with("tests/index.test.ts"))
        );

        let files = discover_source_files(root.path(), &packages, true);
        let targets = targets_for(&packages, "test");
        let test_roots = test_roots_for(&packages, "test", &files);
        let roots = &test_roots["config-service"];
        assert!(
            roots
                .iter()
                .any(|path| path.ends_with("tests/index.test.ts"))
        );
        assert!(roots.iter().any(|path| path.ends_with("vitest.config.ts")));
        assert!(
            roots
                .iter()
                .any(|path| path.ends_with("apps/engine/src/index.ts"))
        );
        assert!(
            test_roots["tools"]
                .iter()
                .any(|path| path.ends_with("packages/tools/src/tool.test.ts"))
        );

        let mut graph = crate::graph::DependencyGraph::build(&files, &targets, &packages).unwrap();
        for (package, roots) in test_roots {
            graph.add_target_roots(&package, &roots);
        }
        let changed = BTreeSet::from([crate::graph::Node::new(
            crate::graph::normalize_path(engine.join("src/index.ts")),
            "run",
        )]);
        let reachability = graph.affected(&changed);
        assert!(
            reachability
                .reached
                .contains(&crate::graph::DependencyGraph::target_node(
                    "config-service"
                ))
        );

        let changed = BTreeSet::from([crate::graph::Node::new(
            crate::graph::normalize_path(tools.join("src/tool.test.ts")),
            "verifies",
        )]);
        let reachability = graph.affected(&changed);
        assert!(
            reachability
                .reached
                .contains(&crate::graph::DependencyGraph::target_node("tools"))
        );
    }
}
