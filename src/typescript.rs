use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use oxc_allocator::Allocator;
use oxc_ast::{
    AstKind,
    ast::{Argument, Expression, IdentifierReference},
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};

use crate::parser::{MODULE_INIT, parse_module};
use crate::tsgo::TsgoClient;

#[derive(Clone, Debug)]
pub struct TypeScriptFact {
    pub file: PathBuf,
    pub consumer: String,
    pub registry_file: PathBuf,
    pub registry_symbol: String,
    pub keys: Option<Vec<String>>,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct TypeScriptRegistry {
    pub file: PathBuf,
    pub symbol: String,
}

#[derive(Clone, Debug, Default)]
pub struct TypeScriptFacts {
    pub indexed_registries: Vec<TypeScriptRegistry>,
    pub facts: Vec<TypeScriptFact>,
}

struct Contract {
    owner: String,
    method: String,
}

struct Callsite {
    file: PathBuf,
    consumer: String,
    method: String,
    keys: Option<Vec<String>>,
    offset: usize,
}

pub fn analyze(
    source_root: &Path,
    compiler_root: &Path,
    files: &[PathBuf],
) -> Result<Option<TypeScriptFacts>> {
    let tsgo = find_tsgo();
    if tsgo.is_none() && !TsgoClient::has_embedded_tsgo() {
        return Ok(None);
    }
    link_node_modules(source_root, compiler_root);
    let mut contracts = Vec::new();
    let mut calls = Vec::new();
    let mut registries = BTreeMap::new();
    let mut sources = BTreeMap::new();

    for file in files {
        let source = fs::read_to_string(file)
            .with_context(|| format!("failed to read {} for tsgo", file.display()))?;
        let parsed = parse_module(file, &source)?;
        for (symbol, registry) in parsed.registries {
            registries.insert(
                (file.clone(), symbol),
                registry.entries.keys().cloned().collect::<BTreeSet<_>>(),
            );
        }
        collect_candidates(file, &source, &mut contracts, &mut calls)?;
        sources.insert(file.clone(), source);
    }
    let method_names: BTreeSet<_> = contracts.iter().map(|contract| &contract.method).collect();
    calls.retain(|call| method_names.contains(&call.method));
    if calls.is_empty() || registries.is_empty() {
        return Ok(None);
    }

    let mut client = TsgoClient::new(tsgo.as_deref())
        .map_err(|error| anyhow::anyhow!("failed to start tsgo: {error}"))?;
    client
        .initialize(&file_uri(source_root)?)
        .map_err(|error| anyhow::anyhow!("failed to initialize tsgo: {error}"))?;
    for (file, source) in &sources {
        client
            .open_file(&file_uri(file)?, source)
            .map_err(|error| {
                anyhow::anyhow!("failed to open {} in tsgo: {error}", file.display())
            })?;
    }

    let mut facts = Vec::new();
    for call in calls {
        let source = &sources[&call.file];
        let (line, character) = offset_to_position(source, call.offset);
        let registry_matches: Vec<_> = registries
            .iter()
            .filter(|(_, keys)| {
                call.keys
                    .as_ref()
                    .is_none_or(|call_keys| call_keys.iter().all(|key| keys.contains(key)))
            })
            .collect();
        if registry_matches.is_empty() {
            continue;
        }
        let hover = client
            .get_type_at_position(&file_uri(&call.file)?, line, character)
            .map_err(|error| {
                anyhow::anyhow!("tsgo hover failed for {}: {error}", call.file.display())
            })?;
        let proven = hover.as_ref().is_some_and(|hover| {
            contracts.iter().any(|contract| {
                contract.method == call.method
                    && hover.contains(&contract.owner)
                    && hover.contains(&format!(".{}", contract.method))
            })
        });
        let unresolved = hover.as_ref().is_none_or(|hover| hover.trim() == "any");
        if !proven && !unresolved {
            continue;
        }
        let location = offset_to_source_location(source, call.offset);
        for ((registry_file, registry_symbol), _) in registry_matches {
            facts.push(TypeScriptFact {
                file: call.file.clone(),
                consumer: call.consumer.clone(),
                registry_file: registry_file.clone(),
                registry_symbol: registry_symbol.clone(),
                keys: proven.then(|| call.keys.clone()).flatten(),
                line: location.0,
                column: location.1,
            });
        }
    }

    if facts.is_empty() {
        return Ok(None);
    }

    Ok(Some(TypeScriptFacts {
        indexed_registries: registries
            .keys()
            .map(|(file, symbol)| TypeScriptRegistry {
                file: file.clone(),
                symbol: symbol.clone(),
            })
            .collect(),
        facts,
    }))
}

fn collect_candidates(
    path: &Path,
    source: &str,
    contracts: &mut Vec<Contract>,
    calls: &mut Vec<Callsite>,
) -> Result<()> {
    let source_type = SourceType::from_path(path)
        .with_context(|| format!("unsupported TypeScript file {}", path.display()))?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        bail!("failed to parse {} for tsgo", path.display());
    }
    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    let scoping = semantic.scoping();
    let root_scope = scoping.root_scope_id();
    let top_level: Vec<_> = scoping
        .symbol_ids()
        .filter(|symbol| scoping.symbol_scope_id(*symbol) == root_scope)
        .map(|symbol| {
            (
                scoping.symbol_name(symbol).to_string(),
                semantic.symbol_declaration(symbol).kind().span(),
                scoping.symbol_flags(symbol).is_value(),
            )
        })
        .collect();

    for node in semantic.nodes().iter() {
        if let AstKind::TSMethodSignature(method) = node.kind()
            && !method.computed
            && method.type_parameters.as_ref().is_some_and(|params| {
                params
                    .params
                    .iter()
                    .any(|parameter| parameter.constraint.is_some())
            })
            && let Some(name) = method.key.static_name()
            && let Some(owner) = enclosing_symbol(&top_level, method.span, false)
        {
            contracts.push(Contract {
                owner,
                method: name.into_owned(),
            });
        }
        let AstKind::CallExpression(call) = node.kind() else {
            continue;
        };
        let callee = call.callee.get_inner_expression();
        let Some(member) = callee.as_member_expression() else {
            continue;
        };
        let Some((property_span, method)) = member.static_property_info() else {
            continue;
        };
        let keys = match call.arguments.first() {
            Some(Argument::StringLiteral(literal)) => Some(vec![literal.value.to_string()]),
            Some(Argument::TemplateLiteral(template)) if template.expressions.is_empty() => {
                template
                    .quasis
                    .first()
                    .and_then(|quasi| quasi.value.cooked)
                    .map(|value| vec![value.to_string()])
            }
            Some(Argument::ObjectExpression(_)) => continue,
            Some(Argument::Identifier(identifier))
                if identifier_is_object_value(&semantic, identifier) =>
            {
                continue;
            }
            Some(_) | None => None,
        };
        calls.push(Callsite {
            file: path.to_path_buf(),
            consumer: enclosing_symbol(&top_level, call.span, true)
                .unwrap_or_else(|| MODULE_INIT.to_string()),
            method: method.to_string(),
            keys,
            offset: property_span.start as usize,
        });
    }
    Ok(())
}

fn identifier_is_object_value(
    semantic: &oxc_semantic::Semantic<'_>,
    identifier: &IdentifierReference<'_>,
) -> bool {
    let Some(reference) = identifier.reference_id.get() else {
        return false;
    };
    let Some(symbol) = semantic.scoping().get_reference(reference).symbol_id() else {
        return false;
    };
    let declaration = semantic.symbol_declaration(symbol).kind().span();
    semantic.nodes().iter().any(|node| {
        let AstKind::VariableDeclarator(declarator) = node.kind() else {
            return false;
        };
        declarator.span.contains_inclusive(declaration)
            && declarator.init.as_ref().is_some_and(|value| {
                matches!(
                    value.get_inner_expression(),
                    Expression::ObjectExpression(_)
                )
            })
    })
}

fn enclosing_symbol(
    symbols: &[(String, Span, bool)],
    span: Span,
    runtime_only: bool,
) -> Option<String> {
    symbols
        .iter()
        .filter(|(_, declaration, runtime)| {
            (!runtime_only || *runtime) && declaration.contains_inclusive(span)
        })
        .min_by_key(|(_, declaration, _)| declaration.size())
        .map(|(name, _, _)| name.clone())
}

fn offset_to_position(source: &str, offset: usize) -> (u32, u32) {
    let prefix = source.get(..offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_prefix = prefix.rsplit('\n').next().unwrap_or_default();
    let character = line_prefix.encode_utf16().count() as u32;
    (line, character)
}

fn offset_to_source_location(source: &str, offset: usize) -> (usize, usize) {
    let (line, _) = offset_to_position(source, offset);
    let prefix = source.get(..offset).unwrap_or(source);
    let column = prefix
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        + 1;
    (line as usize + 1, column)
}

fn file_uri(path: &Path) -> Result<String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    url::Url::from_file_path(&path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow::anyhow!("cannot convert {} to a file URI", path.display()))
}

fn link_node_modules(source_root: &Path, compiler_root: &Path) {
    if source_root == compiler_root || source_root.join("node_modules").exists() {
        return;
    }
    let modules = compiler_root.join("node_modules");
    if !modules.exists() {
        return;
    }
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(modules, source_root.join("node_modules"));
    #[cfg(windows)]
    let _ = std::os::windows::fs::symlink_dir(modules, source_root.join("node_modules"));
}

fn find_tsgo() -> Option<PathBuf> {
    if let Some(path) = env::var_os("MONORIPPLE_TSGO") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = which::which("tsgo") {
        return Some(path);
    }
    let cache = env::var_os("HOME")
        .map(PathBuf::from)?
        .join(".bun/install/cache");
    let mut binaries: Vec<_> = fs::read_dir(cache)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("@typescript/native-preview-")
        })
        .map(|entry| entry.path().join("lib/tsgo"))
        .filter(|path| path.is_file())
        .collect();
    binaries.sort();
    binaries.pop()
}
