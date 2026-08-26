/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Collection of implementations for completions, and related types.

use std::collections::HashMap;
use std::path::Path;

use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use lsp_types::CompletionTextEdit;
use lsp_types::Documentation;
use lsp_types::MarkupContent;
use lsp_types::MarkupKind;
use lsp_types::Range;
use lsp_types::TextEdit;
use starlark::codemap::ResolvedSpan;
use starlark::docs::DocItem;
use starlark::docs::DocMember;
use starlark::docs::markdown::render_doc_item_no_link;
use starlark::docs::markdown::render_doc_param;
use starlark_syntax::codemap::ResolvedPos;
use starlark_syntax::syntax::ast::ArgumentP;
use starlark_syntax::syntax::ast::AssignTargetP;
use starlark_syntax::syntax::ast::ExprP;
use starlark_syntax::syntax::ast::StmtP;
use starlark_syntax::syntax::module::AstModuleFields;
use starlark_syntax::syntax::top_level_stmts::top_level_stmts;

use crate::definition::Definition;
use crate::definition::DottedDefinition;
use crate::definition::IdentifierDefinition;
use crate::definition::LspModule;
use crate::docs::get_doc_item_for_def;
use crate::exported::SymbolKind as ExportedSymbolKind;
use crate::server::Backend;
use crate::server::LspContext;
use crate::server::LspOpError;
use crate::server::LspUri;
use crate::symbols::SymbolKind;
use crate::symbols::find_symbols_at_location;

/// The context in which to offer string completion options.
#[derive(Debug, PartialEq)]
pub enum StringCompletionType {
    /// The first argument to a `load` statement.
    LoadPath,
    /// A string in another context.
    String,
}

/// A possible result in auto-complete for a string context.
#[derive(Debug, PartialEq)]
pub struct StringCompletionResult {
    /// The value to complete.
    pub value: String,
    /// The text to insert, if different from the value.
    pub insert_text: Option<String>,
    /// From where to start the insertion, compared to the start of the string.
    pub insert_text_offset: usize,
    /// The kind of result, e.g. a file vs a folder.
    pub kind: CompletionItemKind,
}

/// The result of scanning a line of source text leftwards from the cursor for a
/// member-access (dot) completion context.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DotContext {
    /// Cursor is after `root.` (possibly mid-word, e.g. `root.par`): offer the
    /// members of `root`.
    Member { root: String },
    /// A dot context that is deliberately not completed (chained access like
    /// `a.b.`, or inside a comment). Distinct from `None` so the caller
    /// suppresses the default completions instead of falling back to them.
    Suppress,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Scan `line` leftwards from the cursor at byte column `character` and
/// classify the completion context. `None` means "not a dot context" and the
/// caller falls through to the regular completion flow.
///
/// Works on raw text rather than the AST because the dot state (`x = yaml.`)
/// is a parse error — the AST is one keystroke stale, but the text is not.
/// Scanning bytes is safe: Starlark identifiers are ASCII, and UTF-8
/// continuation bytes never match `is_ident_byte`.
pub(crate) fn scan_dot_context(line: &str, character: u32) -> Option<DotContext> {
    let bytes = line.as_bytes();
    let cursor = (character as usize).min(bytes.len());

    // Skip back over the partially-typed member word (`re` of `helpers.re`).
    let mut i = cursor;
    while i > 0 && is_ident_byte(bytes[i - 1]) {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot = i - 1;

    // Scan back over the root identifier (`helpers` of `helpers.`).
    let mut root_start = dot;
    while root_start > 0 && is_ident_byte(bytes[root_start - 1]) {
        root_start -= 1;
    }
    let root = &line[root_start..dot];
    // Empty (`f().`, `"s".`) or number-like (`1.` is a float literal) roots
    // are not member access on an identifier.
    if root.is_empty() || root.as_bytes()[0].is_ascii_digit() {
        return None;
    }

    if root_start > 0 && bytes[root_start - 1] == b'.' {
        return Some(DotContext::Suppress);
    }

    // A `#` earlier on the line outside string quotes means the cursor is in a
    // comment. Line-local heuristic; multi-line strings are accepted noise.
    let mut in_single = false;
    let mut in_double = false;
    for &b in &bytes[..root_start] {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double => return Some(DotContext::Suppress),
            _ => {}
        }
    }

    Some(DotContext::Member {
        root: root.to_owned(),
    })
}

/// Kind and docs for the top-level symbol `name` in `module`.
///
/// Unlike `exported_symbols()`, underscore-prefixed names are included: struct
/// fields routinely reference private helpers (`render = _render`).
fn top_level_symbol_kind_and_docs(
    module: &LspModule,
    name: &str,
) -> Option<(ExportedSymbolKind, Option<DocItem>)> {
    for stmt in top_level_stmts(module.ast.statement()) {
        match &stmt.node {
            StmtP::Def(def) if def.name.ident == name => {
                let kind = ExportedSymbolKind::Function {
                    argument_names: def
                        .params
                        .iter()
                        .filter_map(|param| param.split().0.map(|n| n.to_string()))
                        .collect(),
                };
                let docs = get_doc_item_for_def(def, module.ast.codemap())
                    .map(|f| DocItem::Member(DocMember::Function(f)));
                return Some((kind, docs));
            }
            StmtP::Assign(assign) => {
                let mut kind = None;
                assign.lhs.visit_lvalue(|ident| {
                    if ident.ident == name {
                        kind = Some(ExportedSymbolKind::from_expr(&assign.rhs));
                    }
                });
                if let Some(kind) = kind {
                    return Some((kind, None));
                }
            }
            _ => {}
        }
    }
    None
}

/// Completion items for the fields of a `root = struct(field = value, ...)`
/// top-level assignment in `module`, in declaration order.
///
/// The completion analogue of `LspModule::find_exported_symbol_and_member`:
/// where a field's value is an identifier naming a top-level symbol (the
/// `render = _render` idiom used by builtin stubs and user modules alike), the
/// symbol's kind and docs are attached to the item.
pub(crate) fn struct_field_completion_items(module: &LspModule, root: &str) -> Vec<CompletionItem> {
    for stmt in top_level_stmts(module.ast.statement()) {
        let StmtP::Assign(assign) = &stmt.node else {
            continue;
        };
        let AssignTargetP::Identifier(lhs) = &assign.lhs.node else {
            continue;
        };
        if lhs.ident != root {
            continue;
        }
        let ExprP::Call(function, args) = &assign.rhs.node else {
            continue;
        };
        let ExprP::Identifier(function_name) = &function.node else {
            continue;
        };
        if function_name.node.ident != "struct" {
            continue;
        }

        return args
            .args
            .iter()
            .filter_map(|arg| {
                let ArgumentP::Named(field, value) = &arg.node else {
                    return None;
                };
                let referenced = match &value.node {
                    ExprP::Identifier(id) => top_level_symbol_kind_and_docs(module, &id.node.ident),
                    _ => None,
                };
                let (kind, documentation) = match referenced {
                    Some((kind, docs)) => {
                        let documentation = docs.map(|docs| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: render_doc_item_no_link(&field.node, &docs),
                            })
                        });
                        (CompletionItemKind::from(kind), documentation)
                    }
                    None => (CompletionItemKind::FIELD, None),
                };
                Some(CompletionItem {
                    label: field.node.clone(),
                    kind: Some(kind),
                    documentation,
                    ..Default::default()
                })
            })
            .collect();
    }
    Vec::new()
}

impl<T: LspContext> Backend<T> {
    pub(crate) fn default_completion_options(
        &self,
        document_uri: &LspUri,
        document: &LspModule,
        line: u32,
        character: u32,
        workspace_root: Option<&Path>,
    ) -> impl Iterator<Item = CompletionItem> + '_ + use<'_, T> {
        let cursor_position = ResolvedPos {
            line: line as usize,
            column: character as usize,
        };

        // Scan through current document
        let mut symbols: HashMap<_, _> = find_symbols_at_location(
            document.ast.codemap(),
            document.ast.statement(),
            cursor_position,
        )
        .into_iter()
        .map(|(key, value)| {
            (
                key,
                CompletionItem {
                    kind: Some(match value.kind {
                        SymbolKind::Method => CompletionItemKind::METHOD,
                        SymbolKind::Variable => CompletionItemKind::VARIABLE,
                    }),
                    detail: value.detail,
                    documentation: value
                        .doc
                        .map(|doc| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: render_doc_item_no_link(&value.name, &doc),
                            })
                        })
                        .or_else(|| {
                            value.param.map(|(starred_name, doc)| {
                                Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: render_doc_param(starred_name, &doc),
                                })
                            })
                        }),
                    label: value.name,
                    ..Default::default()
                },
            )
        })
        .collect();

        // Discover exported symbols from other documents
        let docs = self.last_valid_parse.read().unwrap();
        if docs.len() > 1 {
            // Find the position of the last load in the current file.
            let mut last_load = None;
            let mut loads = HashMap::new();
            document.ast.statement().visit_stmt(|node| {
                if let StmtP::Load(load) = &node.node {
                    last_load = Some(node.span);
                    loads.insert(load.module.node.clone(), (load.args.clone(), node.span));
                }
            });
            let last_load = last_load.map(|span| document.ast.codemap().resolve_span(span));

            symbols.extend(
                self.get_all_exported_symbols(
                    Some(document_uri),
                    &symbols,
                    workspace_root,
                    document_uri,
                    |module, symbol| {
                        Self::get_load_text_edit(
                            module,
                            symbol,
                            document,
                            last_load,
                            loads.get(module),
                        )
                    },
                )
                .into_iter()
                .map(|item| (item.label.clone(), item)),
            );
        }

        symbols
            .into_values()
            .chain(self.get_global_symbol_completion_items(document_uri))
            .chain(Self::get_keyword_completion_items())
    }

    pub(crate) fn exported_symbol_options(
        &self,
        load_path: &str,
        current_span: ResolvedSpan,
        previously_loaded: &[String],
        document_uri: &LspUri,
        workspace_root: Option<&Path>,
    ) -> Vec<CompletionItem> {
        self.context
            .resolve_load(load_path, document_uri, workspace_root)
            // FIXME(JakobDegen): Why are we throwing away errors?
            .map_err(|_| ())
            .and_then(|uri| self.get_ast_or_load_from_disk(&uri).map_err(|_| ()))
            .into_iter()
            .flatten()
            .flat_map(|ast| {
                ast.get_exported_symbols()
                    .into_iter()
                    .filter(|symbol| !previously_loaded.iter().any(|s| s == &symbol.name))
                    .map(|symbol| {
                        let mut item: CompletionItem = symbol.into();
                        item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                            range: current_span.into(),
                            new_text: item.label.clone(),
                        }));
                        item
                    })
            })
            .collect()
    }

    pub(crate) fn parameter_name_options(
        &self,
        function_name_span: &ResolvedSpan,
        document: &LspModule,
        document_uri: &LspUri,
        previously_used_named_parameters: &[String],
        workspace_root: Option<&Path>,
    ) -> impl Iterator<Item = CompletionItem> + use<T> {
        match document.find_definition_at_location(
            function_name_span.begin.line as u32,
            function_name_span.begin.column as u32,
        ) {
            Definition::Identifier(identifier) => self
                .parameter_name_options_for_identifier_definition(
                    &identifier,
                    document,
                    document_uri,
                    previously_used_named_parameters,
                    workspace_root,
                )
                .unwrap_or_default(),
            Definition::Dotted(DottedDefinition {
                root_definition_location,
                ..
            }) => self
                .parameter_name_options_for_identifier_definition(
                    &root_definition_location,
                    document,
                    document_uri,
                    previously_used_named_parameters,
                    workspace_root,
                )
                .unwrap_or_default(),
        }
        .into_iter()
        .flatten()
    }

    fn parameter_name_options_for_identifier_definition(
        &self,
        identifier_definition: &IdentifierDefinition,
        document: &LspModule,
        document_uri: &LspUri,
        previously_used_named_parameters: &[String],
        workspace_root: Option<&Path>,
    ) -> Result<Option<Vec<CompletionItem>>, LspOpError> {
        Ok(match identifier_definition {
            IdentifierDefinition::Location {
                destination, name, ..
            } => {
                // Can we resolve it again at that location?
                // TODO: This seems very inefficient. Once the document starts
                // holding the `Scope` including AST nodes, this indirection
                // should be removed.
                find_symbols_at_location(
                    document.ast.codemap(),
                    document.ast.statement(),
                    ResolvedPos {
                        line: destination.begin.line,
                        column: destination.begin.column,
                    },
                )
                .remove(name)
                .and_then(|symbol| match symbol.kind {
                    SymbolKind::Method => symbol.doc,
                    SymbolKind::Variable => None,
                })
                .and_then(|docs| match docs {
                    DocItem::Member(DocMember::Function(doc_function)) => Some(
                        doc_function
                            .params
                            .regular_params()
                            .filter(|p| !previously_used_named_parameters.contains(&p.name))
                            .map(|p| CompletionItem {
                                label: p.name.to_owned(),
                                kind: Some(CompletionItemKind::PROPERTY),
                                ..Default::default()
                            })
                            .collect(),
                    ),
                    _ => None,
                })
            }
            IdentifierDefinition::LoadedLocation { path, name, .. } => {
                let load_uri = self.resolve_load_path(path, document_uri, workspace_root)?;
                self.get_ast_or_load_from_disk(&load_uri)?
                    .and_then(|ast| ast.find_exported_symbol(name))
                    .and_then(|symbol| match symbol.kind {
                        ExportedSymbolKind::Any => None,
                        ExportedSymbolKind::Function { argument_names } => Some(
                            argument_names
                                .into_iter()
                                .map(|name| CompletionItem {
                                    label: name,
                                    kind: Some(CompletionItemKind::PROPERTY),
                                    ..Default::default()
                                })
                                .collect(),
                        ),
                    })
            }
            IdentifierDefinition::Unresolved { name, .. } => {
                // Maybe it's a global symbol.
                match self
                    .context
                    .get_environment(document_uri)
                    .members
                    .into_iter()
                    .find(|symbol| &symbol.0 == name)
                {
                    Some(symbol) => match symbol.1 {
                        DocItem::Member(DocMember::Function(doc_function)) => Some(
                            doc_function
                                .params
                                .regular_params()
                                .map(|param| CompletionItem {
                                    label: param.name.to_owned(),
                                    kind: Some(CompletionItemKind::PROPERTY),
                                    ..Default::default()
                                })
                                .collect(),
                        ),
                        _ => None,
                    },
                    _ => None,
                }
            }
            // None of these can be functions, so can't have any parameters.
            IdentifierDefinition::LoadPath { .. }
            | IdentifierDefinition::StringLiteral { .. }
            | IdentifierDefinition::NotFound => None,
        })
    }

    pub(crate) fn string_completion_options(
        &self,
        document_uri: &LspUri,
        kind: StringCompletionType,
        current_value: &str,
        current_span: ResolvedSpan,
        workspace_root: Option<&Path>,
    ) -> Result<Vec<CompletionItem>, LspOpError> {
        Ok(self
            .context
            .get_string_completion_options(document_uri, kind, current_value, workspace_root)
            .map_err(LspOpError::FromContext)?
            .into_iter()
            .map(|result| {
                let mut range: Range = current_span.into();
                range.start.character += result.insert_text_offset as u32;

                CompletionItem {
                    label: result.value.clone(),
                    kind: Some(result.kind),
                    insert_text: result.insert_text.clone(),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range,
                        new_text: result.insert_text.unwrap_or(result.value),
                    })),
                    ..Default::default()
                }
            })
            .collect())
    }

    pub(crate) fn type_completion_options() -> impl Iterator<Item = CompletionItem> {
        ["str", "int", "bool", "None", "float"]
            .into_iter()
            .map(|type_| CompletionItem {
                label: type_.to_owned(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                ..Default::default()
            })
    }

    /// Completion items for `root.` where `root` is a symbol loaded by
    /// `document`. Empty when the root is not a loaded symbol or the loaded
    /// module has no matching `name = struct(...)` — deliberately not falling
    /// back to default completions, which are noise after a dot.
    pub(crate) fn member_completion_options(
        &self,
        document: &LspModule,
        document_uri: &LspUri,
        root: &str,
        workspace_root: Option<&Path>,
    ) -> Vec<CompletionItem> {
        // Match load statements by name rather than resolving the root by
        // position: completion positions come from the latest text while
        // `document` is the last valid parse, so positions can be stale but
        // names cannot. `load("m", h = "helpers")` binds local `h` to remote
        // `helpers`; the struct lookup needs the remote name.
        let Some((load_path, remote_name)) = top_level_stmts(document.ast.statement())
            .into_iter()
            .find_map(|stmt| {
                let StmtP::Load(load) = &stmt.node else {
                    return None;
                };
                load.args.iter().find_map(|arg| {
                    (arg.local.ident == root)
                        .then(|| (load.module.node.clone(), arg.their.node.clone()))
                })
            })
        else {
            return Vec::new();
        };
        let Ok(load_uri) = self.resolve_load_path(&load_path, document_uri, workspace_root) else {
            return Vec::new();
        };
        let Ok(Some(loaded)) = self.get_ast_or_load_from_disk(&load_uri) else {
            return Vec::new();
        };
        struct_field_completion_items(&loaded, &remote_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(root: &str) -> Option<DotContext> {
        Some(DotContext::Member {
            root: root.to_owned(),
        })
    }

    #[test]
    fn test_scan_dot_context_member() {
        assert_eq!(scan_dot_context("x = yaml.", 9), member("yaml"));
        assert_eq!(scan_dot_context("x = yaml.du", 11), member("yaml"));
        assert_eq!(scan_dot_context("yaml.", 5), member("yaml"));
        assert_eq!(scan_dot_context("f(yaml.", 7), member("yaml"));
        // Cursor mid-word: everything right of the cursor is ignored.
        assert_eq!(scan_dot_context("x = yaml.dump", 10), member("yaml"));
    }

    #[test]
    fn test_scan_dot_context_none() {
        assert_eq!(scan_dot_context("x = yaml", 8), None);
        assert_eq!(scan_dot_context("", 0), None);
        assert_eq!(scan_dot_context("x = ", 4), None);
        // Not an identifier root.
        assert_eq!(scan_dot_context("f().", 4), None);
        assert_eq!(scan_dot_context("\"s\".", 4), None);
        // Float literal, not member access.
        assert_eq!(scan_dot_context("x = 1.", 6), None);
        // Cursor beyond line end is clamped.
        assert_eq!(scan_dot_context("x", 5), None);
    }

    #[test]
    fn test_scan_dot_context_suppressed() {
        // Chained access: recognised, deliberately not completed.
        assert_eq!(scan_dot_context("a.b.", 4), Some(DotContext::Suppress));
        // Comments.
        assert_eq!(scan_dot_context("# yaml.", 7), Some(DotContext::Suppress));
        assert_eq!(
            scan_dot_context("x = 1  # yaml.", 14),
            Some(DotContext::Suppress)
        );
    }

    #[test]
    fn test_scan_dot_context_hash_inside_string_is_not_a_comment() {
        assert_eq!(scan_dot_context("x = \"#\" + yaml.", 15), member("yaml"));
    }

    use starlark::syntax::AstModule;
    use starlark::syntax::Dialect;

    use crate::definition::LspModule;

    fn lsp_module(source: &str) -> LspModule {
        LspModule::new(
            AstModule::parse("t.star", source.to_owned(), &Dialect::AllOptionsInternal).unwrap(),
        )
    }

    #[test]
    fn test_struct_field_completion_items() {
        let module = lsp_module(
            r#"
def _render(config):
    """Render the config."""
    pass

helpers = struct(
    render = _render,
    version = "1.0",
)
"#,
        );
        let items = struct_field_completion_items(&module, "helpers");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "render");
        assert_eq!(items[0].kind, Some(CompletionItemKind::FUNCTION));
        assert!(
            items[0].documentation.is_some(),
            "docstring of _render should be attached"
        );
        assert_eq!(items[1].label, "version");
        assert_eq!(items[1].kind, Some(CompletionItemKind::FIELD));

        assert!(struct_field_completion_items(&module, "nope").is_empty());
    }

    #[test]
    fn test_struct_field_completion_items_non_struct_assign() {
        let module = lsp_module("helpers = [1, 2]\n");
        assert!(struct_field_completion_items(&module, "helpers").is_empty());
    }
}
