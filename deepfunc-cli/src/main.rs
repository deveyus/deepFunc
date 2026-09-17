//! deepFunc CLI — workspace discovery, target validation, syntactic scan.
//!
//! v1 engine is a text scan (no type resolution). It finds candidate call
//! sites by identifier and extracts the enclosing `fn` by brace matching.
//! Output carries a note that results are syntactic. The rust-analyzer LSP
//! driver replaces the scan layer next without changing core.

#![forbid(unsafe_code)]

#[cfg(kani)]
mod harness;

use deepfunc_core::{
    parse_target, render_markdown, CallerEntry, CallerNode, CallerRef, Error, Report,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Args {
    project: PathBuf,
    target: String,
    out: Option<PathBuf>,
    fail_if_empty: bool,
}

fn usage() -> String {
    "usage: deepfunc --project <dir> --target <path::to::fn|file.rs:line> [--out <file>] [--fail-if-empty]".to_owned()
}

fn parse_args(argv: &[String]) -> Result<Args, Error> {
    let mut project: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut fail_if_empty = false;
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
            "--fail-if-empty" => fail_if_empty = true,
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

/// Last path segment of `crate::mod::func` — the identifier we scan for.
fn target_ident(target: &str) -> String {
    match target.rsplit("::").next() {
        Some(ident) => ident.to_owned(),
        None => target.to_owned(),
    }
}

/// Find the start line of the enclosing `fn` by scanning upward.
fn enclosing_fn_start(lines: &[&str], call_line: usize) -> Option<usize> {
    let mut index = call_line;
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

/// Extract the enclosing function text by balancing braces from the fn line.
fn extract_fn(lines: &[&str], start: usize) -> (String, u32) {
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
    let body = lines[start..=end].join("\n");
    (body, (end + 1) as u32)
}

fn signature_of(body: &str) -> String {
    match body.lines().next() {
        Some(first) => first.trim().to_owned(),
        None => String::new(),
    }
}

fn fn_name_of(body: &str, fallback_file: &str) -> String {
    let first = body.lines().next().unwrap_or("");
    if let Some(pos) = first.find("fn ") {
        let rest = &first[pos + 3..];
        let end = rest
            .find(|c: char| matches!(c, '(' | '<' | ' '))
            .unwrap_or(rest.len());
        let name = rest[..end].trim();
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    fallback_file.to_owned()
}

fn scan_workspace(root: &Path, target: &str) -> Result<Vec<CallerEntry>, Error> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files)?;
    let ident = target_ident(target);
    let needle = ident.clone() + "(";
    let mut depth1: Vec<CallerEntry> = Vec::new();
    for path in &files {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                return Err(Error::Io {
                    path: path.display().to_string(),
                    message: error.to_string(),
                })
            }
        };
        let lines: Vec<&str> = text.lines().collect();
        for (line_idx, line) in lines.iter().enumerate() {
            if !line.contains(needle.as_str()) {
                continue;
            }
            let start = match enclosing_fn_start(&lines, line_idx) {
                Some(start) => start,
                None => continue,
            };
            let (body, end_line) = extract_fn(&lines, start);
            // Skip the target's own definition line matching itself.
            if body.contains(&("fn ".to_owned() + &ident + "(")) && start == line_idx {
                continue;
            }
            let relative = match path.strip_prefix(root) {
                Ok(rel) => rel.display().to_string(),
                Err(_) => path.display().to_string(),
            };
            let caller = CallerNode {
                name: fn_name_of(&body, &relative),
                file: relative,
                line: (start + 1) as u32,
                end_line,
                signature: signature_of(&body),
                body,
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
                let other_lines: Vec<&str> = other_text.lines().collect();
                for (other_idx, other_line) in other_lines.iter().enumerate() {
                    if !other_line.contains(caller_needle.as_str()) {
                        continue;
                    }
                    let other_start = match enclosing_fn_start(&other_lines, other_idx) {
                        Some(start) => start,
                        None => continue,
                    };
                    let (other_body, _) = extract_fn(&other_lines, other_start);
                    let other_rel = match other.strip_prefix(root) {
                        Ok(rel) => rel.display().to_string(),
                        Err(_) => other.display().to_string(),
                    };
                    called_by.push(CallerRef {
                        name: fn_name_of(&other_body, &other_rel),
                        file: other_rel,
                        line: (other_idx + 1) as u32,
                        signature: signature_of(&other_body),
                    });
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

fn run(argv: &[String]) -> Result<String, Error> {
    let args = parse_args(argv)?;
    let target = parse_target(&args.target)?;
    let root = find_workspace_root(&args.project)?;
    let depth1 = scan_workspace(&root, &target)?;
    let empty = depth1.is_empty();
    let report = Report {
        target: target.clone(),
        workspace: root.display().to_string(),
        depth1,
        empty,
    };
    let mut markdown = render_markdown(&report);
    markdown.push_str("\n---\nnote: v1 syntactic scan — names matched textually, not type-resolved. Suspected false positives: methods with the same name in other impls. The rust-analyzer driver (E01/E04/E06 paths) replaces this layer next.\n");
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
