use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use oxc_resolver::{ResolveOptions, Resolver};
use path_clean::PathClean;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::diagnostics::{Diagnostic, Severity, cycles};
use crate::parser::{
    ImportBinding, ImportedName, MODULE_INIT, ParsedModule, SourceLocation, parse_module,
    registry_entry_symbol,
};
use crate::typescript::TypeScriptFacts;
use crate::workspace::{Package, Target};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Node {
    pub file: PathBuf,
    pub symbol: String,
}

impl Node {
    pub fn new(file: impl Into<PathBuf>, symbol: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            symbol: symbol.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TargetNode {
    pub package: String,
    pub node: Node,
}

#[derive(Debug)]
pub struct DependencyGraph {
    pub modules: BTreeMap<PathBuf, ParsedModule>,
    pub edges: BTreeMap<Node, BTreeSet<Node>>,
    pub type_edges: BTreeMap<Node, BTreeSet<Node>>,
    pub targets: Vec<TargetNode>,
    pub diagnostics: Vec<Diagnostic>,
    pub cache_stats: CacheStats,
    typed_registry_edges: BTreeMap<(Node, Node), SourceLocation>,
    resolver: Resolver,
    workspace_packages: Vec<WorkspacePackage>,
}

#[derive(Debug)]
struct WorkspacePackage {
    name: String,
    dir: PathBuf,
    entrypoint: Option<PathBuf>,
    exports: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct CacheStats {
    pub local_hits: usize,
    pub misses: usize,
}

#[derive(Clone, Debug)]
pub struct Reachability {
    pub reached: BTreeSet<Node>,
    pub previous: BTreeMap<Node, Node>,
    pub seed_for: BTreeMap<Node, Node>,
    pub predecessors: BTreeMap<Node, BTreeSet<Node>>,
    pub seeds: BTreeSet<Node>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgeExplanation {
    pub detail: String,
    pub path: Option<PathBuf>,
    pub location: Option<SourceLocation>,
}

impl DependencyGraph {
    pub fn build(files: &[PathBuf], targets: &[Target], packages: &[Package]) -> Result<Self> {
        Self::build_with_cache(files, targets, packages, None)
    }

    pub fn build_with_cache(
        files: &[PathBuf],
        targets: &[Target],
        packages: &[Package],
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        let mut modules = BTreeMap::new();
        let mut cache_stats = CacheStats::default();

        for path in files {
            let path = normalize_path(path);
            let source = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let parsed = if let Some(cache_dir) = cache_dir {
                let mut hasher = Sha256::new();
                hasher.update(b"monoripple-parse-v7\0");
                hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
                hasher.update(b"\0oxc-0.120.0\0");
                hasher.update(path.extension().unwrap_or_default().as_encoded_bytes());
                hasher.update(b"\0");
                hasher.update(source.as_bytes());
                let key = format!("{:x}", hasher.finalize());
                let cache_path = cache_dir.join("parse-v7").join(format!("{key}.json"));

                match fs::read(&cache_path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                {
                    Some(parsed) => {
                        cache_stats.local_hits += 1;
                        parsed
                    }
                    None => {
                        let parsed = parse_module(&path, &source)?;
                        cache_stats.misses += 1;
                        if let Some(parent) = cache_path.parent() {
                            fs::create_dir_all(parent).ok();
                        }
                        if let Ok(bytes) = serde_json::to_vec(&parsed) {
                            let temporary =
                                cache_path.with_extension(format!("{}.tmp", std::process::id()));
                            if fs::write(&temporary, bytes).is_ok()
                                && fs::rename(&temporary, &cache_path).is_err()
                            {
                                fs::remove_file(temporary).ok();
                            }
                        }
                        parsed
                    }
                }
            } else {
                cache_stats.misses += 1;
                parse_module(&path, &source)?
            };
            modules.insert(path, parsed);
        }

        let resolver = Resolver::new(ResolveOptions {
            condition_names: vec![
                "source".to_string(),
                "import".to_string(),
                "module".to_string(),
                "default".to_string(),
            ],
            extensions: vec![
                ".ts".to_string(),
                ".tsx".to_string(),
                ".mts".to_string(),
                ".cts".to_string(),
                ".js".to_string(),
                ".jsx".to_string(),
                ".mjs".to_string(),
                ".cjs".to_string(),
                ".json".to_string(),
            ],
            extension_alias: vec![
                (
                    ".js".to_string(),
                    vec![".ts".to_string(), ".tsx".to_string(), ".js".to_string()],
                ),
                (
                    ".mjs".to_string(),
                    vec![".mts".to_string(), ".mjs".to_string()],
                ),
                (
                    ".cjs".to_string(),
                    vec![".cts".to_string(), ".cjs".to_string()],
                ),
            ],
            main_fields: vec![
                "source".to_string(),
                "module".to_string(),
                "main".to_string(),
            ],
            ..ResolveOptions::default()
        });

        let workspace_packages = packages
            .iter()
            .map(|package| WorkspacePackage {
                name: package.name.clone(),
                dir: normalize_path(&package.dir),
                entrypoint: package.entrypoint.as_ref().map(normalize_path),
                exports: package
                    .exports
                    .iter()
                    .map(|(name, path)| (name.clone(), normalize_path(path)))
                    .collect(),
            })
            .collect();
        let mut graph = Self {
            modules,
            edges: BTreeMap::new(),
            type_edges: BTreeMap::new(),
            targets: Vec::new(),
            diagnostics: Vec::new(),
            cache_stats,
            typed_registry_edges: BTreeMap::new(),
            resolver,
            workspace_packages,
        };
        graph.link_modules();
        graph.link_targets(targets);
        graph.add_graph_diagnostics(packages);
        Ok(graph)
    }

    pub fn affected(&self, seeds: &BTreeSet<Node>) -> Reachability {
        self.affected_with_edges(seeds, false)
    }

    pub fn affected_typecheck(&self, seeds: &BTreeSet<Node>) -> Reachability {
        self.affected_with_edges(seeds, true)
    }

    fn affected_with_edges(&self, seeds: &BTreeSet<Node>, include_types: bool) -> Reachability {
        let mut reverse: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
        for (consumer, dependencies) in &self.edges {
            for dependency in dependencies {
                reverse
                    .entry(dependency.clone())
                    .or_default()
                    .insert(consumer.clone());
            }
        }
        if include_types {
            for (consumer, dependencies) in &self.type_edges {
                for dependency in dependencies {
                    reverse
                        .entry(dependency.clone())
                        .or_default()
                        .insert(consumer.clone());
                }
            }
        }

        let mut reached = seeds.clone();
        let mut previous = BTreeMap::new();
        let mut seed_for = BTreeMap::new();
        let mut predecessors: BTreeMap<Node, BTreeSet<Node>> = BTreeMap::new();
        let mut queue = VecDeque::new();

        for seed in seeds {
            seed_for.insert(seed.clone(), seed.clone());
            queue.push_back(seed.clone());
        }

        while let Some(node) = queue.pop_front() {
            let Some(consumers) = reverse.get(&node) else {
                continue;
            };

            for consumer in consumers {
                predecessors
                    .entry(consumer.clone())
                    .or_default()
                    .insert(node.clone());
                if reached.insert(consumer.clone()) {
                    previous.insert(consumer.clone(), node.clone());
                    seed_for.insert(consumer.clone(), seed_for[&node].clone());
                    queue.push_back(consumer.clone());
                }
            }
        }

        Reachability {
            reached,
            previous,
            seed_for,
            predecessors,
            seeds: seeds.clone(),
        }
    }

    pub fn path_to(&self, reachability: &Reachability, target: &Node) -> Option<Vec<Node>> {
        let seed = reachability.seed_for.get(target)?;
        let mut path = vec![target.clone()];
        let mut current = target;

        while current != seed {
            current = reachability.previous.get(current)?;
            path.push(current.clone());
        }

        path.reverse();
        Some(path)
    }

    pub fn paths_to(
        &self,
        reachability: &Reachability,
        target: &Node,
        limit: usize,
    ) -> Vec<Vec<Node>> {
        if limit == 0 || !reachability.reached.contains(target) {
            return Vec::new();
        }
        let Some(primary) = self.path_to(reachability, target) else {
            return Vec::new();
        };
        if limit == 1 {
            return vec![primary];
        }

        let mut paths = vec![primary];
        let mut pending = VecDeque::from([(target.clone(), vec![target.clone()])]);
        let mut examined = 0;
        let budget = limit.saturating_mul(10_000).max(10_000);
        while let Some((current, reverse_path)) = pending.pop_front() {
            examined += 1;
            if examined > budget {
                break;
            }
            if reachability.seeds.contains(&current) {
                let path = reverse_path.into_iter().rev().collect();
                if !paths.contains(&path) {
                    paths.push(path);
                }
                if paths.len() == limit {
                    break;
                }
                continue;
            }

            for predecessor in reachability
                .predecessors
                .get(&current)
                .into_iter()
                .flatten()
            {
                if reverse_path.contains(predecessor) {
                    continue;
                }
                let mut next_path = reverse_path.clone();
                next_path.push(predecessor.clone());
                pending.push_back((predecessor.clone(), next_path));
            }
        }
        paths
    }

    pub fn edge_explanation(
        &self,
        consumer: &Node,
        dependency: &Node,
        type_only: bool,
    ) -> EdgeExplanation {
        let relationship = if type_only { "type" } else { "runtime" };
        if consumer.file == Path::new("<target>") {
            return EdgeExplanation {
                detail: format!(
                    "deployment target `{}` includes `{}`",
                    consumer.symbol, dependency.symbol
                ),
                path: None,
                location: None,
            };
        }

        if let Some(module) = self.modules.get(&consumer.file) {
            if !type_only
                && let Some(location) = self
                    .typed_registry_edges
                    .get(&(consumer.clone(), dependency.clone()))
            {
                return EdgeExplanation {
                    detail: format!(
                        "`{}` passes a TypeScript-proven registry key",
                        consumer.symbol
                    ),
                    path: Some(consumer.file.clone()),
                    location: Some(*location),
                };
            }

            if !type_only && let Some(symbol) = module.symbols.get(&consumer.symbol) {
                for (registry, keys) in &symbol.keyed_dependencies {
                    for key in keys {
                        let dependencies = self.resolve_keyed_dependency(
                            &consumer.file,
                            registry,
                            &BTreeSet::from([key.clone()]),
                        );
                        if dependencies.is_some_and(|nodes| nodes.contains(dependency)) {
                            let source = module
                                .imports
                                .get(registry)
                                .map_or_else(String::new, |import| {
                                    format!(" from `{}`", import.source)
                                });
                            return EdgeExplanation {
                                detail: format!(
                                    "`{}` reads registry key `{key}`{source}",
                                    consumer.symbol
                                ),
                                path: Some(consumer.file.clone()),
                                location: symbol.dependency_locations.get(registry).copied(),
                            };
                        }
                    }
                }
            }

            if !type_only
                && let Some(registry) = module.registries.get(&consumer.symbol)
                && let Some((key, entry)) = registry.entries.iter().find(|(key, _)| {
                    dependency.symbol == registry_entry_symbol(&consumer.symbol, key)
                })
            {
                return EdgeExplanation {
                    detail: format!("registry `{}` contains key `{key}`", consumer.symbol),
                    path: Some(consumer.file.clone()),
                    location: Some(entry.location),
                };
            }

            let import = if type_only {
                module.type_imports.get(&consumer.symbol)
            } else {
                module.imports.get(&consumer.symbol)
            };
            if let Some(import) = import {
                let imported = match &import.imported {
                    ImportedName::Named(name) => format!("`{name}`"),
                    ImportedName::Namespace {
                        members: Some(members),
                    } => format!(
                        "namespace members {}",
                        members
                            .iter()
                            .map(|member| format!("`{member}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    ImportedName::Namespace { members: None } => "the namespace".to_string(),
                };
                let type_qualifier = if type_only { "type " } else { "" };
                return EdgeExplanation {
                    detail: format!(
                        "`{}` imports {type_qualifier}{imported} from `{}`",
                        consumer.symbol, import.source
                    ),
                    path: Some(consumer.file.clone()),
                    location: Some(import.location),
                };
            }

            if consumer.file == dependency.file
                && let Some(symbol) = module.symbols.get(&consumer.symbol)
            {
                let location = if type_only {
                    symbol.type_dependency_locations.get(&dependency.symbol)
                } else {
                    symbol.dependency_locations.get(&dependency.symbol)
                };
                if let Some(location) = location {
                    return EdgeExplanation {
                        detail: format!(
                            "`{}` references `{}` as a {relationship} dependency",
                            consumer.symbol, dependency.symbol
                        ),
                        path: Some(consumer.file.clone()),
                        location: Some(*location),
                    };
                }
            }

            if let Some(load) = module.runtime_loads.iter().find(|load| {
                load.consumers.contains(&consumer.symbol)
                    && self.resolve(&consumer.file, &load.source).as_ref() == Some(&dependency.file)
            }) {
                return EdgeExplanation {
                    detail: format!("`{}` loads `{}` at runtime", consumer.symbol, load.source),
                    path: Some(consumer.file.clone()),
                    location: Some(load.location),
                };
            }

            if consumer.symbol == MODULE_INIT && dependency.symbol == MODULE_INIT {
                let request = module.module_requests.iter().find(|request| {
                    self.resolve(&consumer.file, request).as_ref() == Some(&dependency.file)
                });
                if let Some(request) = request {
                    let location = module
                        .imports
                        .values()
                        .find(|import| import.source == *request)
                        .map(|import| import.location);
                    return EdgeExplanation {
                        detail: format!("module loads `{request}` at runtime"),
                        path: Some(consumer.file.clone()),
                        location,
                    };
                }
            }
        }

        EdgeExplanation {
            detail: format!("{relationship} dependency"),
            path: (consumer.file != Path::new("<target>")).then(|| consumer.file.clone()),
            location: None,
        }
    }

    pub fn target_node(package: &str) -> Node {
        Node::new(PathBuf::from("<target>"), package)
    }

    pub fn remap_root(&mut self, from: &Path, to: &Path) {
        let from = normalize_path(from);
        let to = normalize_path(to);
        self.modules = std::mem::take(&mut self.modules)
            .into_iter()
            .map(|(path, module)| (remap_path(&path, &from, &to), module))
            .collect();
        self.edges = remap_edges(std::mem::take(&mut self.edges), &from, &to);
        self.type_edges = remap_edges(std::mem::take(&mut self.type_edges), &from, &to);
        self.typed_registry_edges = std::mem::take(&mut self.typed_registry_edges)
            .into_iter()
            .map(|((consumer, dependency), location)| {
                (
                    (
                        Node::new(remap_path(&consumer.file, &from, &to), consumer.symbol),
                        Node::new(remap_path(&dependency.file, &from, &to), dependency.symbol),
                    ),
                    location,
                )
            })
            .collect();
        for diagnostic in &mut self.diagnostics {
            diagnostic.path = diagnostic
                .path
                .as_ref()
                .map(|path| remap_path(path, &from, &to));
            diagnostic.members = diagnostic
                .members
                .iter()
                .map(|path| remap_path(path, &from, &to))
                .collect();
        }
        for package in &mut self.workspace_packages {
            package.dir = remap_path(&package.dir, &from, &to);
            package.entrypoint = package
                .entrypoint
                .as_ref()
                .map(|path| remap_path(path, &from, &to));
            package.exports = std::mem::take(&mut package.exports)
                .into_iter()
                .map(|(name, path)| (name, remap_path(&path, &from, &to)))
                .collect();
        }
    }

    pub fn add_external_edge(&mut self, consumer: Node, dependency: Node, type_only: bool) {
        if type_only {
            self.add_type_edge(consumer, dependency);
        } else {
            self.add_edge(consumer, dependency);
        }
    }

    pub fn apply_typescript_facts(&mut self, facts: &TypeScriptFacts) {
        let typescript_indexed: BTreeSet<_> = facts
            .indexed_registries
            .iter()
            .map(|registry| Node::new(normalize_path(&registry.file), &registry.symbol))
            .collect();
        let mut indexed = BTreeSet::new();
        let mut transparent_edges = Vec::new();
        for (path, module) in &self.modules {
            for dependency in &module.indexed_registry_dependencies {
                let Some(registries) = self.resolve_registry_nodes(path, dependency) else {
                    continue;
                };
                let registries: BTreeSet<_> = registries
                    .into_iter()
                    .filter(|node| {
                        typescript_indexed.contains(node)
                            && self
                                .modules
                                .get(&node.file)
                                .is_some_and(|module| module.registries.contains_key(&node.symbol))
                    })
                    .collect();
                if registries.is_empty() {
                    continue;
                }
                indexed.extend(registries);
                transparent_edges.push((Node::new(path, MODULE_INIT), Node::new(path, dependency)));
            }
        }
        for (consumer, dependency) in transparent_edges {
            if let Some(dependencies) = self.edges.get_mut(&consumer) {
                dependencies.remove(&dependency);
            }
        }

        for fact in &facts.facts {
            let registry = Node::new(normalize_path(&fact.registry_file), &fact.registry_symbol);
            if !indexed.contains(&registry) {
                continue;
            }
            let file = normalize_path(&fact.file);
            let Some(module) = self.modules.get(&file) else {
                continue;
            };
            let consumer_symbol = if module.symbols.contains_key(&fact.consumer) {
                fact.consumer.as_str()
            } else {
                MODULE_INIT
            };
            let consumer = Node::new(&file, consumer_symbol);
            let mut dependencies = BTreeSet::new();
            if let Some(keys) = &fact.keys {
                for key in keys {
                    let has_entry = self
                        .modules
                        .get(&registry.file)
                        .and_then(|module| module.registries.get(&registry.symbol))
                        .is_some_and(|registry| registry.entries.contains_key(key));
                    if !has_entry {
                        dependencies.clear();
                        break;
                    }
                    dependencies.insert(Node::new(
                        &registry.file,
                        registry_entry_symbol(&registry.symbol, key),
                    ));
                }
            }
            if dependencies.is_empty() {
                dependencies.insert(registry.clone());
            }
            let location = SourceLocation {
                line: fact.line,
                column: fact.column,
            };
            for dependency in dependencies {
                self.typed_registry_edges
                    .insert((consumer.clone(), dependency.clone()), location);
                self.add_edge(consumer.clone(), dependency);
            }
        }
    }

    pub fn merge_edges_from(&mut self, other: &Self) {
        for (consumer, dependencies) in &other.edges {
            self.edges
                .entry(consumer.clone())
                .or_default()
                .extend(dependencies.iter().cloned());
        }
        for (consumer, dependencies) in &other.type_edges {
            self.type_edges
                .entry(consumer.clone())
                .or_default()
                .extend(dependencies.iter().cloned());
        }
        self.typed_registry_edges
            .extend(other.typed_registry_edges.clone());
    }

    fn link_modules(&mut self) {
        let paths: Vec<_> = self.modules.keys().cloned().collect();

        for path in paths {
            let Some(module) = self.modules.get(&path).cloned() else {
                continue;
            };

            for (name, symbol) in &module.symbols {
                let consumer = Node::new(&path, name);
                for dependency in &symbol.dependencies {
                    let keyed = symbol
                        .keyed_dependencies
                        .get(dependency)
                        .and_then(|keys| self.resolve_keyed_dependency(&path, dependency, keys));
                    if let Some(keyed) = keyed {
                        for dependency in keyed {
                            self.add_edge(consumer.clone(), dependency);
                        }
                    } else {
                        self.add_edge(consumer.clone(), Node::new(&path, dependency));
                    }
                }
                for dependency in &symbol.type_dependencies {
                    self.add_type_edge(consumer.clone(), Node::new(&path, dependency));
                }
            }

            for (name, registry) in &module.registries {
                let registry_node = Node::new(&path, name);
                for (key, entry) in &registry.entries {
                    let entry_node = Node::new(&path, registry_entry_symbol(name, key));
                    self.add_edge(registry_node.clone(), entry_node.clone());
                    if let Some(dependency) = &entry.dependency {
                        self.add_edge(entry_node, Node::new(&path, dependency));
                    }
                }
            }

            for expression in &module.unresolved_dynamic_imports {
                self.diagnostics.push(Diagnostic {
                    code: "MONORIPPLE_DYNAMIC_IMPORT_NON_LITERAL",
                    severity: Severity::Warning,
                    message: format!(
                        "dynamic import `{expression}` cannot be resolved to a finite module set"
                    ),
                    path: Some(path.clone()),
                    members: Vec::new(),
                });
            }

            for (local_name, import) in &module.imports {
                self.link_import(&path, local_name, import);
            }
            for (local_name, import) in &module.type_imports {
                self.link_type_import(&path, local_name, import);
            }

            for request in &module.module_requests {
                if let Some(dependency_path) = self.resolve(&path, request) {
                    self.add_edge(
                        Node::new(&path, MODULE_INIT),
                        Node::new(dependency_path, MODULE_INIT),
                    );
                } else if self.request_is_internal(request)
                    && !module
                        .imports
                        .values()
                        .any(|import| import.source == *request)
                {
                    let workspace_package = self.request_is_workspace_package(request);
                    self.diagnostics.push(Diagnostic {
                        code: if workspace_package {
                            "MONORIPPLE_UNRESOLVED_WORKSPACE_IMPORT"
                        } else {
                            "MONORIPPLE_UNRESOLVED_LOCAL_IMPORT"
                        },
                        severity: if workspace_package {
                            Severity::Error
                        } else {
                            Severity::Warning
                        },
                        message: format!("cannot resolve runtime module `{request}`"),
                        path: Some(path.clone()),
                        members: Vec::new(),
                    });
                }
            }

            for load in &module.runtime_loads {
                let Some(dependency_path) = self.resolve(&path, &load.source) else {
                    if self.request_is_internal(&load.source) {
                        let workspace_package = self.request_is_workspace_package(&load.source);
                        self.diagnostics.push(Diagnostic {
                            code: if workspace_package {
                                "MONORIPPLE_UNRESOLVED_WORKSPACE_IMPORT"
                            } else {
                                "MONORIPPLE_UNRESOLVED_LOCAL_IMPORT"
                            },
                            severity: if workspace_package {
                                Severity::Error
                            } else {
                                Severity::Warning
                            },
                            message: format!("cannot resolve runtime module `{}`", load.source),
                            path: Some(path.clone()),
                            members: Vec::new(),
                        });
                    }
                    continue;
                };

                for consumer_name in &load.consumers {
                    let consumer = Node::new(&path, consumer_name);
                    self.add_edge(consumer.clone(), Node::new(&dependency_path, MODULE_INIT));
                    let mut visited = BTreeSet::new();
                    for dependency in self.all_exports(&dependency_path, &mut visited) {
                        self.add_edge(consumer.clone(), dependency);
                    }
                }
            }
        }
    }

    fn resolve_keyed_dependency(
        &self,
        path: &Path,
        dependency: &str,
        keys: &BTreeSet<String>,
    ) -> Option<BTreeSet<Node>> {
        if keys.is_empty() {
            return None;
        }
        let registries = self.resolve_registry_nodes(path, dependency)?;
        if registries.is_empty() {
            return None;
        }

        let mut entries = BTreeSet::new();
        for registry_node in registries {
            let registry = self
                .modules
                .get(&registry_node.file)?
                .registries
                .get(&registry_node.symbol)?;
            for key in keys {
                if !registry.entries.contains_key(key) {
                    return None;
                }
                entries.insert(Node::new(
                    &registry_node.file,
                    registry_entry_symbol(&registry_node.symbol, key),
                ));
            }
        }
        Some(entries)
    }

    fn resolve_registry_nodes(&self, path: &Path, dependency: &str) -> Option<BTreeSet<Node>> {
        let module = self.modules.get(path)?;
        if module.registries.contains_key(dependency) {
            return Some(BTreeSet::from([Node::new(path, dependency)]));
        }

        let import = module.imports.get(dependency)?;
        let ImportedName::Named(imported) = &import.imported else {
            return None;
        };
        let dependency_path = self.resolve(path, &import.source)?;
        let mut visited = BTreeSet::new();
        let registries = self.resolve_export(&dependency_path, imported, &mut visited);
        (!registries.is_empty()).then_some(registries)
    }

    fn link_import(&mut self, path: &Path, local_name: &str, import: &ImportBinding) {
        let Some(dependency_path) = self.resolve(path, &import.source) else {
            if self.request_is_internal(&import.source) {
                let workspace_package = self.request_is_workspace_package(&import.source);
                self.diagnostics.push(Diagnostic {
                    code: if workspace_package {
                        "MONORIPPLE_UNRESOLVED_WORKSPACE_IMPORT"
                    } else {
                        "MONORIPPLE_UNRESOLVED_LOCAL_IMPORT"
                    },
                    severity: if workspace_package {
                        Severity::Error
                    } else {
                        Severity::Warning
                    },
                    message: format!("cannot resolve imported module `{}`", import.source),
                    path: Some(path.to_path_buf()),
                    members: Vec::new(),
                });
            }
            return;
        };
        let consumer = Node::new(path, local_name);
        self.add_edge(consumer.clone(), Node::new(&dependency_path, MODULE_INIT));

        match &import.imported {
            ImportedName::Named(name) => {
                let mut visited = BTreeSet::new();
                for dependency in self.resolve_export(&dependency_path, name, &mut visited) {
                    self.add_edge(consumer.clone(), dependency);
                }
            }
            ImportedName::Namespace { members } => {
                let mut visited = BTreeSet::new();
                if let Some(members) = members {
                    for member in members {
                        for dependency in
                            self.resolve_export(&dependency_path, member, &mut visited)
                        {
                            self.add_edge(consumer.clone(), dependency);
                        }
                    }
                } else {
                    self.diagnostics.push(Diagnostic {
                        code: "MONORIPPLE_NAMESPACE_DYNAMIC_ACCESS",
                        severity: Severity::Warning,
                        message: format!(
                            "namespace import `{local_name}` uses dynamic access; all exports are dependencies"
                        ),
                        path: Some(path.to_path_buf()),
                        members: Vec::new(),
                    });
                    for dependency in self.all_exports(&dependency_path, &mut visited) {
                        self.add_edge(consumer.clone(), dependency);
                    }
                }
            }
        }
    }

    fn link_type_import(&mut self, path: &Path, local_name: &str, import: &ImportBinding) {
        let Some(dependency_path) = self.resolve(path, &import.source) else {
            return;
        };
        let consumer = Node::new(path, local_name);

        match &import.imported {
            ImportedName::Named(name) => {
                let mut visited = BTreeSet::new();
                for dependency in self.resolve_type_export(&dependency_path, name, &mut visited) {
                    self.add_type_edge(consumer.clone(), dependency);
                }
            }
            ImportedName::Namespace { members } => {
                let mut visited = BTreeSet::new();
                if let Some(members) = members {
                    for member in members {
                        for dependency in
                            self.resolve_type_export(&dependency_path, member, &mut visited)
                        {
                            self.add_type_edge(consumer.clone(), dependency);
                        }
                    }
                } else {
                    for dependency in self.all_type_exports(&dependency_path, &mut visited) {
                        self.add_type_edge(consumer.clone(), dependency);
                    }
                }
            }
        }
    }

    fn link_targets(&mut self, targets: &[Target]) {
        for target in targets {
            let entrypoint = normalize_path(&target.entrypoint);
            if self.modules.contains_key(&entrypoint) {
                self.add_target_roots(&target.package, &[entrypoint]);
            } else {
                self.diagnostics.push(Diagnostic {
                    code: "MONORIPPLE_VIRTUAL_OR_GENERATED_ENTRYPOINT",
                    severity: Severity::Warning,
                    message: format!(
                        "target `{}` entrypoint `{}` is not represented in the source graph",
                        target.package,
                        entrypoint.display()
                    ),
                    path: Some(entrypoint),
                    members: Vec::new(),
                });
                self.targets.push(TargetNode {
                    package: target.package.clone(),
                    node: Self::target_node(&target.package),
                });
            }
        }
    }

    pub fn add_target_roots(&mut self, package: &str, roots: &[PathBuf]) {
        let target_node = Self::target_node(package);
        for root in roots {
            let root = normalize_path(root);
            if !self.modules.contains_key(&root) {
                continue;
            }
            self.add_edge(target_node.clone(), Node::new(&root, MODULE_INIT));

            let mut visited = BTreeSet::new();
            let exports = self.all_exports(&root, &mut visited);
            if exports.is_empty() {
                if let Some(module) = self.modules.get(&root) {
                    let symbols: Vec<_> = module
                        .symbols
                        .iter()
                        .filter(|(name, symbol)| name.as_str() != MODULE_INIT && symbol.runtime)
                        .map(|(name, _)| name.clone())
                        .collect();
                    for symbol in symbols {
                        self.add_edge(target_node.clone(), Node::new(&root, symbol));
                    }
                }
            } else {
                for exported in exports {
                    self.add_edge(target_node.clone(), exported);
                }
            }
        }

        if !self.targets.iter().any(|target| target.package == package) {
            self.targets.push(TargetNode {
                package: package.to_string(),
                node: target_node,
            });
        }
        self.diagnostics.retain(|diagnostic| {
            diagnostic.code != "MONORIPPLE_VIRTUAL_OR_GENERATED_ENTRYPOINT"
                || !diagnostic
                    .message
                    .starts_with(&format!("target `{package}` "))
        });
    }

    pub fn add_external_target(&mut self, package: &str) {
        if !self.targets.iter().any(|target| target.package == package) {
            self.targets.push(TargetNode {
                package: package.to_string(),
                node: Self::target_node(package),
            });
        }
        self.diagnostics.retain(|diagnostic| {
            diagnostic.code != "MONORIPPLE_VIRTUAL_OR_GENERATED_ENTRYPOINT"
                || !diagnostic
                    .message
                    .starts_with(&format!("target `{package}` "))
        });
    }

    fn resolve_export(
        &self,
        path: &Path,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> BTreeSet<Node> {
        let key = (path.to_path_buf(), export_name.to_string());
        if !visited.insert(key) {
            return BTreeSet::new();
        }

        let Some(module) = self.modules.get(path) else {
            return BTreeSet::new();
        };
        let mut resolved = BTreeSet::new();

        if let Some(local_name) = module.local_exports.get(export_name)
            && module
                .symbols
                .get(local_name)
                .is_some_and(|symbol| symbol.runtime)
        {
            resolved.insert(Node::new(path, local_name));
        }

        for re_export in module
            .re_exports
            .iter()
            .filter(|re_export| re_export.exported == export_name)
        {
            let Some(dependency_path) = self.resolve(path, &re_export.source) else {
                continue;
            };
            match &re_export.imported {
                ImportedName::Named(name) => {
                    resolved.extend(self.resolve_export(&dependency_path, name, visited));
                }
                ImportedName::Namespace { .. } => {
                    resolved.extend(self.all_exports(&dependency_path, visited));
                }
            }
        }

        if resolved.is_empty() && export_name != "default" {
            for source in &module.star_exports {
                let Some(dependency_path) = self.resolve(path, source) else {
                    continue;
                };
                resolved.extend(self.resolve_export(&dependency_path, export_name, visited));
            }
        }

        resolved
    }

    fn resolve_type_export(
        &self,
        path: &Path,
        export_name: &str,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> BTreeSet<Node> {
        let key = (path.to_path_buf(), export_name.to_string());
        if !visited.insert(key) {
            return BTreeSet::new();
        }

        let Some(module) = self.modules.get(path) else {
            return BTreeSet::new();
        };
        let mut resolved = BTreeSet::new();

        if let Some(local_name) = module.type_local_exports.get(export_name)
            && module.symbols.contains_key(local_name)
        {
            resolved.insert(Node::new(path, local_name));
        }

        for re_export in module
            .type_re_exports
            .iter()
            .filter(|re_export| re_export.exported == export_name)
        {
            let Some(dependency_path) = self.resolve(path, &re_export.source) else {
                continue;
            };
            match &re_export.imported {
                ImportedName::Named(name) => {
                    resolved.extend(self.resolve_type_export(&dependency_path, name, visited));
                }
                ImportedName::Namespace { .. } => {
                    resolved.extend(self.all_type_exports(&dependency_path, visited));
                }
            }
        }

        if resolved.is_empty() && export_name != "default" {
            for source in &module.type_star_exports {
                let Some(dependency_path) = self.resolve(path, source) else {
                    continue;
                };
                resolved.extend(self.resolve_type_export(&dependency_path, export_name, visited));
            }
        }

        resolved
    }

    fn all_type_exports(
        &self,
        path: &Path,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> BTreeSet<Node> {
        let Some(module) = self.modules.get(path) else {
            return BTreeSet::new();
        };
        let mut resolved = BTreeSet::new();

        for export_name in module.type_local_exports.keys() {
            resolved.extend(self.resolve_type_export(path, export_name, visited));
        }
        for re_export in &module.type_re_exports {
            resolved.extend(self.resolve_type_export(path, &re_export.exported, visited));
        }
        for source in &module.type_star_exports {
            let Some(dependency_path) = self.resolve(path, source) else {
                continue;
            };
            resolved.extend(self.all_type_exports(&dependency_path, visited));
        }

        resolved
    }

    fn all_exports(
        &self,
        path: &Path,
        visited: &mut BTreeSet<(PathBuf, String)>,
    ) -> BTreeSet<Node> {
        let Some(module) = self.modules.get(path) else {
            return BTreeSet::new();
        };
        let mut resolved = BTreeSet::new();

        for export_name in module.local_exports.keys() {
            resolved.extend(self.resolve_export(path, export_name, visited));
        }
        for re_export in &module.re_exports {
            resolved.extend(self.resolve_export(path, &re_export.exported, visited));
        }
        for source in &module.star_exports {
            let Some(dependency_path) = self.resolve(path, source) else {
                continue;
            };
            resolved.extend(self.all_exports(&dependency_path, visited));
        }

        resolved
    }

    pub fn refresh_graph_diagnostics(&mut self, packages: &[Package]) {
        self.diagnostics.retain(|diagnostic| {
            !matches!(
                diagnostic.code,
                "MONORIPPLE_PACKAGE_CYCLE" | "MONORIPPLE_RUNTIME_MODULE_CYCLE"
            )
        });
        self.add_graph_diagnostics(packages);
    }

    fn add_graph_diagnostics(&mut self, packages: &[Package]) {
        let package_names: BTreeSet<_> = packages
            .iter()
            .map(|package| package.name.clone())
            .collect();
        let package_graph: BTreeMap<_, _> = packages
            .iter()
            .map(|package| {
                (
                    package.name.clone(),
                    package
                        .dependencies
                        .intersection(&package_names)
                        .cloned()
                        .collect(),
                )
            })
            .collect();
        for component in cycles(&package_graph) {
            self.diagnostics.push(Diagnostic {
                code: "MONORIPPLE_PACKAGE_CYCLE",
                severity: Severity::Info,
                message: format!("workspace package cycle: {}", component.join(" -> ")),
                path: None,
                members: Vec::new(),
            });
        }

        let mut reachable: BTreeSet<_> = self
            .targets
            .iter()
            .map(|target| target.node.clone())
            .collect();
        let mut pending: Vec<_> = reachable.iter().cloned().collect();
        while let Some(node) = pending.pop() {
            if let Some(dependencies) = self.edges.get(&node) {
                for dependency in dependencies {
                    if reachable.insert(dependency.clone()) {
                        pending.push(dependency.clone());
                    }
                }
            }
        }

        let reachable_files: BTreeSet<_> = reachable
            .iter()
            .filter(|node| node.file != Path::new("<target>"))
            .map(|node| node.file.clone())
            .collect();
        self.diagnostics.retain(|diagnostic| {
            diagnostic.path.is_none()
                || diagnostic.code == "MONORIPPLE_VIRTUAL_OR_GENERATED_ENTRYPOINT"
                || diagnostic
                    .path
                    .as_ref()
                    .is_some_and(|path| reachable_files.contains(path))
        });

        let mut module_graph: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
        for (consumer, dependencies) in &self.edges {
            if !reachable.contains(consumer) || consumer.file == Path::new("<target>") {
                continue;
            }
            for dependency in dependencies {
                if reachable.contains(dependency)
                    && dependency.file != Path::new("<target>")
                    && dependency.file != consumer.file
                {
                    module_graph
                        .entry(consumer.file.clone())
                        .or_default()
                        .insert(dependency.file.clone());
                }
            }
        }
        for component in cycles(&module_graph) {
            self.diagnostics.push(Diagnostic {
                code: "MONORIPPLE_RUNTIME_MODULE_CYCLE",
                severity: Severity::Info,
                message: format!(
                    "runtime module cycle containing {} modules",
                    component.len()
                ),
                path: component.first().cloned(),
                members: component,
            });
        }
        self.diagnostics.sort();
        self.diagnostics.dedup();
    }

    fn request_is_internal(&self, request: &str) -> bool {
        request.starts_with('.')
            || request.starts_with('/')
            || request.starts_with('#')
            || self.request_is_workspace_package(request)
    }

    fn request_is_workspace_package(&self, request: &str) -> bool {
        self.workspace_packages.iter().any(|package| {
            request == package.name
                || request
                    .strip_prefix(&package.name)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }

    fn resolve(&self, importer: &Path, request: &str) -> Option<PathBuf> {
        if let Some(package) = self.workspace_packages.iter().find(|package| {
            request == package.name
                || request
                    .strip_prefix(&package.name)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            let subpath = request
                .strip_prefix(&package.name)
                .unwrap_or_default()
                .trim_start_matches('/');
            let export_name = if subpath.is_empty() {
                ".".to_string()
            } else {
                format!("./{subpath}")
            };
            let candidate = package.exports.get(&export_name).cloned().or_else(|| {
                if subpath.is_empty() {
                    package.entrypoint.clone()
                } else {
                    resolve_from_directory(&self.resolver, &package.dir, &format!("./{subpath}"))
                }
            });
            if let Some(path) =
                candidate.filter(|path| self.modules.contains_key(path) || path.is_file())
            {
                return Some(path);
            }
        }

        let directory = importer.parent()?;
        if request.starts_with('.') {
            let requested = directory.join(request).clean();
            let mut candidates = Vec::new();

            match requested
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some("js") => {
                    candidates.push(requested.with_extension("ts"));
                    candidates.push(requested.with_extension("tsx"));
                    candidates.push(requested);
                }
                Some("mjs") => {
                    candidates.push(requested.with_extension("mts"));
                    candidates.push(requested);
                }
                Some("cjs") => {
                    candidates.push(requested.with_extension("cts"));
                    candidates.push(requested);
                }
                Some("ts" | "tsx" | "mts" | "cts" | "jsx" | "json") => candidates.push(requested),
                Some(_) | None => {
                    candidates.push(requested.clone());
                    for extension in ["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"] {
                        candidates.push(PathBuf::from(format!(
                            "{}.{}",
                            requested.display(),
                            extension
                        )));
                        candidates.push(requested.join("index").with_extension(extension));
                    }
                }
            }

            return candidates.into_iter().find_map(|candidate| {
                let candidate = normalize_path(candidate);
                (self.modules.contains_key(&candidate) || candidate.is_file()).then_some(candidate)
            });
        }

        if !request.starts_with('/') && !request.starts_with('#') {
            return None;
        }

        let path = resolve_from_directory(&self.resolver, directory, request)?;
        (self.modules.contains_key(&path) || path.is_file()).then_some(path)
    }

    fn add_edge(&mut self, consumer: Node, dependency: Node) {
        if consumer != dependency {
            self.edges.entry(consumer).or_default().insert(dependency);
        }
    }

    fn add_type_edge(&mut self, consumer: Node, dependency: Node) {
        if consumer != dependency {
            self.type_edges
                .entry(consumer)
                .or_default()
                .insert(dependency);
        }
    }
}

fn remap_edges(
    edges: BTreeMap<Node, BTreeSet<Node>>,
    from: &Path,
    to: &Path,
) -> BTreeMap<Node, BTreeSet<Node>> {
    edges
        .into_iter()
        .map(|(consumer, dependencies)| {
            (
                Node::new(remap_path(&consumer.file, from, to), consumer.symbol),
                dependencies
                    .into_iter()
                    .map(|dependency| {
                        Node::new(remap_path(&dependency.file, from, to), dependency.symbol)
                    })
                    .collect(),
            )
        })
        .collect()
}

fn remap_path(path: &Path, from: &Path, to: &Path) -> PathBuf {
    if path == Path::new("<target>") {
        return path.to_path_buf();
    }
    path.strip_prefix(from)
        .map(|relative| to.join(relative))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_from_directory(resolver: &Resolver, directory: &Path, request: &str) -> Option<PathBuf> {
    resolver
        .resolve(directory, request)
        .ok()
        .map(|resolution| normalize_path(resolution.full_path()))
}

pub fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    path.as_ref()
        .canonicalize()
        .unwrap_or_else(|_| path.as_ref().to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::typescript::{TypeScriptFact, TypeScriptFacts, TypeScriptRegistry};

    #[test]
    fn reuses_local_parse_cache() {
        let root = tempdir().unwrap();
        let cache = tempdir().unwrap();
        let source = root.path().join("index.ts");
        fs::write(&source, "export const value = 1;").unwrap();

        let first = DependencyGraph::build_with_cache(
            std::slice::from_ref(&source),
            &[],
            &[],
            Some(cache.path()),
        )
        .unwrap();
        let second = DependencyGraph::build_with_cache(
            std::slice::from_ref(&source),
            &[],
            &[],
            Some(cache.path()),
        )
        .unwrap();

        assert_eq!(first.cache_stats.misses, 1);
        assert_eq!(second.cache_stats.local_hits, 1);
    }

    #[test]
    fn reaches_only_the_target_importing_the_changed_symbol() {
        let root = tempdir().unwrap();
        let shared = root.path().join("shared.ts");
        let app_a = root.path().join("app-a.ts");
        let app_b = root.path().join("app-b.ts");
        fs::write(
            &shared,
            "export const used = 1; export const unrelated = 2;",
        )
        .unwrap();
        fs::write(
            &app_a,
            "import { used } from './shared'; export default used;",
        )
        .unwrap();
        fs::write(
            &app_b,
            "import { unrelated } from './shared'; export default unrelated;",
        )
        .unwrap();

        let targets = vec![
            Target {
                package: "app-a".to_string(),
                entrypoint: app_a.clone(),
            },
            Target {
                package: "app-b".to_string(),
                entrypoint: app_b.clone(),
            },
        ];
        let graph = DependencyGraph::build(&[shared.clone(), app_a, app_b], &targets, &[]).unwrap();
        let seeds = BTreeSet::from([Node::new(normalize_path(shared), "used")]);
        let reached = graph.affected(&seeds);

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app-a"))
        );
        assert!(
            !reached
                .reached
                .contains(&DependencyGraph::target_node("app-b"))
        );
    }

    #[test]
    fn typescript_facts_narrow_indexed_array_registry_calls() {
        let root = tempdir().unwrap();
        let manifest = root.path().join("manifest.ts");
        let runtime = root.path().join("runtime.ts");
        let alpha = root.path().join("alpha.ts");
        let added = root.path().join("added.ts");
        fs::write(
            &manifest,
            "const alpha = { name: 'alpha' } as const; const added = { name: 'added' } as const; export const registry = [alpha, added] as const;",
        )
        .unwrap();
        fs::write(
            &runtime,
            "import { registry } from './manifest'; const index = new Map(); export function lookup(key: string) { return index.get(key); } for (const entry of registry) { index.set(entry.name, entry); }",
        )
        .unwrap();
        fs::write(
            &alpha,
            "import { lookup } from './runtime'; const result = lookup('alpha'); export default result;",
        )
        .unwrap();
        fs::write(
            &added,
            "import { lookup } from './runtime'; const result = lookup('added'); export default result;",
        )
        .unwrap();
        let targets = [
            Target {
                package: "alpha".to_string(),
                entrypoint: alpha.clone(),
            },
            Target {
                package: "added".to_string(),
                entrypoint: added.clone(),
            },
        ];
        let mut graph = DependencyGraph::build(
            &[manifest.clone(), runtime, alpha.clone(), added.clone()],
            &targets,
            &[],
        )
        .unwrap();
        let registry = Node::new(normalize_path(&manifest), "registry");
        let added_entry = Node::new(
            normalize_path(&manifest),
            registry_entry_symbol("registry", "added"),
        );
        let broad = graph.affected(&BTreeSet::from([added_entry.clone()]));
        assert!(
            broad
                .reached
                .contains(&DependencyGraph::target_node("alpha"))
        );

        graph.apply_typescript_facts(&TypeScriptFacts {
            indexed_registries: vec![TypeScriptRegistry {
                file: manifest.clone(),
                symbol: "registry".to_string(),
            }],
            facts: vec![
                TypeScriptFact {
                    file: alpha,
                    consumer: "result".to_string(),
                    registry_file: manifest.clone(),
                    registry_symbol: "registry".to_string(),
                    keys: Some(vec!["alpha".to_string()]),
                    line: 1,
                    column: 60,
                },
                TypeScriptFact {
                    file: added,
                    consumer: "result".to_string(),
                    registry_file: manifest,
                    registry_symbol: "registry".to_string(),
                    keys: Some(vec!["added".to_string()]),
                    line: 1,
                    column: 60,
                },
            ],
        });
        let narrowed = graph.affected(&BTreeSet::from([added_entry]));
        assert!(
            narrowed
                .reached
                .contains(&DependencyGraph::target_node("added"))
        );
        assert!(
            !narrowed
                .reached
                .contains(&DependencyGraph::target_node("alpha"))
        );
        assert!(
            graph
                .edges
                .values()
                .any(|dependencies| dependencies.contains(&registry))
        );
    }

    #[test]
    fn returns_multiple_paths_with_source_explanations() {
        let root = tempdir().unwrap();
        let shared = root.path().join("shared.ts");
        let app = root.path().join("app.ts");
        fs::write(&shared, "export const changed = 1;").unwrap();
        fs::write(
            &app,
            "import { changed } from './shared';\nconst viaA = changed + 1;\nconst viaB = changed + 2;\nconst result = viaA + viaB;\nexport { result as default };",
        )
        .unwrap();

        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(&[shared.clone(), app.clone()], &targets, &[]).unwrap();
        let seed = Node::new(normalize_path(&shared), "changed");
        let reachability = graph.affected(&BTreeSet::from([seed.clone()]));
        let paths = graph.paths_to(&reachability, &DependencyGraph::target_node("app"), 20);

        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .any(|path| path.iter().any(|node| node.symbol == "viaA"))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.iter().any(|node| node.symbol == "viaB"))
        );

        let imported = Node::new(normalize_path(&app), "changed");
        let explanation = graph.edge_explanation(&imported, &seed, false);
        assert!(
            explanation
                .detail
                .contains("imports `changed` from `./shared`")
        );
        assert_eq!(explanation.location.unwrap().line, 1);

        let local =
            graph.edge_explanation(&Node::new(normalize_path(app), "viaA"), &imported, false);
        assert!(local.detail.contains("references `changed`"));
        assert_eq!(local.location.unwrap().line, 2);
    }

    #[test]
    fn propagates_type_only_changes_for_typecheck_queries() {
        let root = tempdir().unwrap();
        let shared = root.path().join("shared.ts");
        let app = root.path().join("app.ts");
        fs::write(&shared, "export interface Config { value: string }").unwrap();
        fs::write(
            &app,
            "import type { Config } from './shared'; export type Options = Config;",
        )
        .unwrap();

        let graph = DependencyGraph::build(&[shared.clone(), app.clone()], &[], &[]).unwrap();
        let seeds = BTreeSet::from([Node::new(normalize_path(shared), "Config")]);
        let runtime = graph.affected(&seeds);
        let typecheck = graph.affected_typecheck(&seeds);

        assert!(
            !runtime
                .reached
                .contains(&Node::new(normalize_path(&app), "Options"))
        );
        assert!(
            typecheck
                .reached
                .contains(&Node::new(normalize_path(app), "Options"))
        );
    }

    #[test]
    fn base_graph_preserves_removed_export_dependencies() {
        let base_root = tempdir().unwrap();
        let current_root = tempdir().unwrap();
        let base_shared = base_root.path().join("shared.ts");
        let base_app = base_root.path().join("app.ts");
        let current_shared = current_root.path().join("shared.ts");
        let current_app = current_root.path().join("app.ts");
        fs::write(&base_shared, "export const removed = 1;").unwrap();
        fs::write(
            &base_app,
            "import { removed } from './shared'; export default removed;",
        )
        .unwrap();
        fs::write(&current_shared, "export const replacement = 1;").unwrap();
        fs::write(
            &current_app,
            "import { removed } from './shared'; export default removed;",
        )
        .unwrap();

        let base_targets = vec![Target {
            package: "app".to_string(),
            entrypoint: base_app.clone(),
        }];
        let current_targets = vec![Target {
            package: "app".to_string(),
            entrypoint: current_app.clone(),
        }];
        let mut base =
            DependencyGraph::build(&[base_shared.clone(), base_app], &base_targets, &[]).unwrap();
        base.remap_root(base_root.path(), current_root.path());
        let mut current = DependencyGraph::build(
            &[current_shared.clone(), current_app],
            &current_targets,
            &[],
        )
        .unwrap();
        current.merge_edges_from(&base);

        let removed = Node::new(
            normalize_path(current_root.path().join("shared.ts")),
            "removed",
        );
        let reached = current.affected(&BTreeSet::from([removed]));
        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn reaches_target_through_top_level_execution() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        fs::write(
            &app,
            "const handler = () => 'ok'; export const router = {}; router.handler = handler;",
        )
        .unwrap();

        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(std::slice::from_ref(&app), &targets, &[]).unwrap();
        let reached = graph.affected(&BTreeSet::from([Node::new(normalize_path(app), "handler")]));

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn reaches_target_through_literal_dynamic_import() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        let lazy = root.path().join("lazy.ts");
        fs::write(
            &app,
            "export async function load() { return (await import('./lazy')).value; }",
        )
        .unwrap();
        fs::write(&lazy, "export const value = 'old';").unwrap();

        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(&[app, lazy.clone()], &targets, &[]).unwrap();
        let reached = graph.affected(&BTreeSet::from([Node::new(normalize_path(lazy), "value")]));

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn reaches_target_through_commonjs_require() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.cjs");
        let dependency = root.path().join("dependency.cjs");
        fs::write(
            &app,
            "const dependency = require('./dependency.cjs'); module.exports = () => dependency.value;",
        )
        .unwrap();
        fs::write(&dependency, "const value = 'old'; exports.value = value;").unwrap();

        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(&[app, dependency.clone()], &targets, &[]).unwrap();
        let reached = graph.affected(&BTreeSet::from([Node::new(
            normalize_path(dependency),
            "value",
        )]));

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn reaches_target_through_imported_asset() {
        let root = tempdir().unwrap();
        let app = root.path().join("app.ts");
        let stylesheet = root.path().join("style.css");
        fs::write(&app, "import './style.css'; export const value = true;").unwrap();
        fs::write(&stylesheet, "body { color: red; }").unwrap();

        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(std::slice::from_ref(&app), &targets, &[]).unwrap();
        let reached = graph.affected(&BTreeSet::from([Node::new(
            normalize_path(stylesheet),
            MODULE_INIT,
        )]));

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }

    #[test]
    fn resolves_workspace_exports_without_node_modules() {
        let root = tempdir().unwrap();
        let shared_dir = root.path().join("packages/shared");
        let app_dir = root.path().join("apps/app");
        fs::create_dir_all(shared_dir.join("src")).unwrap();
        fs::create_dir_all(app_dir.join("src")).unwrap();
        let shared = shared_dir.join("src/index.ts");
        let app = app_dir.join("src/index.ts");
        fs::write(&shared, "export const value = 1;").unwrap();
        fs::write(
            &app,
            "import { value } from '@repo/shared'; export default value;",
        )
        .unwrap();

        let packages = vec![Package {
            name: "@repo/shared".to_string(),
            dir: shared_dir,
            scripts: BTreeMap::new(),
            entrypoint: Some(shared.clone()),
            exports: BTreeMap::from([(".".to_string(), shared.clone())]),
            dependencies: BTreeSet::new(),
        }];
        let targets = vec![Target {
            package: "app".to_string(),
            entrypoint: app.clone(),
        }];
        let graph = DependencyGraph::build(&[shared.clone(), app], &targets, &packages).unwrap();
        let seeds = BTreeSet::from([Node::new(normalize_path(shared), "value")]);
        let reached = graph.affected(&seeds);

        assert!(
            reached
                .reached
                .contains(&DependencyGraph::target_node("app"))
        );
    }
}
