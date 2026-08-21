//! LSP protocol types for tsgo communication.
//!
//! This module defines the JSON-RPC request and response structures
//! used to communicate with the tsgo LSP server.

#![allow(dead_code)] // Protocol fields may not be used but are required for serialization

use serde::{Deserialize, Serialize};

/// JSON-RPC request structure.
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest<T> {
    pub jsonrpc: &'static str,
    pub id: u32,
    pub method: &'static str,
    pub params: T,
}

impl<T> JsonRpcRequest<T> {
    pub fn new(id: u32, method: &'static str, params: T) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// JSON-RPC notification structure (no id, no response expected).
#[derive(Debug, Serialize)]
pub struct JsonRpcNotification<T> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: T,
}

impl<T> JsonRpcNotification<T> {
    pub fn new(method: &'static str, params: T) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

/// JSON-RPC ID which can be either a number or string (LSP spec allows both).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u32),
    String(String),
}

/// JSON-RPC response structure.
#[derive(Debug, Deserialize)]
pub struct JsonRpcResponse<T> {
    pub jsonrpc: String,
    pub id: Option<JsonRpcId>,
    pub result: Option<T>,
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// ============================================================================
// LSP Initialize
// ============================================================================

/// Parameters for the `initialize` request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub process_id: Option<u32>,
    pub root_uri: Option<String>,
    pub capabilities: ClientCapabilities,
}

/// Client capabilities sent during initialization.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub text_document: TextDocumentClientCapabilities,
}

/// Text document related client capabilities.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentClientCapabilities {
    pub hover: HoverClientCapabilities,
}

/// Hover-specific client capabilities.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverClientCapabilities {
    pub content_format: Vec<String>,
}

/// Result of the `initialize` request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
}

/// Server capabilities returned after initialization.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub hover_provider: Option<bool>,
    pub text_document_sync: Option<TextDocumentSyncOptions>,
}

/// Text document sync options.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentSyncOptions {
    pub open_close: Option<bool>,
    pub change: Option<u32>,
}

// ============================================================================
// Text Document Operations
// ============================================================================

/// Parameters for `textDocument/didOpen` notification.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

/// A text document item.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: i32,
    pub text: String,
}

/// Parameters for `textDocument/didClose` notification.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

/// Parameters for `textDocument/didChange` notification.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: VersionedTextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

/// A versioned text document identifier.
#[derive(Debug, Serialize)]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
    pub version: i32,
}

/// A text document content change event.
/// When range is None, the entire document content is replaced.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentContentChangeEvent {
    /// The range of the document that changed. If None, the whole document changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    /// The new text of the range/document.
    pub text: String,
}

/// A text document identifier.
#[derive(Debug, Serialize, Clone)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

// ============================================================================
// Hover
// ============================================================================

/// Parameters for `textDocument/hover` request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

/// A position in a text document.
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Result of the `textDocument/hover` request.
#[derive(Debug, Deserialize)]
pub struct Hover {
    pub contents: HoverContents,
    pub range: Option<Range>,
}

/// Hover contents can be a single value or an array.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum HoverContents {
    /// A single MarkedString or MarkupContent
    Single(MarkedStringOrMarkup),
    /// An array of MarkedStrings
    Array(Vec<MarkedStringOrMarkup>),
}

/// A marked string or markup content.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MarkedStringOrMarkup {
    /// Plain string
    String(String),
    /// Markup content with kind
    MarkupContent(MarkupContent),
    /// Marked string with language
    MarkedString(MarkedString),
}

/// Markup content with a kind (plaintext or markdown).
#[derive(Debug, Deserialize)]
pub struct MarkupContent {
    pub kind: String,
    pub value: String,
}

/// A marked string with optional language.
#[derive(Debug, Deserialize)]
pub struct MarkedString {
    pub language: String,
    pub value: String,
}

/// A range in a text document.
#[derive(Debug, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

// ============================================================================
// Shutdown
// ============================================================================

/// Empty params for shutdown request.
#[derive(Debug, Serialize)]
pub struct ShutdownParams {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_params_serialization() {
        let params = InitializeParams {
            process_id: Some(12345),
            root_uri: Some("file:///path/to/project".to_string()),
            capabilities: ClientCapabilities {
                text_document: TextDocumentClientCapabilities {
                    hover: HoverClientCapabilities {
                        content_format: vec!["plaintext".to_string()],
                    },
                },
            },
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("processId"));
        assert!(json.contains("rootUri"));
        assert!(json.contains("textDocument"));
    }

    #[test]
    fn test_hover_response_deserialization() {
        let json = r#"{
            "contents": {
                "kind": "plaintext",
                "value": "const x: string"
            },
            "range": {
                "start": {"line": 0, "character": 6},
                "end": {"line": 0, "character": 7}
            }
        }"#;

        let hover: Hover = serde_json::from_str(json).unwrap();
        match hover.contents {
            HoverContents::Single(MarkedStringOrMarkup::MarkupContent(mc)) => {
                assert_eq!(mc.value, "const x: string");
            }
            _ => panic!("Expected MarkupContent"),
        }
    }

    #[test]
    fn test_hover_response_string_contents() {
        let json = r#"{
            "contents": "const x: string"
        }"#;

        let hover: Hover = serde_json::from_str(json).unwrap();
        match hover.contents {
            HoverContents::Single(MarkedStringOrMarkup::String(s)) => {
                assert_eq!(s, "const x: string");
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn test_hover_response_array_contents() {
        let json = r#"{
            "contents": [
                {"language": "typescript", "value": "const x: string"},
                "Documentation for x"
            ]
        }"#;

        let hover: Hover = serde_json::from_str(json).unwrap();
        match hover.contents {
            HoverContents::Array(arr) => {
                assert_eq!(arr.len(), 2);
            }
            _ => panic!("Expected Array"),
        }
    }
}
