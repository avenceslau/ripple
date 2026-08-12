use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::diagnostics::{Diagnostic, Severity};

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    plugins: Vec<PluginConfig>,
    #[serde(default)]
    diagnostics: DiagnosticConfig,
}

#[derive(Default, Deserialize)]
struct DiagnosticConfig {
    #[serde(default)]
    exclude: Vec<DiagnosticExclusion>,
}

#[derive(Deserialize)]
pub struct DiagnosticExclusion {
    pub code: String,
    pub path: Option<String>,
    pub reason: String,
}

#[derive(Deserialize)]
struct PluginConfig {
    name: String,
    command: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PluginRequest<'a> {
    protocol_version: u32,
    repository_root: &'a Path,
    target: &'a str,
}

#[derive(Deserialize)]
pub struct PluginResponse {
    #[serde(default)]
    pub targets: Vec<PluginTarget>,
    #[serde(default)]
    pub edges: Vec<PluginEdge>,
    #[serde(default)]
    diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Deserialize)]
pub struct PluginTarget {
    pub package: String,
    pub roots: Vec<PathBuf>,
}

#[derive(Deserialize)]
pub struct PluginEdge {
    pub consumer: PluginNode,
    pub dependency: PluginNode,
    #[serde(default)]
    pub kind: PluginEdgeKind,
}

#[derive(Deserialize)]
pub struct PluginNode {
    pub path: PathBuf,
    pub symbol: String,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginEdgeKind {
    #[default]
    Runtime,
    Type,
}

#[derive(Deserialize)]
struct PluginDiagnostic {
    severity: Severity,
    message: String,
    path: Option<PathBuf>,
}

pub struct PluginOutput {
    pub targets: Vec<PluginTarget>,
    pub edges: Vec<PluginEdge>,
    pub diagnostics: Vec<Diagnostic>,
    pub exclusions: Vec<DiagnosticExclusion>,
}

pub fn run_configured_plugins(root: &Path, target: &str) -> Result<PluginOutput> {
    let config_path = root.join("monoripple.json");
    if !config_path.is_file() {
        return Ok(PluginOutput {
            targets: Vec::new(),
            edges: Vec::new(),
            diagnostics: Vec::new(),
            exclusions: Vec::new(),
        });
    }

    let config: Config = serde_json::from_slice(&fs::read(&config_path)?)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let exclusions = config.diagnostics.exclude;
    let request = serde_json::to_vec(&PluginRequest {
        protocol_version: 1,
        repository_root: root,
        target,
    })?;
    let mut targets = Vec::new();
    let mut edges = Vec::new();
    let mut diagnostics = Vec::new();

    for plugin in config.plugins {
        let Some(program) = plugin.command.first() else {
            bail!("plugin `{}` has an empty command", plugin.name);
        };
        let mut child = Command::new(program)
            .args(&plugin.command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start plugin `{}`", plugin.name))?;
        let mut stdin = child
            .stdin
            .take()
            .with_context(|| format!("plugin `{}` stdin was unavailable", plugin.name))?;
        stdin.write_all(&request)?;
        drop(stdin);
        let output = child.wait_with_output()?;
        if !output.status.success() {
            diagnostics.push(Diagnostic {
                code: "MONORIPPLE_PLUGIN_FAILURE",
                severity: Severity::Error,
                message: format!(
                    "plugin `{}` failed: {}",
                    plugin.name,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                path: Some(config_path.clone()),
                members: Vec::new(),
            });
            continue;
        }

        let response: PluginResponse = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("plugin `{}` returned invalid JSON", plugin.name))?;
        targets.extend(response.targets);
        edges.extend(response.edges);
        diagnostics.extend(
            response
                .diagnostics
                .into_iter()
                .map(|diagnostic| Diagnostic {
                    code: "MONORIPPLE_PLUGIN_DIAGNOSTIC",
                    severity: diagnostic.severity,
                    message: format!("plugin `{}`: {}", plugin.name, diagnostic.message),
                    path: diagnostic.path.map(|path| root.join(path)),
                    members: Vec::new(),
                }),
        );
    }

    Ok(PluginOutput {
        targets,
        edges,
        diagnostics,
        exclusions,
    })
}
