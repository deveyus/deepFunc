//! deepFunc CLI — rust-analyzer LSP driver with a syntactic scan fallback.
//!
//! Default engine resolves the target through rust-analyzer
//! (`workspace/symbol` then `prepareCallHierarchy` / `incomingCalls`), so
//! results are type-accurate. `--scan` keeps the old textual matcher for
//! offline use; its output footer says so.

#![forbid(unsafe_code)]

mod lsp;

#[cfg(kani)]
mod harness;

use deepfunc_core::{
    caller_from_range, caller_from_text, parse_target, render_markdown, CallerEntry, CallerRef,
    Error, Report,
};
use lsp::{uri_to_path, RaClient};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Args {
    project: PathBuf,
    target: String,
    out: Option<PathBuf>,
    fail_if_empty: bool,
    scan: bool,
    ra_bin: String,
    timeout_secs: u64,
}

fn usage() -> String {
    "usage: deepfunc --project <dir> --target <path::to::fn|file.rs:line> [--out <file>] [--fail-if-empty] [--scan] [--ra-bin <path>] [--timeout <secs>]".to_owned()
}

fn parse_u64(raw: &str, flag: &str) -> Result<u64, Error> {
    match raw.parse::<u64>() {
        Ok(value) => Ok(value),
        Err(_) => Err(Error::BadTarget {
            received: "bad value for `".to_owned() + flag + "`: `" + raw + "`. " + &usage(),
        }),
    }
}

fn parse_args(argv: &[String]) -> Result<Args, Error> {
    let mut project: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut fail_if_empty = false;
    let mut scan = false;
    let mut ra_bin = "rust-analyzer".to_owned();
    let mut timeout_secs: u64 = 120;
    let mut index = 1;
    while index < argv.len() {
        match argv[index].as_str() {
            "--project" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => project = Some(PathBuf::from(value)),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--project without a value".to_owned(),
                        })
                    }
                }
            }
            "--target" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => target = Some(value.clone()),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--target without a value".to_owned(),
                        })
                    }
                }
            }
            "--out" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => out = Some(PathBuf::from(value)),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--out without a value".to_owned(),
                        })
                    }
                }
            }
            "--ra-bin" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => ra_bin = value.clone(),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--ra-bin without a value".to_owned(),
                        })
                    }
                }
            }
            "--timeout" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => timeout_secs = parse_u64(value, "--timeout")?,
                    None => {
                        return Err(Error::BadTarget {
                            received: "--timeout without a value".to_owned(),
                        })
                    }
                }
            }
            "--fail-if-empty" => fail_if_empty = true,
            "--scan" => scan = true,
            "--help" | "-h" => {
                return Err(Error::BadTarget {
                    received: "help requested. ".to_owned() + &usage(),
                })
            }
            other => {
                return Err(Error::BadTarget {
                    received: "unknown flag `".to_owned() + other + "`. " + &usage(),
                })
            }
        }
        index += 1;
    }
    match (project, target) {
        (Some(project), Some(target)) => Ok(Args {
            project,
            target,
            out,
            fail_if_empty,
            scan,
            ra_bin,
            timeout_secs,
        }),
        _ => Err(Error::BadTarget {
            received: "missing --project or --target. ".to_owned() + &usage(),
        }),
    }
}

/// Walk up from `start` until a directory containing Cargo.toml is found.
fn find_workspace_root(start: &Path) -> Result<PathBuf, Error> {
    let mut current: PathBuf = if start.is_absolute() {
        start.to_path_buf()
    } else {
        match env::current_dir() {
            Ok(cwd) => cwd.join(start),
            Err(error) => {
                return Err(Error::Io {
                    path: start.display().to_string(),
                    message: error.to_string(),
                })
            }
        }
    };
    if current.is_file() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => {
                return Err(Error::WorkspaceNotFound {
                    start_dir: start.display().to_string(),
                    depth: 0,
                })
            }
        }
    }
    let mut depth: u32 = 0;
    loop {
        if current.join("Cargo.toml").is_file() {
            return Ok(current);
        }
        match current.parent() {
            Some(parent) => {
                current = parent.to_path_buf();
                depth += 1;
                if depth > 32 {
                    return Err(Error::WorkspaceNotFound {
                        start_dir: start.display().to_string(),
                        depth,
                    });
                }
            }
            None => {
                return Err(Error::WorkspaceNotFound {
                    start_dir: start.display().to_string(),
                    depth,
                })
            }
        }
    }
}

fn read_file(path: &Path) -> Result<String, Error> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) => Err(Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

/// Workspace-relative display path; falls back to the absolute path.
fn display_path(root: &Path, absolute: &Path) -> String {
    match absolute.strip_prefix(root) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => absolute.display().to_string(),
    }
}

fn collect_rs_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            return Err(Error::Io {
                path: root.display().to_string(),
                message: error.to_string(),
            })
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Err(Error::Io {
                    path: root.display().to_string(),
                    message: error.to_string(),
                })
            }
        };
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name == ".git" {
                continue;
            }
            collect_rs_files(&path, files)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// Last path segment of `crate::mod::func` — the identifier to search for.
fn target_ident(target: &str) -> String {
    match target.rsplit("::").next() {
        Some(ident) => ident.to_owned(),
        None => target.to_owned(),
    }
}

fn scan_workspace(root: &Path, target: &str) -> Result<Vec<CallerEntry>, Error> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files)?;
    let ident = target_ident(target);
    let needle = ident.clone() + "(";
    let mut depth1: Vec<CallerEntry> = Vec::new();
    for path in &files {
        let text = read_file(path)?;
        let relative = display_path(root, path);
        let lines: Vec<&str> = text.lines().collect();
        for (line_idx, line) in lines.iter().enumerate() {
            if !line.contains(needle.as_str()) {
                continue;
            }
            let start = match deepfunc_core::enclosing_fn_start0(&lines, line_idx) {
                Some(start) => start,
                None => continue,
            };
            let (body, _) = deepfunc_core::fn_block(&lines, start);
            // Skip the target's own definition line matching itself.
            if body.contains(&("fn ".to_owned() + &ident + "(")) && start == line_idx {
                continue;
            }
            let caller = match caller_from_text(&text, &relative, (start + 1) as u32) {
                Some(caller) => caller,
                None => continue,
            };
            // Depth 2 via second scan for the depth-1 caller name (signature refs only).
            let mut called_by: Vec<CallerRef> = Vec::new();
            let caller_needle = caller.name.clone() + "(";
            for other in &files {
                if other == path {
                    continue;
                }
                let other_text = match fs::read_to_string(other) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                let other_rel = display_path(root, other);
                let other_lines: Vec<&str> = other_text.lines().collect();
                for (other_idx, other_line) in other_lines.iter().enumerate() {
                    if !other_line.contains(caller_needle.as_str()) {
                        continue;
                    }
                    if let Some(child) =
                        caller_from_text(&other_text, &other_rel, (other_idx + 1) as u32)
                    {
                        called_by.push(CallerRef {
                            name: child.name,
                            file: child.file,
                            line: (other_idx + 1) as u32,
                            signature: child.signature,
                        });
                    }
                    if called_by.len() >= 10 {
                        break;
                    }
                }
                if called_by.len() >= 10 {
                    break;
                }
            }
            depth1.push(CallerEntry { caller, called_by });
            if depth1.len() >= 25 {
                break;
            }
        }
        if depth1.len() >= 25 {
            break;
        }
    }
    Ok(depth1)
}

/// Resolve `file.rs:line` targets to (absolute path, 0-based line).
fn parse_file_line(root: &Path, target: &str) -> Option<(PathBuf, u32)> {
    let (path, line) = target.rsplit_once(':')?;
    if !path.ends_with(".rs") || line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u32 = match line.parse() {
        Ok(number) => number,
        Err(_) => return None,
    };
    if number == 0 {
        return None;
    }
    Some((root.join(path), number - 1))
}

/// First non-whitespace character index of a line (for hierarchy position).
fn indent_of(line: &str) -> u32 {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count() as u32
}

fn lsp_workspace(
    root: &Path,
    target: &str,
    ra_bin: &str,
    timeout_secs: u64,
) -> Result<Vec<CallerEntry>, Error> {
    let mut client = RaClient::spawn(ra_bin, root, timeout_secs)?;
    // Resolve the target to one hierarchy item.
    let (uri, line0, character) = if let Some((path, line0)) = parse_file_line(root, target) {
        let text = read_file(&path)?;
        let uri = format!("file://{}", path.display());
        let character = text.lines().nth(line0 as usize).map_or(0, indent_of);
        client.did_open(&uri, &text);
        (uri, line0, character)
    } else {
        let ident = target_ident(target);
        let parents: Vec<&str> = target.split("::").collect();
        let parent = if parents.len() > 1 {
            parents[parents.len() - 2]
        } else {
            ""
        };
        let symbols = client.workspace_symbols(&ident)?;
        let mut exact: Option<(String, u32, u32)> = None;
        let mut fallback: Option<(String, u32, u32)> = None;
        for symbol in &symbols {
            if symbol.name() != ident {
                continue;
            }
            let entry = (symbol.uri().to_owned(), symbol.line0(), symbol.char0());
            if !parent.is_empty() && symbol.container() == Some(parent) {
                exact = Some(entry);
                break;
            }
            if fallback.is_none() {
                fallback = Some(entry);
            }
        }
        let (uri, line0, character) = match exact.or(fallback) {
            Some(entry) => entry,
            None => {
                return Err(Error::TargetNotFound {
                    target: target.to_owned(),
                    workspace: root.display().to_string(),
                })
            }
        };
        match uri_to_path(&uri) {
            Some(path) => {
                let text = read_file(Path::new(&path))?;
                client.did_open(&uri, &text);
            }
            None => {
                return Err(Error::TargetNotFound {
                    target: target.to_owned(),
                    workspace: root.display().to_string(),
                })
            }
        }
        (uri, line0, character)
    };
    let items = client.prepare_hierarchy(&uri, line0, character)?;
    if items.is_empty() {
        return Err(Error::TargetNotFound {
            target: target.to_owned(),
            workspace: root.display().to_string(),
        });
    }
    let target_item = &items[0];
    let incoming = client.incoming_calls(target_item)?;
    let mut depth1: Vec<CallerEntry> = Vec::new();
    for call in incoming.iter().take(25) {
        let caller_uri = call.from().uri().to_owned();
        let caller_path = match uri_to_path(&caller_uri) {
            Some(path) => path,
            None => continue,
        };
        let caller_text = read_file(Path::new(&caller_path))?;
        client.did_open(&caller_uri, &caller_text);
        let relative = display_path(root, Path::new(&caller_path));
        let (start0, end0) = call.from().def_span0();
        let caller = caller_from_range(&caller_text, &relative, call.from().name(), start0, end0);
        // Depth 2: callers of this caller, signature refs only.
        let (sel_line, sel_char) = call.from().sel_pos0();
        let mut called_by: Vec<CallerRef> = Vec::new();
        if let Ok(sub_items) = client.prepare_hierarchy(&caller_uri, sel_line, sel_char) {
            if let Some(sub) = sub_items.first() {
                if let Ok(sub_calls) = client.incoming_calls(sub) {
                    for sub_call in sub_calls.iter().take(10) {
                        let sub_path = match uri_to_path(sub_call.from().uri()) {
                            Some(path) => path,
                            None => continue,
                        };
                        let sub_text = match fs::read_to_string(&sub_path) {
                            Ok(text) => text,
                            Err(_) => continue,
                        };
                        let sub_rel = display_path(root, Path::new(&sub_path));
                        let (sub_start, _) = sub_call.from().def_span0();
                        let sub_lines: Vec<&str> = sub_text.lines().collect();
                        let signature = sub_lines
                            .get(sub_start as usize)
                            .map_or(String::new(), |line| line.trim().to_owned());
                        called_by.push(CallerRef {
                            name: sub_call.from().name().to_owned(),
                            file: sub_rel,
                            line: sub_call.call_line1(),
                            signature,
                        });
                    }
                }
            }
        }
        depth1.push(CallerEntry { caller, called_by });
    }
    client.shutdown();
    Ok(depth1)
}

fn run(argv: &[String]) -> Result<String, Error> {
    let args = parse_args(argv)?;
    let target = parse_target(&args.target)?;
    let root = find_workspace_root(&args.project)?;
    let (depth1, engine_note) = if args.scan {
        let depth1 = scan_workspace(&root, &target)?;
        let note = "note: syntactic scan (`--scan`) — names matched textually, not type-resolved. Suspected false positives: methods with the same name in other impls.";
        (depth1, note)
    } else {
        match lsp_workspace(&root, &target, &args.ra_bin, args.timeout_secs) {
            Ok(depth1) => {
                let note = "note: resolved by rust-analyzer (type-accurate call hierarchy).";
                (depth1, note)
            }
            Err(error) => {
                // E01 (no binary) and E06 (request failure) degrade to the
                // scan with a clear warning instead of hard failing, unless
                // the target itself is unknown (E04) or IO failed (E07).
                match error {
                    Error::RaNotFound { .. } | Error::RequestFailed { .. } => {
                        eprintln!("{error}");
                        eprintln!("warning: falling back to `--scan` (textual matching). Pass `--ra-bin` or raise `--timeout` for accurate results.");
                        let depth1 = scan_workspace(&root, &target)?;
                        let note = "note: SYNTACTIC FALLBACK — rust-analyzer failed (see warning above). Names matched textually; suspected false positives: same-name methods in other impls.";
                        (depth1, note)
                    }
                    _ => return Err(error),
                }
            }
        }
    };
    let empty = depth1.is_empty();
    let report = Report {
        target: target.clone(),
        workspace: root.display().to_string(),
        depth1,
        empty,
    };
    let mut markdown = render_markdown(&report);
    markdown.push_str("\n---\n");
    markdown.push_str(engine_note);
    markdown.push('\n');
    if empty && args.fail_if_empty {
        return Err(Error::TargetNotFound {
            target,
            workspace: root.display().to_string(),
        });
    }
    if let Some(out) = args.out {
        match fs::write(&out, &markdown) {
            Ok(()) => (),
            Err(error) => {
                return Err(Error::Io {
                    path: out.display().to_string(),
                    message: error.to_string(),
                })
            }
        }
    }
    Ok(markdown)
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    match run(&argv) {
        Ok(markdown) => {
            print!("{markdown}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
