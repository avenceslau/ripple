use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use globset::Glob;
use serde::Serialize;

use monoripple::analysis::{ChangeSeeds, find_change_seeds};
use monoripple::cache::default_cache_dir;
use monoripple::diagnostics::{Diagnostic, Severity};
use monoripple::git::{changed_files, extract_revision, repository_root};
use monoripple::graph::{DependencyGraph, EdgeExplanation, Node};
use monoripple::parser::ImportedName;
use monoripple::plugin::{PluginEdgeKind, run_configured_plugins};
use monoripple::ui::{WhyUiItem, WhyUiModel};
use monoripple::viz::{GraphLink, GraphNode, GraphView, NodeKind, render_html};
use monoripple::workspace::{discover_packages, discover_source_files, targets_for};

#[derive(Parser)]
#[command(name = "monoripple", version, about)]
struct Cli {
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Affected(QueryArgs),
    Check(CheckArgs),
    Why {
        package: String,
        #[arg(long)]
        ui: bool,
        #[command(flatten)]
        query: QueryArgs,
    },
    Graph(GraphArgs),
    Run(RunArgs),
}

#[derive(Clone, clap::Args)]
struct RunArgs {
    task_name: String,

    #[arg(long, default_value = "origin/main")]
    base: String,

    #[arg(long)]
    target: Option<String>,

    #[arg(long, value_enum, default_value = "deploy")]
    mode: TaskKind,

    #[arg(long, value_enum, default_value = "auto")]
    runner: TaskRunner,

    #[arg(long)]
    print: bool,

    #[arg(long)]
    no_cache: bool,

    #[arg(long)]
    cache_report: bool,

    #[arg(long, value_enum, default_value = "warn")]
    warnings: WarningPolicy,

    #[arg(long = "runner-arg", alias = "turbo-arg", allow_hyphen_values = true)]
    runner_args: Vec<String>,

    #[arg(last = true)]
    task_args: Vec<String>,
}

#[derive(Clone, clap::Args)]
struct GraphArgs {
    #[command(flatten)]
    query: QueryArgs,

    #[arg(long, value_enum, default_value = "affected")]
    scope: GraphScope,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long, default_value_t = 0)]
    port: u16,

    #[arg(long)]
    no_open: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum GraphScope {
    Affected,
    All,
}

#[derive(Clone, clap::Args)]
struct QueryArgs {
    #[arg(long, default_value = "origin/main")]
    base: String,

    #[arg(long, default_value = "deploy")]
    target: String,

    #[arg(long, value_enum, default_value = "deploy")]
    task: TaskKind,

    #[arg(long, value_enum, default_value = "text")]
    format: OutputFormat,

    #[arg(long)]
    no_cache: bool,

    #[arg(long)]
    cache_report: bool,

    #[arg(long, value_enum, default_value = "warn")]
    warnings: WarningPolicy,
}

#[derive(Clone, clap::Args)]
struct CheckArgs {
    #[arg(long, default_value = "deploy")]
    target: String,

    #[arg(long, value_enum, default_value = "text")]
    format: OutputFormat,

    #[arg(long)]
    no_cache: bool,

    #[arg(long)]
    cache_report: bool,

    #[arg(long, value_enum, default_value = "warn")]
    warnings: WarningPolicy,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Turbo,
    VitePlus,
}

#[derive(Clone, Copy, ValueEnum)]
enum TaskRunner {
    Auto,
    Pnpm,
    Npx,
    Bun,
    Yarn,
    Turbo,
    VitePlus,
}

#[derive(Clone, Copy, ValueEnum)]
enum WarningPolicy {
    Off,
    Warn,
    Error,
}

#[derive(Clone, Copy, ValueEnum)]
enum TaskKind {
    Deploy,
    Typecheck,
}

const MAX_REASON_PATHS: usize = 20;

#[derive(Serialize)]
struct AffectedOutput {
    base: String,
    target: String,
    task: &'static str,
    packages: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let start = if cli.root.is_absolute() {
        cli.root
    } else {
        env::current_dir()?.join(cli.root)
    };
    let root = repository_root(&start)?;

    match cli.command {
        Command::Affected(query) => run_affected(&root, &query),
        Command::Check(args) => run_check(&root, &args),
        Command::Why { package, ui, query } => run_why(&root, &package, ui, &query),
        Command::Graph(args) => run_graph(&root, &args),
        Command::Run(args) => run_task(&root, &args),
    }
}

fn run_task(root: &Path, args: &RunArgs) -> Result<()> {
    let target = args
        .target
        .clone()
        .unwrap_or_else(|| args.task_name.clone());
    let query = QueryArgs {
        base: args.base.clone(),
        target,
        task: args.mode,
        format: OutputFormat::Text,
        no_cache: args.no_cache,
        cache_report: args.cache_report,
        warnings: args.warnings,
    };
    let analysis = analyze(root, &query)?;
    emit_diagnostics(
        root,
        &analysis.graph.diagnostics,
        OutputFormat::Text,
        args.warnings,
        false,
    )?;
    let affected = affected_packages(&analysis);
    if affected.is_empty() {
        eprintln!(
            "no affected packages have a `{}` task; the task runner was not invoked",
            args.task_name
        );
        return Ok(());
    }

    let package_json = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    let has_vite_plus =
        root.join("node_modules/.bin/vp").is_file() || package_json.contains("\"vite-plus\"");
    let runner = match args.runner {
        TaskRunner::Auto if has_vite_plus => TaskRunner::VitePlus,
        TaskRunner::Auto if root.join("pnpm-lock.yaml").is_file() => TaskRunner::Pnpm,
        TaskRunner::Auto if root.join("bun.lock").is_file() => TaskRunner::Bun,
        TaskRunner::Auto if root.join("yarn.lock").is_file() => TaskRunner::Yarn,
        TaskRunner::Auto => TaskRunner::Npx,
        runner => runner,
    };
    let (program, mut command_args): (&str, Vec<String>) = if matches!(runner, TaskRunner::VitePlus)
    {
        if root.join("pnpm-lock.yaml").is_file() {
            ("pnpm", vec!["exec".into(), "vp".into(), "run".into()])
        } else if root.join("bun.lock").is_file() {
            ("bunx", vec!["vp".into(), "run".into()])
        } else if root.join("yarn.lock").is_file() {
            ("yarn", vec!["vp".into(), "run".into()])
        } else {
            (
                "npx",
                vec!["--no-install".into(), "vp".into(), "run".into()],
            )
        }
    } else {
        match runner {
            TaskRunner::Pnpm => ("pnpm", vec!["exec".into(), "turbo".into(), "run".into()]),
            TaskRunner::Npx => (
                "npx",
                vec!["--no-install".into(), "turbo".into(), "run".into()],
            ),
            TaskRunner::Bun => ("bunx", vec!["turbo".into(), "run".into()]),
            TaskRunner::Yarn => ("yarn", vec!["turbo".into(), "run".into()]),
            TaskRunner::Turbo => ("turbo", vec!["run".into()]),
            TaskRunner::VitePlus | TaskRunner::Auto => unreachable!(),
        }
    };
    command_args.extend(args.runner_args.iter().cloned());
    if matches!(runner, TaskRunner::VitePlus) {
        command_args.extend(affected.iter().map(|package| format!("--filter={package}")));
        command_args.push(args.task_name.clone());
    } else {
        command_args.push(args.task_name.clone());
        command_args.extend(affected.iter().map(|package| format!("--filter={package}")));
    }
    if !args.task_args.is_empty() {
        command_args.push("--".to_string());
        command_args.extend(args.task_args.iter().cloned());
    }

    eprintln!(
        "running: {} {}",
        program,
        command_args
            .iter()
            .map(|argument| shell_argument(argument))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if args.print {
        return Ok(());
    }

    let status = ProcessCommand::new(program)
        .args(&command_args)
        .current_dir(root)
        .status()
        .with_context(|| format!("failed to launch {program}"))?;
    if !status.success() {
        bail!("task runner exited with {status}");
    }
    Ok(())
}

fn shell_argument(argument: &str) -> String {
    if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_=:@/.,".contains(character))
    {
        argument.to_string()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

fn run_graph(root: &Path, args: &GraphArgs) -> Result<()> {
    let analysis = analyze(root, &args.query)?;
    let view = build_graph_view(root, &analysis, args.scope);
    let html = render_html(&view);

    if let Some(output) = &args.output {
        std::fs::write(output, &html)
            .with_context(|| format!("failed to write {}", output.display()))?;
        eprintln!("graph written to {}", output.display());
    }

    serve(html, args.port, !args.no_open)
}

fn serve(html: String, port: u16, open: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind 127.0.0.1:{port}"))?;
    let url = format!("http://{}", listener.local_addr()?);
    eprintln!("serving monoripple graph at {url}  (press Ctrl-C to stop)");

    if open {
        open_in_browser(&url)?;
    }

    let body = html.into_bytes();
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let mut buffer = [0u8; 2048];
        let _ = stream.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(header.as_bytes()).is_ok() {
            let _ = stream.write_all(&body);
        }
        let _ = stream.flush();
    }

    Ok(())
}

fn build_graph_view(root: &Path, analysis: &Analysis, scope: GraphScope) -> GraphView {
    if matches!(scope, GraphScope::All) {
        return build_module_view(root, analysis);
    }

    let seeds = match analysis.task {
        TaskKind::Deploy => &analysis.seeds.nodes,
        TaskKind::Typecheck => &analysis.seeds.type_nodes,
    };
    let reached = &analysis.reachability.reached;

    let mut include = BTreeSet::new();
    for target in &analysis.graph.targets {
        let affected = reached.contains(&target.node)
            || analysis.seeds.direct_packages.contains_key(&target.package);
        if !affected {
            continue;
        }
        include.insert(target.node.clone());
        for path in analysis
            .graph
            .paths_to(&analysis.reachability, &target.node, MAX_REASON_PATHS)
        {
            include.extend(path);
        }
    }

    let mut index = BTreeMap::new();
    let mut nodes = Vec::new();
    for node in &include {
        let kind = if node.file == Path::new("<target>") {
            NodeKind::Target
        } else if seeds.contains(node) {
            NodeKind::Seed
        } else if reached.contains(node) {
            NodeKind::Affected
        } else {
            NodeKind::Normal
        };
        let details = analysis
            .seeds
            .direct_packages
            .get(&node.symbol)
            .filter(|_| matches!(kind, NodeKind::Target))
            .map(|inputs| {
                inputs
                    .iter()
                    .flat_map(|(input, changes)| {
                        let file = input
                            .strip_prefix(root)
                            .unwrap_or(input)
                            .display()
                            .to_string();
                        std::iter::once(file).chain(changes.iter().cloned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        index.insert(node.clone(), nodes.len());
        nodes.push(GraphNode {
            id: nodes.len(),
            label: display_node(root, node),
            file: node
                .file
                .strip_prefix(root)
                .unwrap_or(&node.file)
                .display()
                .to_string(),
            symbol: node.symbol.clone(),
            package: package_of(analysis, node),
            kind,
            details,
            paths: Vec::new(),
        });
    }

    let mut shared_nodes: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut shared_links = Vec::new();
    for target in &analysis.graph.targets {
        let Some(&target_id) = index.get(&target.node) else {
            continue;
        };
        let Some(inputs) = analysis.seeds.direct_packages.get(&target.package) else {
            continue;
        };
        let dependencies: BTreeSet<_> = inputs
            .values()
            .flatten()
            .filter_map(|detail| added_dependency_name(detail))
            .collect();
        let Some(package) = analysis
            .packages
            .iter()
            .find(|package| package.name == target.package)
        else {
            continue;
        };

        for dependency in dependencies {
            let mut runtime = BTreeSet::new();
            let mut types = BTreeSet::new();
            for module in analysis
                .graph
                .modules
                .iter()
                .filter(|(path, _)| path.starts_with(&package.dir))
                .map(|(_, module)| module)
            {
                for binding in module
                    .imports
                    .values()
                    .filter(|binding| binding.source == dependency)
                {
                    collect_imported_names(&binding.imported, &mut runtime);
                }
                for binding in module
                    .type_imports
                    .values()
                    .filter(|binding| binding.source == dependency)
                {
                    collect_imported_names(&binding.imported, &mut types);
                }
            }
            types.retain(|name| !runtime.contains(name));

            for (name, type_only) in runtime
                .into_iter()
                .map(|name| (name, false))
                .chain(types.into_iter().map(|name| (name, true)))
            {
                let key = (dependency.clone(), name.clone());
                let dependency_id = if let Some(&id) = shared_nodes.get(&key) {
                    id
                } else {
                    let id = nodes.len();
                    shared_nodes.insert(key, id);
                    nodes.push(GraphNode {
                        id,
                        label: format!("{dependency}#{name}"),
                        file: dependency.clone(),
                        symbol: name.clone(),
                        package: dependency.clone(),
                        kind: NodeKind::Dependency,
                        details: Vec::new(),
                        paths: Vec::new(),
                    });
                    id
                };
                let usage = if type_only { "type" } else { "runtime" };
                nodes[dependency_id]
                    .details
                    .push(format!("{usage} export used by {}", target.package));
                nodes[target_id]
                    .details
                    .push(format!("uses {usage} export `{name}` from `{dependency}`"));
                shared_links.push(GraphLink {
                    source: target_id,
                    target: dependency_id,
                    type_only,
                    detail: format!("`{}` uses `{name}` from `{dependency}`", target.package),
                    location: None,
                });
            }
        }
    }

    for (node, &id) in &index {
        nodes[id].paths = analysis
            .graph
            .paths_to(&analysis.reachability, node, MAX_REASON_PATHS)
            .into_iter()
            .map(|path| {
                path.iter()
                    .filter_map(|hop| index.get(hop).copied())
                    .collect()
            })
            .collect();
    }

    let mut links = shared_links;
    for (edges, type_only) in [
        (&analysis.graph.edges, false),
        (&analysis.graph.type_edges, true),
    ] {
        for (consumer, dependencies) in edges {
            let Some(&source) = index.get(consumer) else {
                continue;
            };
            for dependency in dependencies {
                if let Some(&target) = index.get(dependency) {
                    let explanation = analysis
                        .graph
                        .edge_explanation(consumer, dependency, type_only);
                    let location = display_edge_location(root, &explanation);
                    links.push(GraphLink {
                        source,
                        target,
                        type_only,
                        detail: explanation.detail,
                        location,
                    });
                }
            }
        }
    }

    GraphView {
        package: None,
        base: analysis.base.clone(),
        task: task_label(analysis.task).to_string(),
        scope: "affected".to_string(),
        nodes,
        links,
    }
}

fn build_module_view(root: &Path, analysis: &Analysis) -> GraphView {
    let reached_files: BTreeSet<_> = analysis
        .reachability
        .reached
        .iter()
        .filter(|node| node.file != Path::new("<target>"))
        .map(|node| node.file.clone())
        .collect();
    let seed_files: BTreeSet<_> = match analysis.task {
        TaskKind::Deploy => &analysis.seeds.nodes,
        TaskKind::Typecheck => &analysis.seeds.type_nodes,
    }
    .iter()
    .map(|node| node.file.clone())
    .collect();

    let mut index = BTreeMap::new();
    let mut nodes = Vec::new();
    for file in analysis.graph.modules.keys() {
        let kind = if seed_files.contains(file) {
            NodeKind::Seed
        } else if reached_files.contains(file) {
            NodeKind::Affected
        } else {
            NodeKind::Normal
        };
        let relative = file
            .strip_prefix(root)
            .unwrap_or(file)
            .display()
            .to_string();
        index.insert(file.clone(), nodes.len());
        nodes.push(GraphNode {
            id: nodes.len(),
            label: relative.clone(),
            file: relative,
            symbol: "<module>".to_string(),
            package: package_of(analysis, &Node::new(file.clone(), "<module>".to_string())),
            kind,
            details: Vec::new(),
            paths: Vec::new(),
        });
    }

    let mut seen = BTreeSet::new();
    let mut links = Vec::new();
    for (edges, type_only) in [
        (&analysis.graph.edges, false),
        (&analysis.graph.type_edges, true),
    ] {
        for (consumer, dependencies) in edges {
            let Some(&source) = index.get(&consumer.file) else {
                continue;
            };
            for dependency in dependencies {
                let Some(&target) = index.get(&dependency.file) else {
                    continue;
                };
                if source != target && seen.insert((source, target, type_only)) {
                    let explanation = analysis
                        .graph
                        .edge_explanation(consumer, dependency, type_only);
                    let location = display_edge_location(root, &explanation);
                    links.push(GraphLink {
                        source,
                        target,
                        type_only,
                        detail: explanation.detail,
                        location,
                    });
                }
            }
        }
    }

    GraphView {
        package: None,
        base: analysis.base.clone(),
        task: task_label(analysis.task).to_string(),
        scope: "all".to_string(),
        nodes,
        links,
    }
}

fn added_dependency_name(detail: &str) -> Option<String> {
    if !detail.starts_with("added ") || !detail.contains("dependency `") {
        return None;
    }
    let start = detail.find('`')? + 1;
    let end = detail[start..].find('`')? + start;
    Some(detail[start..end].to_string())
}

fn collect_imported_names(imported: &ImportedName, names: &mut BTreeSet<String>) {
    match imported {
        ImportedName::Named(name) => {
            names.insert(name.clone());
        }
        ImportedName::Namespace {
            members: Some(members),
        } => {
            names.extend(members.iter().cloned());
        }
        ImportedName::Namespace { members: None } => {
            names.insert("*".to_string());
        }
    }
}

fn task_label(task: TaskKind) -> &'static str {
    match task {
        TaskKind::Deploy => "deploy",
        TaskKind::Typecheck => "typecheck",
    }
}

fn package_of(analysis: &Analysis, node: &Node) -> String {
    if node.file == Path::new("<target>") {
        return node.symbol.clone();
    }
    analysis
        .packages
        .iter()
        .filter(|package| node.file.starts_with(&package.dir))
        .max_by_key(|package| package.dir.components().count())
        .map(|package| package.name.clone())
        .unwrap_or_default()
}

fn open_in_browser(target: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    ProcessCommand::new(opener)
        .arg(target)
        .status()
        .with_context(|| format!("failed to launch {opener}"))?;
    Ok(())
}

fn run_affected(root: &Path, query: &QueryArgs) -> Result<()> {
    let analysis = analyze(root, query)?;
    emit_diagnostics(
        root,
        &analysis.graph.diagnostics,
        query.format,
        query.warnings,
        false,
    )?;
    let affected = affected_packages(&analysis);

    match query.format {
        OutputFormat::Text => {
            for package in affected {
                println!("{package}");
            }
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&AffectedOutput {
                    base: query.base.clone(),
                    target: query.target.clone(),
                    task: match query.task {
                        TaskKind::Deploy => "deploy",
                        TaskKind::Typecheck => "typecheck",
                    },
                    packages: affected,
                })?
            );
        }
        OutputFormat::Turbo | OutputFormat::VitePlus => {
            if affected.is_empty() {
                println!("--filter=__monoripple_no_affected_packages__");
            } else {
                println!(
                    "{}",
                    affected
                        .iter()
                        .map(|package| format!("--filter={package}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }

    Ok(())
}

fn run_why(root: &Path, package: &str, ui: bool, query: &QueryArgs) -> Result<()> {
    if matches!(query.task, TaskKind::Typecheck) {
        bail!("`why` for typecheck queries is not implemented yet");
    }
    let analysis = analyze(root, query)?;
    emit_diagnostics(
        root,
        &analysis.graph.diagnostics,
        query.format,
        query.warnings,
        false,
    )?;

    if let Some(inputs) = analysis.seeds.direct_packages.get(package) {
        if ui {
            let mut items: Vec<_> = inputs
                .iter()
                .map(|(input, changes)| WhyUiItem {
                    label: input
                        .strip_prefix(root)
                        .unwrap_or(input)
                        .display()
                        .to_string(),
                    detail: if changes.is_empty() {
                        "This changed build input directly affects the package.".to_string()
                    } else {
                        changes.join("\n")
                    },
                })
                .collect();
            items.push(WhyUiItem {
                label: format!("target {package}"),
                detail: format!("Deployment target `{package}` is directly affected."),
            });
            return monoripple::ui::run(WhyUiModel {
                package: package.to_string(),
                base: query.base.clone(),
                paths: vec![items],
                cycles: Vec::new(),
            });
        }

        println!("{package} is directly affected by:");
        for (input, changes) in inputs {
            println!(
                "  - {}",
                input.strip_prefix(root).unwrap_or(input).display()
            );
            for change in changes {
                println!("      {change}");
            }
        }
        return Ok(());
    }

    let Some(target) = analysis
        .graph
        .targets
        .iter()
        .find(|target| target.package == package)
    else {
        bail!("target package '{package}' was not found");
    };
    let paths = analysis
        .graph
        .paths_to(&analysis.reachability, &target.node, MAX_REASON_PATHS);
    if paths.is_empty() {
        bail!("target package '{package}' is not affected");
    }

    let path_files: BTreeSet<_> = paths
        .iter()
        .flatten()
        .map(|node| node.file.clone())
        .collect();
    if ui {
        let ui_paths =
            paths
                .iter()
                .map(|path| {
                    path.iter()
                        .enumerate()
                        .map(|(index, node)| {
                            let detail =
                                if let Some(consumer) = path.get(index + 1) {
                                    let type_only =
                                        !analysis.graph.edges.get(consumer).is_some_and(
                                            |dependencies| dependencies.contains(node),
                                        ) && analysis.graph.type_edges.get(consumer).is_some_and(
                                            |dependencies| dependencies.contains(node),
                                        );
                                    let explanation =
                                        analysis.graph.edge_explanation(consumer, node, type_only);
                                    let location = display_edge_location(root, &explanation)
                                        .map(|location| format!("\nSource: {location}"))
                                        .unwrap_or_default();
                                    let changed = if index == 0 {
                                        "This declaration changed.\n"
                                    } else {
                                        ""
                                    };
                                    format!(
                                        "{changed}Next relationship: {}{location}",
                                        explanation.detail
                                    )
                                } else {
                                    format!("The impact reaches deployment target `{package}`.")
                                };
                            WhyUiItem {
                                label: display_node(root, node),
                                detail,
                            }
                        })
                        .collect()
                })
                .collect();
        let cycles = analysis
            .graph
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "MONORIPPLE_RUNTIME_MODULE_CYCLE"
                    && diagnostic
                        .members
                        .iter()
                        .any(|member| path_files.contains(member))
            })
            .map(|diagnostic| {
                diagnostic
                    .members
                    .iter()
                    .map(|member| {
                        member
                            .strip_prefix(root)
                            .unwrap_or(member)
                            .display()
                            .to_string()
                    })
                    .collect()
            })
            .collect();
        return monoripple::ui::run(WhyUiModel {
            package: package.to_string(),
            base: query.base.clone(),
            paths: ui_paths,
            cycles,
        });
    }

    if paths.len() == 1 {
        println!("{package} is affected through this impact path:");
    } else {
        println!(
            "{package} is affected through {} impact paths (showing up to {MAX_REASON_PATHS}):",
            paths.len()
        );
    }
    for (path_index, path) in paths.iter().enumerate() {
        if paths.len() > 1 {
            println!("\nPath {}:", path_index + 1);
        }
        for (index, node) in path.iter().enumerate() {
            println!("  {}. {}", index + 1, display_node(root, node));
            if let Some(consumer) = path.get(index + 1) {
                let type_only = !analysis
                    .graph
                    .edges
                    .get(consumer)
                    .is_some_and(|dependencies| dependencies.contains(node))
                    && analysis
                        .graph
                        .type_edges
                        .get(consumer)
                        .is_some_and(|dependencies| dependencies.contains(node));
                let explanation = analysis.graph.edge_explanation(consumer, node, type_only);
                let location = display_edge_location(root, &explanation)
                    .map(|location| format!(" at {location}"))
                    .unwrap_or_default();
                println!("     ↓ {}{location}", explanation.detail);
            }
        }
    }

    for diagnostic in &analysis.graph.diagnostics {
        if diagnostic.code == "MONORIPPLE_RUNTIME_MODULE_CYCLE"
            && diagnostic
                .members
                .iter()
                .any(|member| path_files.contains(member))
        {
            println!("\nimpact paths enter a runtime module cycle:");
            for member in &diagnostic.members {
                println!(
                    "  - {}",
                    member.strip_prefix(root).unwrap_or(member).display()
                );
            }
        }
    }

    Ok(())
}

fn run_check(root: &Path, args: &CheckArgs) -> Result<()> {
    let graph = build_graph(root, &args.target, args.no_cache, args.cache_report)?;

    if matches!(args.format, OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&graph.diagnostics)?);
    }
    emit_diagnostics(root, &graph.diagnostics, args.format, args.warnings, true)
}

fn emit_diagnostics(
    root: &Path,
    diagnostics: &[Diagnostic],
    format: OutputFormat,
    warnings: WarningPolicy,
    include_info: bool,
) -> Result<()> {
    if !matches!(format, OutputFormat::Json) {
        for diagnostic in diagnostics {
            if diagnostic.severity == Severity::Info && !include_info {
                continue;
            }
            if diagnostic.severity == Severity::Warning && matches!(warnings, WarningPolicy::Off) {
                continue;
            }

            let path = diagnostic
                .path
                .as_ref()
                .map(|path| {
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .unwrap_or_default();
            let location = if path.is_empty() {
                String::new()
            } else {
                format!(" {path}")
            };
            eprintln!(
                "{:?} {}{}: {}",
                diagnostic.severity, diagnostic.code, location, diagnostic.message
            );
            for member in &diagnostic.members {
                eprintln!(
                    "  - {}",
                    member.strip_prefix(root).unwrap_or(member).display()
                );
            }
        }
    }

    let has_error = diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error);
    let warning_is_error = matches!(warnings, WarningPolicy::Error)
        && diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Warning);
    if has_error || warning_is_error {
        bail!("monoripple graph diagnostics failed");
    }

    Ok(())
}

struct Analysis {
    graph: DependencyGraph,
    packages: Vec<monoripple::workspace::Package>,
    seeds: ChangeSeeds,
    reachability: monoripple::graph::Reachability,
    task: TaskKind,
    base: String,
}

fn analyze(root: &Path, query: &QueryArgs) -> Result<Analysis> {
    let packages = discover_packages(root)?;
    let mut graph = build_graph(root, &query.target, query.no_cache, query.cache_report)?;
    let changes = changed_files(root, &query.base)?;
    let needs_base_graph = changes.iter().any(|change| {
        monoripple::parser::is_source_file(&change.path)
            || matches!(
                change.kind,
                monoripple::git::ChangeKind::Deleted | monoripple::git::ChangeKind::Renamed { .. }
            )
            || change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == "package.json"
                        || name == "pnpm-workspace.yaml"
                        || (name.starts_with("tsconfig") && name.ends_with(".json"))
                })
    });
    let base_snapshot = needs_base_graph
        .then(|| extract_revision(root, &query.base))
        .transpose()?;
    let base_graph = if let Some(snapshot) = &base_snapshot {
        let mut base_graph = build_graph(snapshot.path(), &query.target, query.no_cache, false)?;
        base_graph.remap_root(snapshot.path(), root);
        graph.merge_edges_from(&base_graph);
        Some(base_graph)
    } else {
        None
    };

    for change in &changes {
        let file_name = change.path.file_name().and_then(|name| name.to_str());
        if matches!(
            file_name,
            Some(
                "pnpm-lock.yaml"
                    | "package-lock.json"
                    | "yarn.lock"
                    | "bun.lock"
                    | "bun.lockb"
                    | "Cargo.lock"
            )
        ) {
            graph.diagnostics.push(Diagnostic {
                code: "MONORIPPLE_LOCKFILE_CHANGE_UNMODELED",
                severity: Severity::Warning,
                message: "lockfile changes conservatively affect targets because exact runtime consumers are not modeled"
                    .to_string(),
                path: Some(change.path.clone()),
                members: Vec::new(),
            });
        }
    }
    graph.diagnostics.sort();
    graph.diagnostics.dedup();
    let seeds = find_change_seeds(
        root,
        &query.base,
        &changes,
        &packages,
        &graph,
        base_graph.as_ref(),
    )?;
    let reachability = match query.task {
        TaskKind::Deploy => graph.affected(&seeds.nodes),
        TaskKind::Typecheck => graph.affected_typecheck(&seeds.type_nodes),
    };

    Ok(Analysis {
        graph,
        packages,
        seeds,
        reachability,
        task: query.task,
        base: query.base.clone(),
    })
}

fn build_graph(
    root: &Path,
    target: &str,
    no_cache: bool,
    cache_report: bool,
) -> Result<DependencyGraph> {
    let packages = discover_packages(root)?;
    let mut files = discover_source_files(root, &packages);
    let targets = targets_for(&packages, target);
    let plugins = run_configured_plugins(root, target)?;
    let plugin_targets: Vec<_> = plugins
        .targets
        .into_iter()
        .map(|plugin_target| {
            let roots: Vec<_> = plugin_target
                .roots
                .into_iter()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
                .collect();
            files.extend(
                roots
                    .iter()
                    .filter(|path| monoripple::parser::is_source_file(path))
                    .cloned(),
            );
            (plugin_target.package, roots)
        })
        .collect();
    files.sort();
    files.dedup();

    let cache_dir = (!no_cache).then(default_cache_dir).flatten();
    let mut graph =
        DependencyGraph::build_with_cache(&files, &targets, &packages, cache_dir.as_deref())?;
    for (package, roots) in plugin_targets {
        graph.add_target_roots(&package, &roots);
    }
    for edge in plugins.edges {
        let consumer_path = if edge.consumer.path.is_absolute() {
            edge.consumer.path
        } else {
            root.join(edge.consumer.path)
        };
        let dependency_path = if edge.dependency.path.is_absolute() {
            edge.dependency.path
        } else {
            root.join(edge.dependency.path)
        };
        graph.add_external_edge(
            Node::new(consumer_path, edge.consumer.symbol),
            Node::new(dependency_path, edge.dependency.symbol),
            matches!(edge.kind, PluginEdgeKind::Type),
        );
    }
    graph.refresh_graph_diagnostics(&packages);
    graph.diagnostics.extend(plugins.diagnostics);
    for exclusion in plugins.exclusions {
        let matcher = exclusion
            .path
            .as_ref()
            .map(|pattern| Glob::new(pattern).map(|glob| glob.compile_matcher()))
            .transpose()?;
        graph.diagnostics.retain(|diagnostic| {
            if diagnostic.code != exclusion.code {
                return true;
            }
            let Some(matcher) = &matcher else {
                return false;
            };
            diagnostic
                .path
                .as_ref()
                .map(|path| path.strip_prefix(root).unwrap_or(path))
                .is_none_or(|path| !matcher.is_match(path))
        });
    }
    graph.diagnostics.sort();
    graph.diagnostics.dedup();
    if cache_report {
        eprintln!(
            "cache: {} local hits, {} misses",
            graph.cache_stats.local_hits, graph.cache_stats.misses
        );
    }
    Ok(graph)
}

fn affected_packages(analysis: &Analysis) -> Vec<String> {
    let mut affected = BTreeSet::new();

    match analysis.task {
        TaskKind::Deploy => {
            for target in &analysis.graph.targets {
                if analysis.reachability.reached.contains(&target.node)
                    || analysis.seeds.direct_packages.contains_key(&target.package)
                {
                    affected.insert(target.package.clone());
                }
            }
        }
        TaskKind::Typecheck => {
            for node in &analysis.reachability.reached {
                if let Some(package) = analysis
                    .packages
                    .iter()
                    .filter(|package| node.file.starts_with(&package.dir))
                    .max_by_key(|package| package.dir.components().count())
                {
                    affected.insert(package.name.clone());
                }
            }
            affected.extend(analysis.seeds.direct_packages.keys().cloned());
        }
    }

    affected.into_iter().collect()
}

fn display_edge_location(root: &Path, explanation: &EdgeExplanation) -> Option<String> {
    explanation.path.as_ref().map(|path| {
        let path = path.strip_prefix(root).unwrap_or(path);
        if let Some(location) = explanation.location {
            format!("{}:{}:{}", path.display(), location.line, location.column)
        } else {
            path.display().to_string()
        }
    })
}

fn display_node(root: &Path, node: &Node) -> String {
    if node.file == Path::new("<target>") {
        return format!("target {}", node.symbol);
    }

    let path = node.file.strip_prefix(root).unwrap_or(&node.file);
    format!("{}#{}", path.display(), node.symbol)
}
