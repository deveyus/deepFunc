//! deepFunc core — pure call-graph model, error taxonomy, markdown render.
//! No IO here. The CLI owns LSP and the filesystem.

#![forbid(unsafe_code)]

#[cfg(kani)]
mod harness;

use std::fmt::{self, Display, Formatter};

/// Stable error codes. Never renumber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// E01: language server binary missing.
    ServerNotFound {
        language: String,
        server: String,
        detail: String,
        install_hint: String,
    },
    /// E02: no workspace marker walking up from start dir.
    WorkspaceNotFound {
        start_dir: String,
        depth: u32,
        markers: Vec<String>,
    },
    /// E03: target string did not parse.
    BadTarget { received: String },
    /// E04: prepareCallHierarchy returned nothing.
    TargetNotFound { target: String, workspace: String },
    /// E06: an LSP request failed or timed out.
    RequestFailed {
        server: String,
        request: String,
        timeout_secs: u64,
        detail: String,
    },
    /// E07: filesystem failure with path context.
    Io { path: String, message: String },
    /// E08: language wired but known-broken (loud gate, never silent wrongness).
    Unsupported { language: String, reason: String },
}

impl Error {
    /// Stable code string, e.g. "E01".
    pub fn code(&self) -> &'static str {
        match self {
            Self::ServerNotFound { .. } => "E01",
            Self::WorkspaceNotFound { .. } => "E02",
            Self::BadTarget { .. } => "E03",
            Self::TargetNotFound { .. } => "E04",
            Self::RequestFailed { .. } => "E06",
            Self::Io { .. } => "E07",
            Self::Unsupported { .. } => "E08",
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerNotFound {
                language,
                server,
                detail,
                install_hint,
            } => write!(
                f,
                "error[E01]: language server not found\n --> language: {language}\n  |\n  | note: could not start `{server}` ({detail})\n  | help: install it: {install_hint}\n  | help: or run `deepfunc provision --lang {language}` to download it"
            ),
            Self::WorkspaceNotFound {
                start_dir,
                depth,
                markers,
            } => write!(
                f,
                "error[E02]: workspace root not found\n --> cwd: {start_dir}\n  |\n  | note: walked up {depth} parents, found none of [{}]\n  | help: suspected cause: wrong --project dir. Run from inside the workspace or pass `--project /path/with/<marker>`",
                markers.join(", ")
            ),
            Self::BadTarget { received } => write!(
                f,
                "error[E03]: bad target syntax\n --> target: {received}\n  |\n  | note: want `path::to::function` or `file.rs:line`, got `{received}`\n  | help: suspected cause: shell quoting or a missing path segment. Try `deepfunc --project . 'crate::module::func'`"
            ),
            Self::TargetNotFound { target, workspace } => write!(
                f,
                "error[E04]: target definition not found\n --> workspace: {workspace}\n  |\n  | note: symbol index was ready; `workspace/symbol` and `prepareCallHierarchy` found no definition for `{target}`\n  | help: suspected cause: wrong name or module path. Check spelling; for trait impls use the fully-qualified path; for `file.rs:line` targets confirm the line sits on (or just above) the `fn` item"
            ),
            Self::RequestFailed {
                server,
                request,
                timeout_secs,
                detail,
            } => write!(
                f,
                "error[E06]: {server} request failed\n --> request: {request}\n  |\n  | note: {detail} (timeout {timeout_secs}s)\n  | help: suspected cause: misconfigured project (read the server error above) or a still-loading index (retry with `--timeout 180`)"
            ),
            Self::Io { path, message } => write!(
                f,
                "error[E07]: filesystem failure\n --> path: {path}\n  |\n  | note: {message}\n  | help: suspected cause: missing file or permissions. Check the path exists and is readable"
            ),
            Self::Unsupported { language, reason } => write!(
                f,
                "error[E08]: language supported but unavailable\n --> language: {language}\n  |\n  | note: {reason}\n  | help: no fallback exists by design (a wrong answer is worse than none). Track DESIGN.md §6 for unblock conditions."
            ),
        }
    }
}

impl std::error::Error for Error {}

/// A fully-extracted caller at depth 1 (body included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerNode {
    /// Fully qualified name, e.g. `crate::net::dial`.
    pub name: String,
    /// Source file, workspace-relative when possible.
    pub file: String,
    /// 1-based start line of the enclosing function.
    pub line: u32,
    /// 1-based end line of the enclosing function.
    pub end_line: u32,
    /// Signature line(s), e.g. `pub fn dial(addr: &str) -> Result<()>`.
    pub signature: String,
    /// Full function body text including signature.
    pub body: String,
}

/// A depth-2 reference (signature + call site only, no body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerRef {
    /// Fully qualified name of the depth-2 caller.
    pub name: String,
    /// File containing the depth-2 call site.
    pub file: String,
    /// 1-based line of the call site.
    pub line: u32,
    /// Signature of the depth-2 caller when known.
    pub signature: String,
}

/// One depth-1 caller plus its depth-2 children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerEntry {
    /// Depth-1 caller with full body.
    pub caller: CallerNode,
    /// Depth-2 callers of `caller` (signatures only).
    pub called_by: Vec<CallerRef>,
}

/// Full report model for one target function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Target as requested, e.g. `crate::net::dial`.
    pub target: String,
    /// Workspace root used for resolution.
    pub workspace: String,
    /// Depth-1 callers (full bodies).
    pub depth1: Vec<CallerEntry>,
    /// True when hierarchy returned zero callers.
    pub empty: bool,
}

/// Render a [`Report`] as single-file markdown for LLM consumption.
///
/// Layout: target header, then one section per depth-1 caller with its
/// fenced body, then a compact depth-2 list of signature references.
pub fn render_markdown(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!("# deepFunc: `{}`\n\n", report.target));
    out.push_str(&format!("workspace: `{}`\n\n", report.workspace));
    if report.empty || report.depth1.is_empty() {
        out.push_str("_No callers found._\n\n");
        out.push_str(
            "note: `incomingCalls` returned empty. The function may be a binary entry point, dead code, or reached via dynamic dispatch.\n",
        );
        return out;
    }
    out.push_str(&format!(
        "callers (depth 1, full): {}\n\n",
        report.depth1.len()
    ));
    for entry in &report.depth1 {
        let caller = &entry.caller;
        out.push_str(&format!(
            "## {} ({}:{}-{})\n\n",
            caller.name, caller.file, caller.line, caller.end_line
        ));
        out.push_str("```rust\n");
        out.push_str(&caller.body);
        if !caller.body.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n\n");
        if entry.called_by.is_empty() {
            out.push_str("_No further callers at depth 2._\n\n");
        } else {
            out.push_str(&format!(
                "callers of `{}` (depth 2, signatures):\n\n",
                caller.name
            ));
            for child in &entry.called_by {
                out.push_str(&format!(
                    "- `{}` ({}:{}) — `{}`\n",
                    child.name, child.file, child.line, child.signature
                ));
            }
            out.push('\n');
        }
    }
    out
}

/// First line of a function body, trimmed — the signature.
pub fn signature_of(body: &str) -> String {
    match body.lines().next() {
        Some(first) => first.trim().to_owned(),
        None => String::new(),
    }
}

/// Function name from a Rust body signature line, or `fallback` when
/// unparseable. Only used when the server sent an empty name; every
/// supported server sends names.
pub fn fn_name_of(body: &str, fallback: &str) -> String {
    let first = body.lines().next().unwrap_or("");
    if let Some(pos) = first.find("fn ") {
        let rest = &first[pos + 3..];
        let end = rest.find(['(', '<', ' ']).unwrap_or(rest.len());
        let name = rest[..end].trim();
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    fallback.to_owned()
}

/// Build a [`CallerNode`] from a 0-based LSP symbol range. Used when
/// the language server already resolved the exact definition span.
pub fn caller_from_range(text: &str, file: &str, name: &str, start0: u32, end0: u32) -> CallerNode {
    let lines: Vec<&str> = text.lines().collect();
    let start = (start0 as usize).min(lines.len().saturating_sub(1));
    let end = (end0 as usize).min(lines.len().saturating_sub(1));
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    let body = lines[lo..=hi].join("\n");
    CallerNode {
        name: if name.is_empty() {
            fn_name_of(&body, file)
        } else {
            name.to_owned()
        },
        file: file.to_owned(),
        line: (lo + 1) as u32,
        end_line: (hi + 1) as u32,
        signature: signature_of(&body),
        body,
    }
}
///
/// Validate and normalize a raw `--target` string.
///
/// Accepts `path::to::func`, `path.to.func`, or `file.ext:line`. Returns
/// the trimmed input on success so the LSP layer receives a canonical value.
pub fn parse_target(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::BadTarget {
            received: raw.to_owned(),
        });
    }
    // file.ext:line form (any extension, any language)
    if let Some((path, line)) = trimmed.rsplit_once(':') {
        if path.contains('.') && !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(trimmed.to_owned());
        }
    }
    // Dotted or double-colon path form: normalize :: to . then validate.
    let dotted = trimmed.replace("::", ".");
    let mut segments = 0;
    for seg in dotted.split('.') {
        if seg.is_empty() {
            return Err(Error::BadTarget {
                received: raw.to_owned(),
            });
        }
        let mut chars = seg.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => (),
            _ => {
                return Err(Error::BadTarget {
                    received: raw.to_owned(),
                })
            }
        }
        if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(Error::BadTarget {
                received: raw.to_owned(),
            });
        }
        segments += 1;
    }
    if segments == 0 {
        return Err(Error::BadTarget {
            received: raw.to_owned(),
        });
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        caller_from_range, parse_target, render_markdown, CallerEntry, CallerNode, CallerRef,
        Report,
    };

    fn sample_report() -> Report {
        Report {
            target: "crate::net::dial".to_owned(),
            workspace: "/home/user/proj".to_owned(),
            empty: false,
            depth1: vec![CallerEntry {
                caller: CallerNode {
                    name: "crate::ui::connect".to_owned(),
                    file: "src/ui.rs".to_owned(),
                    line: 41,
                    end_line: 48,
                    signature: "pub fn connect(addr: &str)".to_owned(),
                    body: "pub fn connect(addr: &str) {\n    dial(addr);\n}".to_owned(),
                },
                called_by: vec![CallerRef {
                    name: "crate::main::main".to_owned(),
                    file: "src/main.rs".to_owned(),
                    line: 12,
                    signature: "fn main()".to_owned(),
                }],
            }],
        }
    }

    #[test]
    fn render_contains_depth1_body_and_depth2_signature() {
        let markdown = render_markdown(&sample_report());
        assert!(markdown.contains("crate::ui::connect"));
        assert!(markdown.contains("dial(addr);"));
        assert!(markdown.contains("crate::main::main"));
        assert!(markdown.contains("src/main.rs:12"));
    }

    #[test]
    fn render_empty_report_notes_dispatch() {
        let report = Report {
            target: "crate::x::y".to_owned(),
            workspace: "/w".to_owned(),
            empty: true,
            depth1: Vec::new(),
        };
        let markdown = render_markdown(&report);
        assert!(markdown.contains("No callers found"));
    }

    #[test]
    fn parse_target_accepts_path_and_file_line() {
        assert_eq!(
            parse_target("crate::net::dial").unwrap_or_default(),
            "crate::net::dial"
        );
        assert_eq!(
            parse_target("os.path.join").unwrap_or_default(),
            "os.path.join"
        );
        assert_eq!(
            parse_target("src/net.rs:120").unwrap_or_default(),
            "src/net.rs:120"
        );
        assert_eq!(
            parse_target("pkg/mod.py:7").unwrap_or_default(),
            "pkg/mod.py:7"
        );
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target("").is_err());
        assert!(parse_target("foo::").is_err());
        assert!(parse_target("9lives::x").is_err());
        assert!(parse_target("foo..bar").is_err());
    }

    #[test]
    fn caller_from_range_slices_exact_span() {
        let text = "line1\nfn target() {\n    body();\n}\nline5\n";
        let caller = caller_from_range(text, "src/a.rs", "target", 1, 3);
        assert_eq!(caller.line, 2);
        assert_eq!(caller.end_line, 4);
        assert_eq!(caller.signature, "fn target() {");
    }

    #[test]
    fn error_display_carries_code_and_help() {
        let error = super::Error::WorkspaceNotFound {
            start_dir: "/tmp/foo".to_owned(),
            depth: 4,
            markers: vec!["Cargo.toml".to_owned()],
        };
        let text = format!("{error}");
        assert!(text.contains("E02"));
        assert!(text.contains("help:"));
    }
}
