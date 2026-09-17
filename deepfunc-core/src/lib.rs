//! deepFunc core — pure call-graph model, error taxonomy, markdown render.
//! No IO here. The CLI owns LSP and the filesystem.

#![forbid(unsafe_code)]

#[cfg(kani)]
mod harness;

use std::fmt::{self, Display, Formatter};

/// Stable error codes. Never renumber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// E01: rust-analyzer binary missing.
    RaNotFound { searched_path: String },
    /// E02: no Cargo.toml walking up from start dir.
    WorkspaceNotFound { start_dir: String, depth: u32 },
    /// E03: target string did not parse.
    BadTarget { received: String },
    /// E04: prepareCallHierarchy returned nothing.
    TargetNotFound { target: String, workspace: String },
    /// E06: an LSP request failed or timed out.
    RequestFailed {
        request: String,
        timeout_secs: u64,
        detail: String,
    },
    /// E07: filesystem failure with path context.
    Io { path: String, message: String },
}

impl Error {
    /// Stable code string, e.g. "E01".
    pub fn code(&self) -> &'static str {
        match self {
            Self::RaNotFound { .. } => "E01",
            Self::WorkspaceNotFound { .. } => "E02",
            Self::BadTarget { .. } => "E03",
            Self::TargetNotFound { .. } => "E04",
            Self::RequestFailed { .. } => "E06",
            Self::Io { .. } => "E07",
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::RaNotFound { searched_path } => write!(
                f,
                "error[E01]: rust-analyzer binary not found\n --> PATH: {searched_path}\n  |\n  | note: searched PATH for `rust-analyzer` and found nothing executable\n  | help: suspected cause: component not installed. Run `rustup component add rust-analyzer` or pass `--ra-bin /path/to/rust-analyzer`"
            ),
            Self::WorkspaceNotFound { start_dir, depth } => write!(
                f,
                "error[E02]: workspace root not found\n --> cwd: {start_dir}\n  |\n  | note: walked up {depth} parents, found no Cargo.toml\n  | help: suspected cause: wrong --project dir. Run from inside the workspace or pass `--project /path/with/Cargo.toml`"
            ),
            Self::BadTarget { received } => write!(
                f,
                "error[E03]: bad target syntax\n --> target: {received}\n  |\n  | note: want `path::to::function` or `file.rs:line`, got `{received}`\n  | help: suspected cause: shell quoting or a missing path segment. Try `deepfunc --project . 'crate::module::func'`"
            ),
            Self::TargetNotFound { target, workspace } => write!(
                f,
                "error[E04]: target definition not found\n --> workspace: {workspace}\n  |\n  | note: `prepareCallHierarchy` returned no definition for `{target}`\n  | help: suspected cause: stale index, cfg-gated code, or a trait impl path. Run `cargo check` in the workspace first, then retry with the fully-qualified path"
            ),
            Self::RequestFailed {
                request,
                timeout_secs,
                detail,
            } => write!(
                f,
                "error[E06]: rust-analyzer request failed\n --> request: {request}\n  |\n  | note: {detail} (timeout {timeout_secs}s)\n  | help: suspected cause: rust-analyzer still indexing. Retry with `--timeout 120`; if it persists, open the workspace in an editor to confirm RA starts cleanly"
            ),
            Self::Io { path, message } => write!(
                f,
                "error[E07]: filesystem failure\n --> path: {path}\n  |\n  | note: {message}\n  | help: suspected cause: missing file or permissions. Check the path exists and is readable"
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

/// Find the 0-based start line of the enclosing `fn` by scanning upward
/// from 0-based `line`.
pub fn enclosing_fn_start0(lines: &[&str], line: usize) -> Option<usize> {
    let mut index = line;
    loop {
        if lines[index].contains("fn ") {
            return Some(index);
        }
        if index == 0 {
            return None;
        }
        index -= 1;
    }
}

/// Extract a function block starting at 0-based `start`. Returns the block
/// text and the 1-based end line, balancing braces from the `fn` line.
pub fn fn_block(lines: &[&str], start: usize) -> (String, u32) {
    let mut depth: i32 = 0;
    let mut seen_open = false;
    let mut end = start;
    for (offset, line) in lines[start..].iter().enumerate() {
        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                seen_open = true;
            } else if ch == '}' {
                depth -= 1;
            }
        }
        if seen_open && depth <= 0 {
            end = start + offset;
            break;
        }
        end = start + offset;
    }
    (lines[start..=end].join("\n"), (end + 1) as u32)
}

/// First line of a function body, trimmed — the signature.
pub fn signature_of(body: &str) -> String {
    match body.lines().next() {
        Some(first) => first.trim().to_owned(),
        None => String::new(),
    }
}

/// Function name from a body signature line, or `fallback` when unparseable.
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

/// Build a full [`CallerNode`] from file text and a 1-based call-site line.
/// Returns `None` when no enclosing `fn` exists (e.g. call at module scope).
pub fn caller_from_text(text: &str, file: &str, call_line1: u32) -> Option<CallerNode> {
    let lines: Vec<&str> = text.lines().collect();
    let call0 = call_line1.checked_sub(1)? as usize;
    if call0 >= lines.len() {
        return None;
    }
    let start = enclosing_fn_start0(&lines, call0)?;
    let (body, end_line) = fn_block(&lines, start);
    Some(CallerNode {
        name: fn_name_of(&body, file),
        file: file.to_owned(),
        line: (start + 1) as u32,
        end_line,
        signature: signature_of(&body),
        body,
    })
}

/// Build a [`CallerNode`] from a 0-based LSP symbol range. Used when
/// rust-analyzer already resolved the exact definition span.
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
/// Accepts `path::to::func` or `file.rs:line`. Returns the trimmed input
/// on success so the LSP layer receives a canonical value.
pub fn parse_target(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::BadTarget {
            received: raw.to_owned(),
        });
    }
    // file.rs:line form
    if let Some((path, line)) = trimmed.rsplit_once(':') {
        if path.ends_with(".rs") && !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(trimmed.to_owned());
        }
    }
    // path::to::func form: at least one ident, segments split by ::
    let mut segments = 0;
    for seg in trimmed.split("::") {
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
        caller_from_range, caller_from_text, parse_target, render_markdown, CallerEntry,
        CallerNode, CallerRef, Report,
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
            parse_target("src/net.rs:120").unwrap_or_default(),
            "src/net.rs:120"
        );
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target("").is_err());
        assert!(parse_target("foo::").is_err());
        assert!(parse_target("9lives::x").is_err());
    }

    #[test]
    fn caller_from_text_extracts_enclosing_fn() {
        let text = "use crate::x;\n\npub fn connect(addr: &str) {\n    dial(addr);\n}\n";
        let caller = caller_from_text(text, "src/ui.rs", 4);
        assert!(caller.is_some());
        if let Some(caller) = caller {
            assert_eq!(caller.name, "connect");
            assert_eq!(caller.line, 3);
            assert!(caller.body.contains("dial(addr);"));
        }
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
        };
        let text = format!("{error}");
        assert!(text.contains("E02"));
        assert!(text.contains("help:"));
    }
}
