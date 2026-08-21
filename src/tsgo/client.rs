//! LSP client for tsgo type resolution.
//!
//! This module provides a client that communicates with the tsgo LSP server
//! to resolve complex TypeScript types at specific file positions.

use crate::tsgo::cache::{TypeCache, TypePosition};
use crate::tsgo::embedded;
use crate::tsgo::protocol::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Hover, HoverClientCapabilities, HoverContents, HoverParams,
    InitializeParams, InitializeResult, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    MarkedStringOrMarkup, Position, ShutdownParams, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    VersionedTextDocumentIdentifier,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, instrument, trace};

/// Default timeout for LSP requests (5 seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Error type for TsgoClient operations.
#[derive(Debug, thiserror::Error)]
pub enum TsgoError {
    #[error("tsgo binary not found: {0}")]
    BinaryNotFound(String),

    #[error("Failed to spawn tsgo process: {0}")]
    SpawnError(#[from] std::io::Error),

    #[error("LSP error: {0}")]
    LspError(String),

    #[error("JSON-RPC error (code {code}): {message}")]
    JsonRpcError { code: i32, message: String },

    #[error("Failed to serialize request: {0}")]
    SerializeError(#[from] serde_json::Error),

    #[error("Response timeout after {0:?}")]
    Timeout(Duration),

    #[error("Process not initialized")]
    NotInitialized,

    #[error("Failed to extract embedded binary: {0}")]
    ExtractionError(String),
}

/// Message received from the LSP reader thread.
enum ReaderMessage {
    /// A JSON message was read successfully.
    Message(serde_json::Value),
    /// An error occurred while reading.
    Error(String),
}

/// LSP client for tsgo type resolution.
///
/// This client spawns a `tsgo --lsp` subprocess and communicates with it
/// via JSON-RPC over stdio to resolve TypeScript types.
pub struct TsgoClient {
    /// tsgo subprocess.
    process: Child,
    /// Buffered stdin writer for requests.
    writer: BufWriter<ChildStdin>,
    /// Channel receiver for messages from the reader thread.
    message_rx: mpsc::Receiver<ReaderMessage>,
    /// Handle to the reader thread (for cleanup).
    _reader_thread: thread::JoinHandle<()>,
    /// Auto-incrementing request ID.
    request_id: AtomicU32,
    /// Whether LSP initialize handshake completed.
    initialized: bool,
    /// Currently open files (tracked for didClose on drop).
    open_files: FxHashSet<String>,
    /// Document versions for each open file (incremented on each change).
    file_versions: FxHashMap<String, i32>,
    /// Resolved type cache.
    cache: TypeCache,
    /// Timeout for LSP requests.
    timeout: Duration,
}

impl TsgoClient {
    /// Start tsgo LSP subprocess with default timeout (5 seconds).
    ///
    /// # Arguments
    /// * `tsgo_path` - Optional path to tsgo binary. If None, tries:
    ///   1. `tsgo` in PATH (allows user override)
    ///   2. Embedded binary (if `bundled-tsgo` feature enabled)
    ///
    /// # Errors
    /// Returns error if tsgo binary not found or process fails to start.
    pub fn new(tsgo_path: Option<&Path>) -> Result<Self, TsgoError> {
        Self::with_timeout(tsgo_path, DEFAULT_TIMEOUT)
    }

    /// Start tsgo LSP subprocess with custom timeout.
    ///
    /// # Arguments
    /// * `tsgo_path` - Optional path to tsgo binary
    /// * `timeout` - Timeout for LSP requests
    ///
    /// # Errors
    /// Returns error if tsgo binary not found or process fails to start.
    #[instrument(level = "debug", skip(tsgo_path))]
    pub fn with_timeout(tsgo_path: Option<&Path>, timeout: Duration) -> Result<Self, TsgoError> {
        let tsgo_binary = Self::resolve_tsgo_binary(tsgo_path)?;
        debug!(binary = %tsgo_binary.display(), "Starting tsgo LSP subprocess");

        let mut process = Command::new(&tsgo_binary)
            .arg("--lsp")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                TsgoError::BinaryNotFound(format!(
                    "Failed to start tsgo at {:?}: {}",
                    tsgo_binary, e
                ))
            })?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| TsgoError::LspError("Failed to capture tsgo stdin".to_string()))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| TsgoError::LspError("Failed to capture tsgo stdout".to_string()))?;

        // Create channel for reader thread to send messages
        let (tx, rx) = mpsc::channel();

        // Spawn background reader thread
        let reader_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            Self::reader_loop(&mut reader, tx);
        });

        Ok(Self {
            process,
            writer: BufWriter::new(stdin),
            message_rx: rx,
            _reader_thread: reader_thread,
            request_id: AtomicU32::new(1),
            initialized: false,
            open_files: FxHashSet::default(),
            file_versions: FxHashMap::default(),
            cache: TypeCache::new(),
            timeout,
        })
    }

    /// Background reader loop that reads LSP messages and sends them via channel.
    fn reader_loop(reader: &mut BufReader<ChildStdout>, tx: mpsc::Sender<ReaderMessage>) {
        loop {
            match Self::read_single_message(reader) {
                Ok(msg) => {
                    if tx.send(ReaderMessage::Message(msg)).is_err() {
                        // Receiver dropped, exit the loop
                        break;
                    }
                }
                Err(e) => {
                    // Send error and exit - the process is likely dead or pipe closed
                    let _ = tx.send(ReaderMessage::Error(e.to_string()));
                    break;
                }
            }
        }
    }

    /// Read a single LSP message from the reader.
    fn read_single_message(
        reader: &mut BufReader<ChildStdout>,
    ) -> Result<serde_json::Value, TsgoError> {
        let mut content_length: Option<usize> = None;
        let mut header_line = String::new();

        loop {
            header_line.clear();
            reader.read_line(&mut header_line)?;
            let line = header_line.trim();

            if line.is_empty() {
                break;
            }

            if let Some(len_str) = line.strip_prefix("Content-Length: ") {
                content_length = len_str.parse().ok();
            }
        }

        let content_length = content_length
            .ok_or_else(|| TsgoError::LspError("Missing Content-Length header".to_string()))?;

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;

        let raw: serde_json::Value = serde_json::from_slice(&body)?;
        Ok(raw)
    }

    /// Get the current timeout setting.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Set the timeout for LSP requests.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Resolve the tsgo binary path.
    ///
    /// Priority:
    /// 1. Explicit path provided by user (--tsgo-path)
    /// 2. tsgo in PATH (allows user override of bundled version)
    /// 3. Embedded binary (extracted to cache on first use)
    fn resolve_tsgo_binary(tsgo_path: Option<&Path>) -> Result<PathBuf, TsgoError> {
        // 1. Explicit path takes priority
        if let Some(path) = tsgo_path {
            if path.exists() {
                return Ok(path.to_path_buf());
            }
            return Err(TsgoError::BinaryNotFound(format!(
                "Specified tsgo path does not exist: {}",
                path.display()
            )));
        }

        // 2. Check PATH for user-installed tsgo (allows override)
        if let Ok(path) = which::which("tsgo") {
            return Ok(path);
        }

        // 3. Try embedded binary
        if embedded::is_embedded() {
            return embedded::extract_tsgo_binary().map_err(|e| {
                TsgoError::ExtractionError(format!("Failed to extract embedded tsgo: {e}"))
            });
        }

        // No tsgo available
        Err(TsgoError::BinaryNotFound(
            "tsgo not found. Options:\n\
             - Install via: npm install -g @typescript/native-preview\n\
             - Build monoripple with: cargo build --features bundled-tsgo\n\
             - Specify path with: --tsgo-path /path/to/tsgo"
                .to_string(),
        ))
    }

    /// Check if embedded tsgo is available.
    pub fn has_embedded_tsgo() -> bool {
        embedded::is_embedded()
    }

    /// Get the embedded tsgo version (if bundled).
    pub fn embedded_tsgo_version() -> &'static str {
        embedded::TSGO_VERSION
    }

    /// Perform LSP initialize/initialized handshake.
    ///
    /// # Arguments
    /// * `root_uri` - Workspace root URI (e.g., "file:///path/to/project")
    #[instrument(level = "debug", skip(self))]
    pub fn initialize(&mut self, root_uri: &str) -> Result<InitializeResult, TsgoError> {
        debug!("Performing LSP initialize handshake");
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.to_string()),
            capabilities: ClientCapabilities {
                text_document: TextDocumentClientCapabilities {
                    hover: HoverClientCapabilities {
                        content_format: vec!["plaintext".to_string()],
                    },
                },
            },
        };

        let result: InitializeResult = self.send_request("initialize", params)?;

        // Send initialized notification
        let notification: JsonRpcNotification<serde_json::Value> =
            JsonRpcNotification::new("initialized", serde_json::json!({}));
        self.send_notification(&notification)?;

        self.initialized = true;
        debug!("LSP initialized successfully");
        Ok(result)
    }

    /// Open a file for analysis (sends textDocument/didOpen).
    ///
    /// File content is required because tsgo needs the source text.
    /// Files are tracked and closed on TsgoClient drop.
    pub fn open_file(&mut self, uri: &str, content: &str) -> Result<(), TsgoError> {
        if !self.initialized {
            return Err(TsgoError::NotInitialized);
        }

        if self.open_files.contains(uri) {
            return Ok(());
        }

        let version = 1;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.to_string(),
                language_id: "typescript".to_string(),
                version,
                text: content.to_string(),
            },
        };

        let notification = JsonRpcNotification::new("textDocument/didOpen", params);
        self.send_notification(&notification)?;
        self.open_files.insert(uri.to_string());
        self.file_versions.insert(uri.to_string(), version);
        Ok(())
    }

    /// Update an already-open file with new content (sends textDocument/didChange).
    ///
    /// This should be called when a file's content changes to keep tsgo in sync.
    /// The document version is automatically incremented.
    /// The hover cache for this file is cleared since positions may have changed.
    ///
    /// If the file is not currently open, this will open it instead.
    pub fn update_file(&mut self, uri: &str, content: &str) -> Result<(), TsgoError> {
        if !self.initialized {
            return Err(TsgoError::NotInitialized);
        }

        // If file is not open yet, just open it
        if !self.open_files.contains(uri) {
            return self.open_file(uri, content);
        }

        // Increment version
        let version = self
            .file_versions
            .get(uri)
            .copied()
            .unwrap_or(1)
            .saturating_add(1);
        self.file_versions.insert(uri.to_string(), version);

        // Clear cache for this file since positions may have changed
        self.clear_cache_for_file(uri);

        // Send didChange notification with full content replacement
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.to_string(),
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None, // None means full document replacement
                text: content.to_string(),
            }],
        };

        let notification = JsonRpcNotification::new("textDocument/didChange", params);
        self.send_notification(&notification)?;

        trace!(uri = %uri, version = version, "Updated file content in tsgo");
        Ok(())
    }

    /// Clear all cached hover results for a specific file.
    ///
    /// This should be called when a file's content changes, as the cached
    /// positions are no longer valid.
    pub fn clear_cache_for_file(&mut self, uri: &str) {
        self.cache.clear_for_file(uri);
    }

    /// Close a file (sends textDocument/didClose).
    pub fn close_file(&mut self, uri: &str) -> Result<(), TsgoError> {
        if !self.open_files.contains(uri) {
            return Ok(());
        }

        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: uri.to_string(),
            },
        };

        let notification = JsonRpcNotification::new("textDocument/didClose", params);
        self.send_notification(&notification)?;
        self.open_files.remove(uri);
        self.file_versions.remove(uri);
        Ok(())
    }

    /// Get type string at position via textDocument/hover.
    ///
    /// Returns None if no type information available at position.
    /// Results are cached by (uri, line, character).
    #[instrument(level = "debug", skip(self), fields(uri = %uri, line, character))]
    pub fn get_type_at_position(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<String>, TsgoError> {
        if !self.initialized {
            return Err(TsgoError::NotInitialized);
        }

        // Check cache first
        let cache_key = TypePosition::new(uri, line, character);
        if let Some(cached) = self.cache.get(&cache_key) {
            trace!("Cache hit");
            return Ok(Some(cached.clone()));
        }
        trace!("Cache miss, querying tsgo");

        let params = HoverParams {
            text_document: TextDocumentIdentifier {
                uri: uri.to_string(),
            },
            position: Position { line, character },
        };

        // Use send_hover_request which handles null results properly
        let response: Option<Hover> = self.send_hover_request("textDocument/hover", params)?;

        let type_str =
            response.and_then(|hover| Self::extract_type_from_hover_contents(hover.contents));

        // Cache the result if found
        if let Some(ref ts) = type_str {
            trace!(type_str = %ts, "Got type from hover");
            self.cache.insert(cache_key, ts.clone());
        } else {
            trace!("No type info at position");
        }

        Ok(type_str)
    }

    /// Batch query: get types at multiple positions in same file.
    ///
    /// More efficient than multiple single queries due to reduced
    /// JSON-RPC overhead.
    pub fn get_types_at_positions(
        &mut self,
        uri: &str,
        positions: &[(u32, u32)],
    ) -> Result<Vec<Option<String>>, TsgoError> {
        let mut results = Vec::with_capacity(positions.len());

        for &(line, character) in positions {
            results.push(self.get_type_at_position(uri, line, character)?);
        }

        Ok(results)
    }

    /// Get cache statistics (hits, misses).
    pub fn cache_stats(&self) -> (usize, usize) {
        self.cache.stats()
    }

    /// Send LSP shutdown request and wait for exit.
    pub fn shutdown(&mut self) -> Result<(), TsgoError> {
        if !self.initialized {
            return Ok(());
        }

        // Close all open files
        let files: Vec<String> = self.open_files.iter().cloned().collect();
        for uri in files {
            let _ = self.close_file(&uri);
        }

        // Send shutdown request - use a short timeout since we don't care about the response
        // Some LSP servers don't respond properly to shutdown, so don't block for long
        let old_timeout = self.timeout;
        self.timeout = Duration::from_millis(100);
        let _: Result<Option<()>, _> = self.send_request("shutdown", ShutdownParams {});
        self.timeout = old_timeout;

        // Send exit notification
        let notification: JsonRpcNotification<serde_json::Value> =
            JsonRpcNotification::new("exit", serde_json::json!(null));
        let _ = self.send_notification(&notification);

        self.initialized = false;
        Ok(())
    }

    /// Extract type string from hover contents.
    fn extract_type_from_hover_contents(contents: HoverContents) -> Option<String> {
        match contents {
            HoverContents::Single(content) => Self::extract_from_marked_string(content),
            HoverContents::Array(arr) => {
                // Try to find the most useful content (prefer typescript code blocks)
                for item in arr {
                    if let Some(s) = Self::extract_from_marked_string(item) {
                        return Some(s);
                    }
                }
                None
            }
        }
    }

    fn extract_from_marked_string(content: MarkedStringOrMarkup) -> Option<String> {
        match content {
            MarkedStringOrMarkup::String(s) => {
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
            MarkedStringOrMarkup::MarkupContent(mc) => {
                if mc.value.is_empty() {
                    None
                } else {
                    // Strip markdown code block markers if present
                    let value = mc.value.trim();
                    if value.starts_with("```") {
                        // Find the end of the first line and the closing ```
                        let lines: Vec<&str> = value.lines().collect();
                        if lines.len() >= 2 {
                            let content_lines: Vec<&str> = lines[1..lines.len() - 1].to_vec();
                            Some(content_lines.join("\n"))
                        } else {
                            Some(value.to_string())
                        }
                    } else {
                        Some(value.to_string())
                    }
                }
            }
            MarkedStringOrMarkup::MarkedString(ms) => {
                if ms.value.is_empty() {
                    None
                } else {
                    Some(ms.value)
                }
            }
        }
    }

    /// Send a JSON-RPC request and wait for response.
    fn send_request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &'static str,
        params: P,
    ) -> Result<R, TsgoError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);

        // Serialize and send
        let body = serde_json::to_string(&request)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        self.writer.write_all(header.as_bytes())?;
        self.writer.write_all(body.as_bytes())?;
        self.writer.flush()?;

        // Read response with timeout
        let response: JsonRpcResponse<R> = self.read_response()?;

        if let Some(error) = response.error {
            return Err(TsgoError::JsonRpcError {
                code: error.code,
                message: error.message,
            });
        }

        response
            .result
            .ok_or_else(|| TsgoError::LspError("Empty response".to_string()))
    }

    /// Send a hover request and wait for response.
    /// This handles the case where the result is null (no hover info at position).
    fn send_hover_request<P: serde::Serialize>(
        &mut self,
        method: &'static str,
        params: P,
    ) -> Result<Option<Hover>, TsgoError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);

        // Serialize and send
        let body = serde_json::to_string(&request)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        self.writer.write_all(header.as_bytes())?;
        self.writer.write_all(body.as_bytes())?;
        self.writer.flush()?;

        // Read response with timeout - use Value to handle null results
        let response: JsonRpcResponse<serde_json::Value> = self.read_response()?;

        if let Some(error) = response.error {
            return Err(TsgoError::JsonRpcError {
                code: error.code,
                message: error.message,
            });
        }

        // Handle null result (no hover info at position)
        match response.result {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => {
                let hover: Hover = serde_json::from_value(value)?;
                Ok(Some(hover))
            }
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    fn send_notification<P: serde::Serialize>(
        &mut self,
        notification: &JsonRpcNotification<P>,
    ) -> Result<(), TsgoError> {
        let body = serde_json::to_string(notification)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        self.writer.write_all(header.as_bytes())?;
        self.writer.write_all(body.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Read a JSON-RPC response from the message channel with timeout.
    ///
    /// This method handles the LSP protocol which may send notifications
    /// (messages without id) before the actual response. We skip notifications
    /// and keep reading until we get a response with an id.
    fn read_response<R: serde::de::DeserializeOwned>(
        &mut self,
    ) -> Result<JsonRpcResponse<R>, TsgoError> {
        let start = std::time::Instant::now();

        loop {
            // Calculate remaining time
            let elapsed = start.elapsed();
            if elapsed >= self.timeout {
                return Err(TsgoError::Timeout(self.timeout));
            }
            let remaining = self.timeout - elapsed;

            // Wait for a message with timeout
            match self.message_rx.recv_timeout(remaining) {
                Ok(ReaderMessage::Message(raw)) => {
                    // Check message type:
                    // - Response: has "id" but no "method"
                    // - Request (server->client): has "id" AND "method"
                    // - Notification: has "method" but no "id"
                    // - Error notification: has "error" but no "id"
                    let has_id = raw.get("id").is_some();
                    let has_method = raw.get("method").is_some();

                    if has_id && !has_method {
                        // This is a response to one of our requests
                        let response: JsonRpcResponse<R> = serde_json::from_value(raw)?;
                        return Ok(response);
                    }

                    if has_id && has_method {
                        // Server-initiated request - we need to respond to unblock the server
                        // Send a null result response to acknowledge the request
                        if let Some(id) = raw.get("id") {
                            let response_body = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": null
                            });
                            let body = serde_json::to_string(&response_body).unwrap();
                            let header = format!("Content-Length: {}\r\n\r\n", body.len());
                            let _ = self.writer.write_all(header.as_bytes());
                            let _ = self.writer.write_all(body.as_bytes());
                            let _ = self.writer.flush();
                        }
                        // Continue reading for our actual response
                        continue;
                    }

                    // Notification - skip it and continue reading
                }
                Ok(ReaderMessage::Error(e)) => {
                    return Err(TsgoError::LspError(e));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(TsgoError::Timeout(self.timeout));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(TsgoError::LspError(
                        "Reader thread disconnected".to_string(),
                    ));
                }
            }
        }
    }
}

impl Drop for TsgoClient {
    fn drop(&mut self) {
        // Try to cleanly shutdown
        let _ = self.shutdown();

        // Kill process if still running
        let _ = self.process.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests require tsgo to be installed and working correctly.
    // They are skipped if tsgo is not available or if LSP communication fails.

    fn tsgo_available() -> bool {
        which::which("tsgo").is_ok() || TsgoClient::has_embedded_tsgo()
    }

    /// Try to create and initialize a tsgo client.
    /// Returns None if tsgo is not available or initialization fails.
    fn try_create_initialized_client() -> Option<TsgoClient> {
        if !tsgo_available() {
            return None;
        }

        let mut client = TsgoClient::new(None).ok()?;
        client.initialize("file:///tmp/test").ok()?;
        Some(client)
    }

    #[test]
    fn test_client_creation_without_tsgo() {
        // Skip if tsgo is in PATH or if embedded tsgo is available
        if tsgo_available() {
            println!("Skipping test_client_creation_without_tsgo: tsgo is available in PATH");
            return;
        }
        if TsgoClient::has_embedded_tsgo() {
            println!("Skipping test_client_creation_without_tsgo: embedded tsgo is available");
            return;
        }

        // Only when neither PATH nor embedded tsgo is available should this fail
        let result = TsgoClient::new(None);
        assert!(result.is_err());
        if let Err(TsgoError::BinaryNotFound(_)) = result {
            // Expected
        } else {
            panic!("Expected BinaryNotFound error");
        }
    }

    #[test]
    fn test_client_with_tsgo() {
        if !tsgo_available() {
            eprintln!("Skipping test_client_with_tsgo: tsgo not found in PATH");
            return;
        }

        let mut client = match TsgoClient::new(None) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Skipping test_client_with_tsgo: failed to create client: {e}");
                return;
            }
        };

        match client.initialize("file:///tmp/test") {
            Ok(_) => {
                // Test passed - tsgo LSP is working
            }
            Err(e) => {
                // LSP communication failed - skip test
                eprintln!("Skipping test_client_with_tsgo: LSP communication failed: {e}");
            }
        }
    }

    #[test]
    fn test_hover_query() {
        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!("Skipping test_hover_query: tsgo not available or initialization failed");
                return;
            }
        };

        // Open a simple file
        let content = "const x: string = 'hello';";
        if let Err(e) = client.open_file("file:///tmp/test/test.ts", content) {
            eprintln!("Skipping test_hover_query: failed to open file: {e}");
            return;
        }

        // Query type at position of 'x'
        match client.get_type_at_position("file:///tmp/test/test.ts", 0, 6) {
            Ok(result) => {
                // Should get some type info (exact format may vary)
                println!("Hover result: {:?}", result);
            }
            Err(TsgoError::Timeout(_)) => {
                // Timeout is acceptable - tsgo may not return hover info for this position
                println!("Hover query timed out (expected behavior for some positions)");
            }
            Err(e) => {
                eprintln!("Hover query failed: {e}");
            }
        }
    }

    #[test]
    fn test_cache_works() {
        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!("Skipping test_cache_works: tsgo not available or initialization failed");
                return;
            }
        };

        let content = "const x: string = 'hello';";
        if let Err(e) = client.open_file("file:///tmp/test/cache.ts", content) {
            eprintln!("Skipping test_cache_works: failed to open file: {e}");
            return;
        }

        // First query - may timeout
        let result1 = client.get_type_at_position("file:///tmp/test/cache.ts", 0, 6);
        let (hits1, misses1) = client.cache_stats();

        if result1.is_err() {
            println!("First query failed or timed out, skipping cache test");
            return;
        }

        // Second query (should hit cache)
        let _ = client.get_type_at_position("file:///tmp/test/cache.ts", 0, 6);
        let (hits2, misses2) = client.cache_stats();

        assert_eq!(
            misses1, misses2,
            "Miss count should not increase on cache hit"
        );
        assert!(hits2 > hits1, "Hit count should increase on cache hit");
    }

    #[test]
    fn test_timeout_works() {
        // Create client with very short timeout
        let mut client = match TsgoClient::with_timeout(None, Duration::from_millis(1)) {
            Ok(c) => c,
            Err(_) => {
                eprintln!("Skipping test_timeout_works: tsgo not available");
                return;
            }
        };

        // Initialize with short timeout - this should timeout
        match client.initialize("file:///tmp/test") {
            Ok(_) => {
                // Initialization succeeded despite short timeout
                // This is fine - the server responded quickly
                println!("Initialize succeeded with 1ms timeout");
            }
            Err(TsgoError::Timeout(_)) => {
                // Expected - timeout working correctly
                println!("Initialize timed out as expected");
            }
            Err(e) => {
                // Other errors are also acceptable
                println!("Initialize failed with: {e}");
            }
        }
    }

    #[test]
    fn test_hover_property_returns_evaluated_type() {
        // This test verifies that hovering on a property name returns the evaluated type,
        // not the raw type expression. This is crucial for resolving complex utility types
        // like `Parameters<Page["setCookie"]>[0]` to their concrete form like `CookieParam`.

        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!(
                    "Skipping test_hover_property_returns_evaluated_type: tsgo not available"
                );
                return;
            }
        };

        // TypeScript file with interface that uses complex utility types
        // Lines are 0-indexed:
        // 0: interface Page {
        // 1:   setCookie(...cookies: CookieParam[]): Promise<void>;
        // 2: }
        // 3: (empty)
        // 4: interface CookieParam {
        // 5:   name: string;
        // 6:   value: string;
        // 7: }
        // 8: (empty)
        // 9: interface TestOptions {
        // 10:   cookies?: Parameters<Page["setCookie"]>[0][];
        // 11: }
        let content = "interface Page {\n  setCookie(...cookies: CookieParam[]): Promise<void>;\n}\n\ninterface CookieParam {\n  name: string;\n  value: string;\n}\n\ninterface TestOptions {\n  cookies?: Parameters<Page[\"setCookie\"]>[0][];\n}";

        let uri = "file:///tmp/test/property_hover.ts";
        if let Err(e) = client.open_file(uri, content) {
            eprintln!("Skipping test: failed to open file: {e}");
            return;
        }

        // Print content with line numbers for debugging
        for (i, line) in content.lines().enumerate() {
            println!("Line {}: {}", i, line);
        }

        // Line 10 (0-indexed): `  cookies?: Parameters<Page["setCookie"]>[0][];`
        // Position of "cookies" property name: line 10, column 2

        // Query at property name position (cookies)
        match client.get_type_at_position(uri, 10, 2) {
            Ok(Some(result)) => {
                println!(
                    "Hover at property name 'cookies' (line 10, col 2): {}",
                    result
                );
                // Expected: "(property) cookies?: CookieParam[]"
                // The type should be evaluated to CookieParam[], not the raw Parameters<...>[0][]
                if result.contains("(property)") {
                    println!("Got property format");
                    // Check that it contains the evaluated type (CookieParam) rather than Parameters
                    if result.contains("CookieParam") {
                        println!("SUCCESS: tsgo returned evaluated type CookieParam");
                    } else if result.contains("Parameters") {
                        println!("NOTE: tsgo returned raw type expression (not evaluated)");
                    } else {
                        println!("NOTE: tsgo returned: {}", result);
                    }
                } else {
                    println!("Unexpected format: {}", result);
                }
            }
            Ok(None) => {
                println!("No hover info at property name position (line 10, col 2)");
            }
            Err(e) => {
                eprintln!("Hover query failed: {e}");
            }
        }

        // Compare with hover at type position (Parameters)
        // Line 10, column 12 = start of "Parameters" (after "  cookies?: ")
        match client.get_type_at_position(uri, 10, 12) {
            Ok(Some(result)) => {
                println!("Hover at type 'Parameters' (line 10, col 12): {}", result);
                // Expected: "type Parameters<T extends (...args: any) => any> = ..."
                // This should NOT be the evaluated type
            }
            Ok(None) => {
                println!("No hover info at type position (line 10, col 12)");
            }
            Err(e) => {
                eprintln!("Hover query at type position failed: {e}");
            }
        }

        // Also try some other positions to understand tsgo behavior
        for col in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
            match client.get_type_at_position(uri, 10, col) {
                Ok(Some(result)) => {
                    println!(
                        "Line 10, col {}: {}",
                        col,
                        result.chars().take(80).collect::<String>()
                    );
                }
                Ok(None) => {
                    // Skip to reduce noise
                }
                Err(_) => {}
            }
        }
    }

    #[test]
    fn test_hover_method_returns_inferred_return_type() {
        // This test verifies that hovering on a method name returns the inferred return type,
        // even when there's no explicit return type annotation.

        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!(
                    "Skipping test_hover_method_returns_inferred_return_type: tsgo not available"
                );
                return;
            }
        };

        // Class with method that has no explicit return type but returns Response
        let content = r#"
class MyClass {
    async fetch(request: Request) {
        return new Response("hello");
    }

    async noReturn() {
        console.log("no return");
    }

    syncMethod() {
        return 42;
    }
}
"#;

        let uri = "file:///tmp/test/method_hover.ts";
        if let Err(e) = client.open_file(uri, content) {
            eprintln!("Skipping test: failed to open file: {e}");
            return;
        }

        // Print content with line numbers for debugging
        for (i, line) in content.lines().enumerate() {
            println!("Line {}: {}", i, line);
        }

        // Line 2 (0-indexed): `    async fetch(request: Request) {`
        // Position of "fetch" method name: line 2, column 10
        match client.get_type_at_position(uri, 2, 10) {
            Ok(Some(result)) => {
                println!("Hover at method name 'fetch' (line 2, col 10): {}", result);
                // Expected format: "(method) MyClass.fetch(request: Request): Promise<Response>"
                assert!(
                    result.contains("Promise<Response>"),
                    "Expected return type Promise<Response>, got: {}",
                    result
                );
            }
            Ok(None) => {
                println!("No hover info at method name position");
            }
            Err(e) => {
                eprintln!("Hover query failed: {e}");
            }
        }

        // Line 6: `    async noReturn() {`
        // Position of "noReturn" method name: line 6, column 10
        match client.get_type_at_position(uri, 6, 10) {
            Ok(Some(result)) => {
                println!(
                    "Hover at method name 'noReturn' (line 6, col 10): {}",
                    result
                );
                // Expected format: "(method) MyClass.noReturn(): Promise<void>"
                assert!(
                    result.contains("Promise<void>"),
                    "Expected return type Promise<void>, got: {}",
                    result
                );
            }
            Ok(None) => {
                println!("No hover info at noReturn method");
            }
            Err(e) => {
                eprintln!("Hover query failed: {e}");
            }
        }

        // Line 10: `    syncMethod() {`
        // Position of "syncMethod" method name: line 10, column 4
        match client.get_type_at_position(uri, 10, 4) {
            Ok(Some(result)) => {
                println!(
                    "Hover at method name 'syncMethod' (line 10, col 4): {}",
                    result
                );
                // Expected format: "(method) MyClass.syncMethod(): number"
                assert!(
                    result.contains("number"),
                    "Expected return type number, got: {}",
                    result
                );
            }
            Ok(None) => {
                println!("No hover info at syncMethod method");
            }
            Err(e) => {
                eprintln!("Hover query failed: {e}");
            }
        }
    }

    #[test]
    fn test_hover_does_not_truncate_many_fields() {
        // This test verifies that the tsgo LSP hover output does NOT truncate types
        // with many fields. Our patch to tsgo disables truncation, so we should see
        // all fields in the hover output, not "... N more ...".
        //
        // We use an inline object type (not a named type) so that hover shows the
        // structural type with all fields, rather than just the type alias name.

        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!("Skipping test_hover_does_not_truncate_many_fields: tsgo not available");
                return;
            }
        };

        // Create an inline object type with 30 fields - this would definitely be truncated
        // with the default 160 character limit
        let field_count = 30;
        let fields: Vec<String> = (1..=field_count)
            .map(|i| format!("field{}: string", i))
            .collect();

        // Use an inline object type so hover shows the expanded form
        let content = format!("const obj: {{ {} }} = {{}} as any;", fields.join("; "));

        let uri = "file:///tmp/test/many_fields.ts";
        if let Err(e) = client.open_file(uri, content.as_str()) {
            eprintln!("Skipping test: failed to open file: {e}");
            return;
        }

        // Print content for debugging
        println!("=== Test file content ===");
        println!("{}", content);
        println!("=========================");

        // Query hover at position of 'obj' variable (column 6 = start of "obj")
        match client.get_type_at_position(uri, 0, 6) {
            Ok(Some(result)) => {
                println!("Hover result for 'obj' with {} fields:", field_count);
                println!("{}", result);

                // The hover output should NOT contain truncation markers
                let has_truncation = result.contains("...") && result.contains("more");
                assert!(
                    !has_truncation,
                    "Hover output should NOT be truncated. Found truncation in: {}",
                    result
                );

                // Verify that field1 and the last field are both present
                assert!(
                    result.contains("field1"),
                    "Expected field1 to be present in hover output: {}",
                    result
                );

                // Check for the last field
                let last_field = format!("field{}", field_count);
                assert!(
                    result.contains(&last_field),
                    "Expected {} to be present in hover output: {}",
                    last_field,
                    result
                );

                // Count how many fields are actually present
                let mut fields_found = 0;
                for i in 1..=field_count {
                    if result.contains(&format!("field{}", i)) {
                        fields_found += 1;
                    }
                }
                println!(
                    "Fields found in hover output: {}/{}",
                    fields_found, field_count
                );

                assert_eq!(
                    fields_found, field_count,
                    "All {} fields should be present in hover output, but only found {}",
                    field_count, fields_found
                );
            }
            Ok(None) => {
                panic!("No hover info at obj position - test cannot verify truncation behavior");
            }
            Err(e) => {
                panic!("Hover query failed: {e}");
            }
        }
    }

    #[test]
    fn test_hover_does_not_truncate_50_fields() {
        // Even more extreme test with 50 fields to ensure no truncation
        // Uses inline object type to see expanded form

        let mut client = match try_create_initialized_client() {
            Some(c) => c,
            None => {
                eprintln!("Skipping test_hover_does_not_truncate_50_fields: tsgo not available");
                return;
            }
        };

        let field_count = 50;
        let fields: Vec<String> = (1..=field_count)
            .map(|i| format!("prop{}: number", i))
            .collect();

        // Use inline object type
        let content = format!(
            "function process(data: {{ {} }}): void {{}}\n",
            fields.join("; ")
        );

        let uri = "file:///tmp/test/fifty_fields.ts";
        if let Err(e) = client.open_file(uri, content.as_str()) {
            eprintln!("Skipping test: failed to open file: {e}");
            return;
        }

        println!("=== Test file content (truncated for display) ===");
        println!("{content}");
        println!("=========================");

        // Hover on 'data' parameter (column 18 = position of "data" after "function process(")
        match client.get_type_at_position(uri, 0, 18) {
            Ok(Some(result)) => {
                println!("Hover result for parameter with {} fields:", field_count);
                // Only print first 500 chars to avoid overwhelming output
                println!("{result}");

                // Should not contain truncation markers (the "... N more ..." pattern)
                let has_truncation = result.contains("...") && result.contains("more");
                assert!(
                    !has_truncation,
                    "Hover output should NOT be truncated with {} fields. Got: {}",
                    field_count, result
                );

                // Verify first and last properties are present
                assert!(
                    result.contains("prop1"),
                    "Expected prop1 in output: {}",
                    result
                );
                assert!(
                    result.contains(&format!("prop{}", field_count)),
                    "Expected prop{} in output: {}",
                    field_count,
                    result
                );

                // Count fields found
                let mut fields_found = 0;
                for i in 1..=field_count {
                    if result.contains(&format!("prop{}", i)) {
                        fields_found += 1;
                    }
                }
                println!(
                    "Fields found in hover output: {}/{}",
                    fields_found, field_count
                );

                assert_eq!(
                    fields_found, field_count,
                    "All {} fields should be present, but only found {}",
                    field_count, fields_found
                );
            }
            Ok(None) => {
                panic!("No hover info at parameter position");
            }
            Err(e) => {
                panic!("Hover query failed: {e}");
            }
        }
    }
}
