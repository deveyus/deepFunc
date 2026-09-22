//! Minimal blocking LSP client over stdio, language-agnostic.
//!
//! Speaks just enough JSON-RPC for one flow: `initialize`,
//! `workspace/symbol`, `textDocument/documentSymbol`,
//! `textDocument/prepareCallHierarchy`, `callHierarchy/incomingCalls`.
//! The server program plus args comes from the caller (per-language table
//! lives in the CLI). Server-to-client requests get a best-effort `null`
//! reply so the server never stalls waiting on us.

#![forbid(unsafe_code)]

use deepfunc_core::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: u32,
    pub location: Location,
    #[serde(rename = "containerName")]
    pub container_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HierarchyItem {
    pub name: String,
    pub kind: u32,
    pub uri: String,
    pub range: Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
}

#[derive(Debug, Deserialize)]
pub struct IncomingCall {
    pub from: HierarchyItem,
    #[serde(rename = "fromRanges")]
    pub from_ranges: Vec<Range>,
}

/// One node of a `textDocument/documentSymbol` tree. Only position data
/// is kept: names come from hierarchy items. (Unknown JSON fields such as
/// `name` are ignored on decode.)
#[derive(Debug, Deserialize, Clone)]
pub struct DocumentSymbol {
    pub kind: u32,
    pub range: Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: Range,
    #[serde(default)]
    pub children: Vec<DocumentSymbol>,
}

#[derive(Debug, Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    id: i64,
    method: String,
    params: Value,
}

enum ClientEvent {
    Response { id: i64, result: Value },
}

/// Symbol kinds that can have callers (Method, Constructor, Function).
pub fn is_callable(kind: u32) -> bool {
    matches!(kind, 6 | 9 | 12)
}

/// Innermost callable symbol containing 0-based `line0`, or `None`.
pub fn innermost_callable(symbols: &[DocumentSymbol], line0: u32) -> Option<&DocumentSymbol> {
    for symbol in symbols {
        if symbol.range.start.line <= line0 && line0 <= symbol.range.end.line {
            if let Some(inner) = innermost_callable(&symbol.children, line0) {
                return Some(inner);
            }
            if is_callable(symbol.kind) {
                return Some(symbol);
            }
        }
    }
    None
}

fn write_message(stdin: &Arc<Mutex<ChildStdin>>, body: &str, server: &str) -> Result<(), Error> {
    let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    match stdin.lock() {
        Ok(mut guard) => match guard.write_all(framed.as_bytes()) {
            Ok(()) => match guard.flush() {
                Ok(()) => Ok(()),
                Err(error) => Err(guard_dropped_error(server, error.to_string())),
            },
            Err(error) => Err(guard_dropped_error(server, error.to_string())),
        },
        Err(error) => Err(guard_dropped_error(server, error.to_string())),
    }
}

fn guard_dropped_error(server: &str, detail: String) -> Error {
    Error::RequestFailed {
        server: server.to_owned(),
        request: "stdio-write".to_owned(),
        timeout_secs: 0,
        detail,
    }
}

fn read_message(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    match content_length {
        Some(length) => {
            let mut buffer = vec![0u8; length];
            reader.read_exact(&mut buffer)?;
            Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
        }
        None => Ok(Some(String::new())),
    }
}

/// Indexing state from `rust-analyzer/serverStatus` notifications.
/// `quiescent: true` means the workspace index is complete and empty
/// hierarchy results are trustworthy (not a cold-index race).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerStatus {
    seen: bool,
    quiescent: bool,
}

pub struct LanguageClient {
    stdin: Arc<Mutex<ChildStdin>>,
    events: mpsc::Receiver<ClientEvent>,
    status: Arc<Mutex<ServerStatus>>,
    notes: Arc<Mutex<Vec<String>>>,
    server_label: String,
    next_id: i64,
    child: Option<Child>,
    timeout_secs: u64,
}

impl LanguageClient {
    /// Spawn `program` with `args` and run `initialize` against `root`.
    /// `server_label` names the server in error output (e.g. `gopls`).
    pub fn spawn(
        program: &str,
        args: &[String],
        root: &Path,
        timeout_secs: u64,
        language: &str,
        install_hint: &str,
    ) -> Result<Self, Error> {
        let root_uri = format!("file://{}", root.display());
        let command_line = if args.is_empty() {
            program.to_owned()
        } else {
            program.to_owned() + " " + &args.join(" ")
        };
        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return Err(Error::ServerNotFound {
                    language: language.to_owned(),
                    server: command_line,
                    detail: error.to_string(),
                    install_hint: install_hint.to_owned(),
                })
            }
        };
        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take();
        let stdin: Arc<Mutex<ChildStdin>> = match child_stdin {
            Some(stdin) => Arc::new(Mutex::new(stdin)),
            None => {
                return Err(Error::RequestFailed {
                    server: command_line.clone(),
                    request: "spawn".to_owned(),
                    timeout_secs,
                    detail: "server started without a stdin pipe".to_owned(),
                })
            }
        };
        let (events_tx, events_rx) = mpsc::channel::<ClientEvent>();
        let status = Arc::new(Mutex::new(ServerStatus {
            seen: false,
            quiescent: false,
        }));
        let notes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let label = command_line.clone();
        if let Some(stdout) = child_stdout {
            let reply_stdin = Arc::clone(&stdin);
            let status_tx = Arc::clone(&status);
            let notes_tx = Arc::clone(&notes);
            let reply_label = label.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(text)) = read_message(&mut reader) {
                    if text.is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    // Server notification (no id): watch indexing state and
                    // keep a short diagnostic log for failure output.
                    if value.get("id").is_none() {
                        if let Some(method) = value.get("method").and_then(Value::as_str) {
                            if method == "rust-analyzer/serverStatus" {
                                let quiescent = value
                                    .get("params")
                                    .and_then(|params| params.get("quiescent"))
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                if let Ok(mut guard) = status_tx.lock() {
                                    guard.seen = true;
                                    guard.quiescent = quiescent;
                                }
                            }
                            if let Ok(mut guard) = notes_tx.lock() {
                                if guard.len() < 30 {
                                    let mut summary =
                                        value.get("params").map_or("null".to_owned(), |params| {
                                            serde_json::to_string(params).unwrap_or_default()
                                        });
                                    if summary.len() > 300 {
                                        summary.truncate(300);
                                    }
                                    guard.push(method.to_owned() + " :: " + &summary);
                                }
                            }
                        }
                        continue;
                    }
                    // Server-to-client request: best-effort null reply.
                    if value.get("method").is_some() {
                        if let Some(id) = value.get("id") {
                            let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
                            if let Ok(body) = serde_json::to_string(&reply) {
                                let _ignored = write_message(&reply_stdin, &body, &reply_label);
                            }
                        }
                        continue;
                    }
                    if let Some(id) = value.get("id").and_then(Value::as_i64) {
                        let result = value.get("result").cloned().unwrap_or(Value::Null);
                        if value.get("error").is_some() {
                            let _ignored =
                                events_tx.send(ClientEvent::Response { id, result: value });
                        } else if events_tx
                            .send(ClientEvent::Response { id, result })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            });
        }
        let mut client = Self {
            stdin,
            events: events_rx,
            status,
            notes,
            server_label: label,
            next_id: 1,
            child: Some(child),
            timeout_secs,
        };
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
        });
        client.request("initialize", params)?;
        client.notify("initialized", json!({}));
        Ok(client)
    }

    /// True once the server reports a complete index. Empty hierarchy
    /// results are only trustworthy when this holds.
    pub fn is_quiescent(&self) -> bool {
        self.status
            .lock()
            .map(|guard| guard.quiescent)
            .unwrap_or(false)
    }

    /// Collected server notifications (most recent last), for failure output.
    pub fn drain_notes(&self) -> Vec<String> {
        match self.notes.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        }
    }

    fn fail(&self, request: &str, detail: String) -> Error {
        Error::RequestFailed {
            server: self.server_label.clone(),
            request: request.to_owned(),
            timeout_secs: self.timeout_secs,
            detail,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        let message = RpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_owned(),
            params,
        };
        let body = match serde_json::to_string(&message) {
            Ok(body) => body,
            Err(error) => {
                return Err(self.fail(
                    method,
                    "failed to encode request: ".to_owned() + &error.to_string(),
                ));
            }
        };
        write_message(&self.stdin, &body, &self.server_label)?;
        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.fail(method, "timed out waiting for a response".to_owned()));
            }
            match self.events.recv_timeout(remaining) {
                Ok(ClientEvent::Response { id: got, result }) => {
                    if got == id {
                        if result.get("error").is_some() {
                            return Err(self.fail(method, result.to_string()));
                        }
                        return Ok(result);
                    }
                }
                Err(_) => {
                    return Err(self.fail(method, "timed out waiting for a response".to_owned()));
                }
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Ok(text) = serde_json::to_string(&body) {
            let _ignored = write_message(&self.stdin, &text, &self.server_label);
        }
    }

    /// Open a file so later hierarchy requests resolve against fresh text.
    pub fn did_open(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text,
            }}),
        );
    }

    /// Single `workspace/symbol` round trip, filtered to callable kinds.
    /// Treats `null` as empty (servers answer `null` while still indexing).
    fn request_symbols(&mut self, query: &str) -> Result<Vec<SymbolInfo>, Error> {
        let result = self.request("workspace/symbol", json!({"query": query}))?;
        let symbols: Vec<SymbolInfo> =
            match serde_json::from_value::<Option<Vec<SymbolInfo>>>(result) {
                Ok(None) => Vec::new(),
                Ok(Some(symbols)) => symbols,
                Err(error) => {
                    return Err(self.fail(
                        "workspace/symbol",
                        "failed to decode symbols: ".to_owned() + &error.to_string(),
                    ));
                }
            };
        Ok(symbols
            .into_iter()
            .filter(|symbol| is_callable(symbol.kind))
            .collect())
    }

    /// Wait until the symbol index is loaded, using a canary query that
    /// matches in virtually every workspace. Polls until the canary answer
    /// is non-empty AND unchanged across two polls, or 30s budget.
    /// Errors count as not-ready (some servers fail queries while their
    /// project model loads). Returns true when the index looks ready. Call
    /// once after spawn; after this, an empty target lookup means
    /// genuinely unknown.
    pub fn ensure_index_ready(&mut self) -> bool {
        let start = Instant::now();
        let budget = Duration::from_secs(30);
        let mut last_count: Option<usize> = None;
        loop {
            match self.request_symbols("a") {
                Ok(functions) => {
                    if self.is_quiescent() {
                        return true;
                    }
                    if !functions.is_empty() && last_count == Some(functions.len()) {
                        return true;
                    }
                    last_count = Some(functions.len());
                }
                Err(_) => {
                    last_count = None;
                }
            }
            if start.elapsed() >= budget {
                return self.is_quiescent();
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Find callable symbols matching `query`. Assumes
    /// [`ensure_index_ready`](Self::ensure_index_ready) ran first: one
    /// request, then short retries (empty AND errored — a freshly loading
    /// project fails queries before it serves them), then trust the answer.
    pub fn workspace_symbols(&mut self, query: &str) -> Result<Vec<SymbolInfo>, Error> {
        let start = Instant::now();
        let budget = Duration::from_secs(20);
        loop {
            match self.request_symbols(query) {
                Ok(functions) => {
                    if !functions.is_empty() || start.elapsed() >= budget {
                        return Ok(functions);
                    }
                }
                Err(error) => {
                    if start.elapsed() >= budget {
                        return Err(error);
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Hierarchical symbols of one open document. Servers answer
    /// `DocumentSymbol[]` or (older) flat `SymbolInformation[]`; both are
    /// normalized to a tree.
    pub fn document_symbols(&mut self, uri: &str) -> Result<Vec<DocumentSymbol>, Error> {
        let result = self.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": uri}}),
        )?;
        if let Ok(symbols) = serde_json::from_value::<Option<Vec<DocumentSymbol>>>(result.clone()) {
            return Ok(symbols.unwrap_or_default());
        }
        match serde_json::from_value::<Option<Vec<SymbolInfo>>>(result) {
            Ok(None) => Ok(Vec::new()),
            Ok(Some(infos)) => Ok(infos
                .into_iter()
                .filter(|info| is_callable(info.kind))
                .map(|info| DocumentSymbol {
                    kind: info.kind,
                    range: info.location.range.clone(),
                    selection_range: info.location.range,
                    children: Vec::new(),
                })
                .collect()),
            Err(error) => Err(self.fail(
                "textDocument/documentSymbol",
                "failed to decode symbols: ".to_owned() + &error.to_string(),
            )),
        }
    }

    /// Resolve a document position to hierarchy items. The readiness gate
    /// runs first, so empty is trusted after a short backstop retry for
    /// reanalysis races (e.g. a just-did_open'd file).
    pub fn prepare_hierarchy(
        &mut self,
        uri: &str,
        line0: u32,
        character: u32,
    ) -> Result<Vec<HierarchyItem>, Error> {
        let start = Instant::now();
        let budget = Duration::from_secs(5);
        loop {
            let result = self.request(
                "textDocument/prepareCallHierarchy",
                json!({"textDocument": {"uri": uri},
                       "position": {"line": line0, "character": character}}),
            )?;
            let items: Vec<HierarchyItem> =
                match serde_json::from_value::<Option<Vec<HierarchyItem>>>(result) {
                    // Servers answer `null` (not `[]`) when the position
                    // resolves to no symbol. Empty, not a protocol error.
                    Ok(None) => Vec::new(),
                    Ok(Some(items)) => items,
                    Err(error) => {
                        return Err(self.fail(
                            "textDocument/prepareCallHierarchy",
                            "failed to decode hierarchy: ".to_owned() + &error.to_string(),
                        ));
                    }
                };
            if !items.is_empty() || self.is_quiescent() || start.elapsed() >= budget {
                return Ok(items);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Direct callers of one hierarchy item. Same backstop policy as
    /// [`prepare_hierarchy`](Self::prepare_hierarchy): trust empty fast,
    /// since readiness was gated upfront.
    pub fn incoming_calls(&mut self, item: &HierarchyItem) -> Result<Vec<IncomingCall>, Error> {
        let item_value = match serde_json::to_value(item) {
            Ok(value) => value,
            Err(error) => {
                return Err(self.fail(
                    "callHierarchy/incomingCalls",
                    "failed to encode item: ".to_owned() + &error.to_string(),
                ));
            }
        };
        let start = Instant::now();
        let budget = Duration::from_secs(5);
        loop {
            let result =
                self.request("callHierarchy/incomingCalls", json!({"item": item_value}))?;
            let calls: Vec<IncomingCall> =
                match serde_json::from_value::<Option<Vec<IncomingCall>>>(result) {
                    Ok(None) => Vec::new(),
                    Ok(Some(calls)) => calls,
                    Err(error) => {
                        return Err(self.fail(
                            "callHierarchy/incomingCalls",
                            "failed to decode callers: ".to_owned() + &error.to_string(),
                        ));
                    }
                };
            if !calls.is_empty() || self.is_quiescent() || start.elapsed() >= budget {
                return Ok(calls);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Best-effort LSP shutdown handshake.
    pub fn shutdown(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "shutdown", "params": null});
        if let Ok(text) = serde_json::to_string(&body) {
            let _ignored = write_message(&self.stdin, &text, &self.server_label);
        }
        self.notify("exit", Value::Null);
        if let Some(mut child) = self.child.take() {
            let _ignored = child.wait();
        }
    }
}

impl Drop for LanguageClient {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.shutdown();
        }
    }
}

impl SymbolInfo {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn container(&self) -> Option<&str> {
        self.container_name.as_deref()
    }
    pub fn uri(&self) -> &str {
        &self.location.uri
    }
    pub fn line0(&self) -> u32 {
        self.location.range.start.line
    }
    pub fn char0(&self) -> u32 {
        self.location.range.start.character
    }
}

impl HierarchyItem {
    pub fn name(&self) -> &str {
        &self.name
    }
    /// LSP SymbolKind (2 = Module, 6 = Method, 9 = Constructor, 12 = Function).
    pub fn kind(&self) -> u32 {
        self.kind
    }
    pub fn uri(&self) -> &str {
        &self.uri
    }
    /// 0-based definition span for body extraction.
    pub fn def_span0(&self) -> (u32, u32) {
        (self.range.start.line, self.range.end.line)
    }
    /// 0-based identifier position for follow-up hierarchy requests.
    pub fn sel_pos0(&self) -> (u32, u32) {
        (
            self.selection_range.start.line,
            self.selection_range.start.character,
        )
    }
}

impl IncomingCall {
    pub fn from(&self) -> &HierarchyItem {
        &self.from
    }
    /// 1-based line of the first call site in the caller.
    pub fn call_line1(&self) -> u32 {
        self.from_ranges
            .first()
            .map(|range| range.start.line + 1)
            .unwrap_or(1)
    }
}

impl DocumentSymbol {
    /// 0-based identifier position for hierarchy requests.
    pub fn sel_pos0(&self) -> (u32, u32) {
        (
            self.selection_range.start.line,
            self.selection_range.start.character,
        )
    }
}

/// Convert a `file://` URI to a filesystem path. Handles the common
/// percent-escapes; returns `None` for non-file URIs.
pub fn uri_to_path(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("file://")?;
    let mut out = String::with_capacity(stripped.len());
    let bytes = stripped.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high * 16 + low) as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{innermost_callable, is_callable, uri_to_path, DocumentSymbol, Position, Range};

    fn span(start: u32, end: u32) -> Range {
        Range {
            start: Position {
                line: start,
                character: 0,
            },
            end: Position {
                line: end,
                character: 1,
            },
        }
    }

    fn symbol(kind: u32, start: u32, end: u32, children: Vec<DocumentSymbol>) -> DocumentSymbol {
        DocumentSymbol {
            kind,
            range: span(start, end),
            selection_range: span(start, end),
            children,
        }
    }

    #[test]
    fn callable_kinds_cover_methods_constructors_functions() {
        assert!(is_callable(6));
        assert!(is_callable(9));
        assert!(is_callable(12));
        assert!(!is_callable(2));
        assert!(!is_callable(5));
        assert!(!is_callable(0));
        assert!(!is_callable(255));
    }

    #[test]
    fn innermost_callable_descends_and_skips_modules() {
        let tree = vec![
            symbol(
                2,
                0,
                30,
                vec![
                    symbol(12, 5, 20, vec![symbol(6, 10, 15, vec![])]),
                    symbol(5, 22, 25, vec![]),
                ],
            ),
            symbol(12, 40, 45, vec![]),
        ];
        let found = innermost_callable(&tree, 12);
        assert!(found.is_some());
        if let Some(found) = found {
            assert_eq!(found.sel_pos0(), (10, 0));
        }
        // Inside outer but outside inner: outer wins.
        let found = innermost_callable(&tree, 7);
        assert!(found.is_some());
        if let Some(found) = found {
            assert_eq!(found.range.start.line, 5);
            assert_eq!(found.range.end.line, 20);
        }
        // Inside a non-callable (field): skipped.
        assert!(innermost_callable(&tree, 23).is_none());
        // Inside the module but outside any callable: skipped.
        assert!(innermost_callable(&tree, 2).is_none());
        // Outside everything: None.
        assert!(innermost_callable(&tree, 99).is_none());
        assert!(innermost_callable(&[], 0).is_none());
    }

    #[test]
    fn uri_to_path_decodes_files() {
        assert_eq!(
            uri_to_path("file:///home/u/proj/main.rs"),
            Some("/home/u/proj/main.rs".to_owned())
        );
        assert_eq!(
            uri_to_path("file:///home/my%20dir/a.rs"),
            Some("/home/my dir/a.rs".to_owned())
        );
        assert_eq!(uri_to_path("file:///a%2Fb.rs"), Some("/a/b.rs".to_owned()));
        assert!(uri_to_path("https://example.com/a.rs").is_none());
        assert!(uri_to_path("not-a-uri").is_none());
        assert!(uri_to_path("file://").is_some());
    }

    #[test]
    fn hierarchy_accessors_report_positions() {
        let item = super::HierarchyItem {
            name: "dial".to_owned(),
            kind: 12,
            uri: "file:///w/a.rs".to_owned(),
            range: span(10, 20),
            selection_range: span(10, 10),
        };
        assert_eq!(item.name(), "dial");
        assert_eq!(item.kind(), 12);
        assert_eq!(item.uri(), "file:///w/a.rs");
        assert_eq!(item.def_span0(), (10, 20));
        assert_eq!(item.sel_pos0(), (10, 0));
        let call = super::IncomingCall {
            from: item,
            from_ranges: vec![span(30, 30)],
        };
        assert_eq!(call.from().name(), "dial");
        assert_eq!(call.call_line1(), 31);
        let no_ranges = super::IncomingCall {
            from: super::HierarchyItem {
                name: "x".to_owned(),
                kind: 6,
                uri: "u".to_owned(),
                range: span(0, 0),
                selection_range: span(0, 0),
            },
            from_ranges: Vec::new(),
        };
        assert_eq!(no_ranges.call_line1(), 1);
    }

    #[test]
    fn symbol_info_accessors() {
        let info = super::SymbolInfo {
            name: "f".to_owned(),
            kind: 12,
            location: super::Location {
                uri: "file:///w/b.py".to_owned(),
                range: span(3, 9),
            },
            container_name: Some("mod".to_owned()),
        };
        assert_eq!(info.name(), "f");
        assert_eq!(info.container(), Some("mod"));
        assert_eq!(info.uri(), "file:///w/b.py");
        assert_eq!((info.line0(), info.char0()), (3, 0));
        let bare = super::SymbolInfo {
            name: "g".to_owned(),
            kind: 6,
            location: super::Location {
                uri: "u".to_owned(),
                range: span(0, 0),
            },
            container_name: None,
        };
        assert!(bare.container().is_none());
    }
}
