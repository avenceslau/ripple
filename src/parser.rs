use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_parser::{Parser, config::TokensParserConfig};
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};
use oxc_syntax::module_record::{ExportExportName, ExportImportName, ImportImportName};
use serde::{Deserialize, Serialize};

pub const MODULE_INIT: &str = "<module>";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct SourceLocation {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct SymbolInfo {
    pub fingerprint: String,
    pub dependencies: BTreeSet<String>,
    pub dependency_locations: BTreeMap<String, SourceLocation>,
    pub type_dependencies: BTreeSet<String>,
    pub type_dependency_locations: BTreeMap<String, SourceLocation>,
    pub runtime: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ImportedName {
    Named(String),
    Namespace { members: Option<BTreeSet<String>> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportBinding {
    pub source: String,
    pub imported: ImportedName,
    pub location: SourceLocation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReExport {
    pub exported: String,
    pub source: String,
    pub imported: ImportedName,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeLoad {
    pub source: String,
    pub consumers: BTreeSet<String>,
    pub location: SourceLocation,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ParsedModule {
    pub symbols: BTreeMap<String, SymbolInfo>,
    pub imports: BTreeMap<String, ImportBinding>,
    pub type_imports: BTreeMap<String, ImportBinding>,
    pub local_exports: BTreeMap<String, String>,
    pub type_local_exports: BTreeMap<String, String>,
    pub re_exports: Vec<ReExport>,
    pub type_re_exports: Vec<ReExport>,
    pub star_exports: Vec<String>,
    pub type_star_exports: Vec<String>,
    pub module_requests: BTreeSet<String>,
    pub runtime_loads: Vec<RuntimeLoad>,
    pub unresolved_dynamic_imports: Vec<String>,
}

pub fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

pub fn is_test_file(path: &Path) -> bool {
    let in_test_directory = path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("test" | "tests" | "__tests__")
        )
    });
    if in_test_directory {
        return true;
    }

    path.file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".test") || name.ends_with(".spec"))
}

pub fn parse_module(path: &Path, source: &str) -> Result<ParsedModule> {
    let source_type = SourceType::from_path(path)
        .with_context(|| format!("unsupported source file {}", path.display()))?;
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type)
        .with_config(TokensParserConfig)
        .parse();

    if !parsed.errors.is_empty() {
        bail!("failed to parse {}: {}", path.display(), parsed.errors[0]);
    }

    let module_record = parsed.module_record;
    let semantic = SemanticBuilder::new().build(&parsed.program);

    if !semantic.errors.is_empty() {
        bail!(
            "failed semantic analysis for {}: {}",
            path.display(),
            semantic.errors[0]
        );
    }

    let semantic = semantic.semantic;
    let scoping = semantic.scoping();
    let root_scope = scoping.root_scope_id();
    let mut top_level = Vec::new();

    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root_scope {
            continue;
        }

        let name = scoping.symbol_name(symbol_id).to_string();
        let span = semantic.symbol_declaration(symbol_id).kind().span();
        let runtime = scoping.symbol_flags(symbol_id).is_value()
            && !scoping.symbol_flags(symbol_id).is_type_import();
        top_level.push((symbol_id, name, span, runtime));
    }

    let runtime_referenced_symbols: BTreeSet<_> = top_level
        .iter()
        .filter(|(symbol_id, _, _, _)| {
            semantic
                .symbol_references(*symbol_id)
                .any(|reference| !reference.flags().is_type_only())
        })
        .map(|(_, name, _, _)| name.clone())
        .collect();
    let type_referenced_symbols: BTreeSet<_> = top_level
        .iter()
        .filter(|(symbol_id, _, _, _)| {
            semantic
                .symbol_references(*symbol_id)
                .any(|reference| reference.flags().is_type_only())
        })
        .map(|(_, name, _, _)| name.clone())
        .collect();

    let mut symbols = BTreeMap::new();
    for (_, name, span, runtime) in &top_level {
        let fingerprint = source
            .get(span.start as usize..span.end as usize)
            .unwrap_or_default()
            .to_string();
        symbols.insert(
            name.clone(),
            SymbolInfo {
                fingerprint,
                dependencies: BTreeSet::new(),
                dependency_locations: BTreeMap::new(),
                type_dependencies: BTreeSet::new(),
                type_dependency_locations: BTreeMap::new(),
                runtime: *runtime,
            },
        );
    }

    for (dependency_id, dependency_name, _, dependency_runtime) in &top_level {
        if !dependency_runtime {
            continue;
        }

        for reference in semantic.symbol_references(*dependency_id) {
            if reference.flags().is_type_only() {
                continue;
            }

            let reference_span = semantic.reference_span(reference);
            for (_, consumer_name, consumer_span, consumer_runtime) in &top_level {
                if !consumer_runtime || consumer_name == dependency_name {
                    continue;
                }

                if consumer_span.start <= reference_span.start
                    && consumer_span.end >= reference_span.end
                    && let Some(consumer) = symbols.get_mut(consumer_name)
                {
                    consumer.dependencies.insert(dependency_name.clone());
                    consumer
                        .dependency_locations
                        .entry(dependency_name.clone())
                        .or_insert_with(|| source_location(source, reference_span.start as usize));
                }
            }
        }
    }

    for (dependency_id, dependency_name, _, _) in &top_level {
        for reference in semantic.symbol_references(*dependency_id) {
            if !reference.flags().is_type_only() {
                continue;
            }

            let reference_span = semantic.reference_span(reference);
            for (_, consumer_name, consumer_span, _) in &top_level {
                if consumer_name == dependency_name {
                    continue;
                }

                if consumer_span.start <= reference_span.start
                    && consumer_span.end >= reference_span.end
                    && let Some(consumer) = symbols.get_mut(consumer_name)
                {
                    consumer.type_dependencies.insert(dependency_name.clone());
                    consumer
                        .type_dependency_locations
                        .entry(dependency_name.clone())
                        .or_insert_with(|| source_location(source, reference_span.start as usize));
                }
            }
        }
    }

    let mut imports = BTreeMap::new();
    let mut type_imports = BTreeMap::new();
    for entry in &module_record.import_entries {
        let imported = match &entry.import_name {
            ImportImportName::Name(name) => ImportedName::Named(name.name.to_string()),
            ImportImportName::Default(_) => ImportedName::Named("default".to_string()),
            ImportImportName::NamespaceObject => {
                let members = top_level
                    .iter()
                    .find(|(_, name, _, _)| name == entry.local_name.name.as_str())
                    .and_then(|(symbol_id, _, _, _)| {
                        let mut members = BTreeSet::new();
                        for reference in semantic.symbol_references(*symbol_id) {
                            if reference.flags().is_type_only() {
                                continue;
                            }

                            let parent = semantic.nodes().parent_kind(reference.node_id());
                            let name = if let Some(member) = parent.as_member_expression_kind() {
                                let name = member.static_property_name()?;
                                name.to_string()
                            } else if let AstKind::JSXMemberExpression(member) = parent {
                                member.property.name.to_string()
                            } else {
                                return None;
                            };
                            members.insert(name);
                        }
                        Some(members)
                    });
                ImportedName::Namespace { members }
            }
        };
        let binding = ImportBinding {
            source: entry.module_request.name.to_string(),
            imported,
            location: source_location(source, entry.statement_span.start as usize),
        };
        let local_name = entry.local_name.name.to_string();
        if !entry.is_type {
            imports.insert(local_name.clone(), binding.clone());
        }
        if entry.is_type || type_referenced_symbols.contains(&local_name) {
            type_imports.insert(local_name, binding);
        }
    }

    let mut local_exports = BTreeMap::new();
    let mut type_local_exports = BTreeMap::new();
    for entry in &module_record.local_export_entries {
        let Some(local_name) = entry.local_name.name().map(|name| name.to_string()) else {
            continue;
        };
        let Some(exported_name) = export_name(&entry.export_name) else {
            continue;
        };
        if !entry.is_type {
            local_exports.insert(exported_name.clone(), local_name.clone());
        }
        type_local_exports.insert(exported_name, local_name);
    }

    let mut re_exports = Vec::new();
    let mut type_re_exports = Vec::new();
    for entry in &module_record.indirect_export_entries {
        let Some(source) = entry
            .module_request
            .as_ref()
            .map(|request| request.name.to_string())
        else {
            continue;
        };
        let Some(exported) = export_name(&entry.export_name) else {
            continue;
        };
        let imported = match &entry.import_name {
            ExportImportName::Name(name) => ImportedName::Named(name.name.to_string()),
            ExportImportName::All => ImportedName::Namespace { members: None },
            ExportImportName::AllButDefault | ExportImportName::Null => continue,
        };
        let re_export = ReExport {
            exported,
            source,
            imported,
        };
        if !entry.is_type {
            re_exports.push(re_export.clone());
        }
        type_re_exports.push(re_export);
    }

    let star_exports = module_record
        .star_export_entries
        .iter()
        .filter(|entry| !entry.is_type)
        .filter_map(|entry| {
            entry
                .module_request
                .as_ref()
                .map(|request| request.name.to_string())
        })
        .collect();
    let type_star_exports = module_record
        .star_export_entries
        .iter()
        .filter_map(|entry| {
            entry
                .module_request
                .as_ref()
                .map(|request| request.name.to_string())
        })
        .collect();

    let mut module_requests = BTreeSet::new();
    for entry in &module_record.import_entries {
        let local_name = entry.local_name.name.as_str();
        let has_runtime_presence = runtime_referenced_symbols.contains(local_name)
            || !type_referenced_symbols.contains(local_name);
        if !entry.is_type && has_runtime_presence {
            module_requests.insert(entry.module_request.name.to_string());
        }
    }
    for entry in module_record
        .indirect_export_entries
        .iter()
        .chain(module_record.star_export_entries.iter())
    {
        if !entry.is_type
            && let Some(request) = &entry.module_request
        {
            module_requests.insert(request.name.to_string());
        }
    }
    for (name, requests) in &module_record.requested_modules {
        let has_side_effect_import = requests.iter().any(|request| {
            request.is_import
                && !request.is_type
                && !module_record
                    .import_entries
                    .iter()
                    .any(|entry| entry.statement_span == request.statement_span)
        });
        if has_side_effect_import {
            module_requests.insert(name.to_string());
        }
    }
    let mut runtime_load_spans = Vec::new();
    let mut unresolved_dynamic_imports = Vec::new();
    for import in &module_record.dynamic_imports {
        if let Some(request) =
            source.get(import.module_request.start as usize..import.module_request.end as usize)
        {
            let quoted = (request.starts_with('\'') && request.ends_with('\''))
                || (request.starts_with('"') && request.ends_with('"'));
            if quoted {
                runtime_load_spans.push((request[1..request.len() - 1].to_string(), import.span));
            } else {
                unresolved_dynamic_imports.push(request.to_string());
            }
        }
    }
    for node in semantic.nodes().iter() {
        if let AstKind::CallExpression(call) = node.kind()
            && let Some(request) = call.common_js_require()
        {
            runtime_load_spans.push((request.value.to_string(), call.span));
        }
    }

    let declaration_spans: Vec<_> = top_level
        .iter()
        .map(|(_, _, span, _)| (span.start as usize, span.end as usize))
        .collect();
    let declaration_statement_spans: Vec<_> = parsed
        .program
        .body
        .iter()
        .map(GetSpan::span)
        .filter(|statement_span| {
            declaration_spans
                .iter()
                .any(|(declaration_start, declaration_end)| {
                    statement_span.start as usize <= *declaration_start
                        && statement_span.end as usize >= *declaration_end
                })
        })
        .collect();

    let mut module_dependencies = BTreeSet::new();
    let mut module_dependency_locations = BTreeMap::new();
    for (symbol_id, name, _, runtime) in &top_level {
        if !runtime {
            continue;
        }

        let location = semantic
            .symbol_references(*symbol_id)
            .find_map(|reference| {
                if reference.flags().is_type_only() {
                    return None;
                }
                let span = semantic.reference_span(reference);
                let belongs_to_declaration =
                    declaration_statement_spans.iter().any(|statement_span| {
                        statement_span.start <= span.start && statement_span.end >= span.end
                    });
                let belongs_to_export_specifier = semantic
                    .nodes()
                    .ancestor_kinds(reference.node_id())
                    .any(|kind| matches!(kind, AstKind::ExportSpecifier(_)));
                (!belongs_to_declaration && !belongs_to_export_specifier)
                    .then(|| source_location(source, span.start as usize))
            });
        if let Some(location) = location {
            module_dependencies.insert(name.clone());
            module_dependency_locations.insert(name.clone(), location);
        }
    }
    for entry in &module_record.import_entries {
        let local_name = entry.local_name.name.as_str();
        if !entry.is_type
            && !runtime_referenced_symbols.contains(local_name)
            && !type_referenced_symbols.contains(local_name)
        {
            let name = local_name.to_string();
            module_dependencies.insert(name.clone());
            module_dependency_locations.insert(
                name,
                source_location(source, entry.statement_span.start as usize),
            );
        }
    }

    let mut runtime_loads = Vec::new();
    for (request, span) in runtime_load_spans {
        let mut consumers: BTreeSet<_> = top_level
            .iter()
            .filter(|(_, _, declaration, runtime)| {
                *runtime && declaration.start <= span.start && declaration.end >= span.end
            })
            .map(|(_, name, _, _)| name.clone())
            .collect();
        if consumers.is_empty() {
            consumers.insert(MODULE_INIT.to_string());
        }
        runtime_loads.push(RuntimeLoad {
            source: request,
            consumers,
            location: source_location(source, span.start as usize),
        });
    }

    let mut module_fingerprint = String::new();
    for token in &parsed.tokens {
        let span = token.span();
        let belongs_to_declaration = declaration_statement_spans.iter().any(|statement_span| {
            statement_span.start <= span.start && statement_span.end >= span.end
        });
        if !belongs_to_declaration {
            module_fingerprint.push_str(
                source
                    .get(span.start as usize..span.end as usize)
                    .unwrap_or_default(),
            );
        }
    }
    for re_export in &re_exports {
        module_fingerprint.push_str(&format!(
            "reexport:{}:{}:{:?};",
            re_export.exported, re_export.source, re_export.imported
        ));
    }
    for source in &star_exports {
        module_fingerprint.push_str(&format!("star:{source};"));
    }
    for request in &module_requests {
        module_fingerprint.push_str(&format!("request:{request};"));
    }

    symbols.insert(
        MODULE_INIT.to_string(),
        SymbolInfo {
            fingerprint: module_fingerprint.clone(),
            dependencies: module_dependencies,
            dependency_locations: module_dependency_locations,
            type_dependencies: BTreeSet::new(),
            type_dependency_locations: BTreeMap::new(),
            runtime: true,
        },
    );

    Ok(ParsedModule {
        symbols,
        imports,
        type_imports,
        local_exports,
        type_local_exports,
        re_exports,
        type_re_exports,
        star_exports,
        type_star_exports,
        module_requests,
        runtime_loads,
        unresolved_dynamic_imports,
    })
}

fn source_location(source: &str, offset: usize) -> SourceLocation {
    let prefix = source.get(..offset).unwrap_or(source);
    SourceLocation {
        line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
        column: prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1,
    }
}

fn export_name(name: &ExportExportName<'_>) -> Option<String> {
    match name {
        ExportExportName::Name(name) => Some(name.name.to_string()),
        ExportExportName::Default(_) => Some("default".to_string()),
        ExportExportName::Null => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_symbol_dependencies_and_runtime_imports() {
        let module = parse_module(
            Path::new("fixture.ts"),
            r#"
                import { value, type Shape } from './dependency';
                const unused = 1;
                export const result = value + 1;
                export type Result = Shape;
            "#,
        )
        .unwrap();

        assert_eq!(
            module.imports["value"].imported,
            ImportedName::Named("value".to_string())
        );
        assert!(!module.imports.contains_key("Shape"));
        assert!(module.symbols["result"].dependencies.contains("value"));
        assert_eq!(module.imports["value"].location.line, 2);
        assert_eq!(
            module.symbols["result"].dependency_locations["value"].line,
            4
        );
        assert!(!module.symbols["Result"].runtime);
        assert_eq!(module.local_exports["result"], "result");
        assert!(!module.local_exports.contains_key("Result"));
    }

    #[test]
    fn records_reexports() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "export { value as renamed } from './dependency'; export * from './other';",
        )
        .unwrap();

        assert_eq!(module.re_exports[0].exported, "renamed");
        assert_eq!(module.star_exports, vec!["./other"]);
    }

    #[test]
    fn comments_do_not_change_fingerprints() {
        let before = parse_module(
            Path::new("fixture.ts"),
            "// old wording\nconst schema = { name: 'view' };",
        )
        .unwrap();
        let after = parse_module(
            Path::new("fixture.ts"),
            "// new wording\nconst schema = { name: 'view' };",
        )
        .unwrap();

        assert_eq!(before.symbols, after.symbols);
    }

    #[test]
    fn type_position_does_not_create_runtime_module_request() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "import { Shape } from './types'; export type Result = Shape;",
        )
        .unwrap();

        assert!(module.module_requests.is_empty());
    }

    #[test]
    fn narrows_static_namespace_members() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "import * as values from './values'; export const result = values.used;",
        )
        .unwrap();

        assert_eq!(
            module.imports["values"].imported,
            ImportedName::Namespace {
                members: Some(BTreeSet::from(["used".to_string()])),
            }
        );
    }

    #[test]
    fn dynamic_namespace_access_uses_all_exports() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "import * as values from './values'; export const result = values[name];",
        )
        .unwrap();

        assert_eq!(
            module.imports["values"].imported,
            ImportedName::Namespace { members: None }
        );
    }

    #[test]
    fn module_initialization_depends_on_top_level_references() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "const handler = () => 'ok'; export const router = {}; router.handler = handler;",
        )
        .unwrap();

        assert!(module.symbols[MODULE_INIT].dependencies.contains("handler"));
    }

    #[test]
    fn records_dynamic_import_and_commonjs_consumers() {
        let module = parse_module(
            Path::new("fixture.ts"),
            "export async function load() { return import('./lazy'); } export const legacy = require('./legacy');",
        )
        .unwrap();

        assert!(
            module
                .runtime_loads
                .iter()
                .any(|load| { load.source == "./lazy" && load.consumers.contains("load") })
        );
        assert!(
            module
                .runtime_loads
                .iter()
                .any(|load| { load.source == "./legacy" && load.consumers.contains("legacy") })
        );
        assert!(!module.module_requests.contains("./lazy"));
    }
}
