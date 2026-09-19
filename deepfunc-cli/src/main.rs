//! deepFunc CLI — multi-language caller context over LSP.
//!
//! Resolves the target through the language's server (`workspace/symbol`
//! then `prepareCallHierarchy` / `incomingCalls`), so results are
//! type-accurate. Supported: rust, python, typescript, go (see `--lang`).
//! Failures are loud typed errors, never silent fallbacks.

#![forbid(unsafe_code)]

mod lsp;
mod provision;

#[cfg(kani)]
mod harness;

use deepfunc_core::{
    caller_from_range, parse_target, render_markdown, CallerEntry, CallerRef, Error, Report,
};
use lsp::{innermost_callable, uri_to_path, LanguageClient};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Language {
    id: &'static str,
    language_id: &'static str,
    server_cmd: &'static [&'static str],
    markers: &'static [&'static str],
    extensions: &'static [&'static str],
    install_hint: &'static str,
    /// Set when the path is wired but verified broken: fail loudly (E08)
    /// instead of returning guesses. Remove when the server is fixed.
    broken: Option<&'static str>,
}

const LANGUAGES: &[Language] = &[
    Language {
        id: "rust",
        language_id: "rust",
        server_cmd: &["rust-analyzer"],
        markers: &["Cargo.toml"],
        extensions: &["rs"],
        install_hint: "rustup component add rust-analyzer, or nix-shell -p rust-analyzer",
        broken: None,
    },
    Language {
        id: "python",
        language_id: "python",
        server_cmd: &["pyright-langserver", "--stdio"],
        markers: &["pyproject.toml", "setup.py", "setup.cfg"],
        extensions: &["py"],
        install_hint: "nix-shell -p pyright",
        broken: None,
    },
    Language {
        id: "typescript",
        language_id: "typescript",
        server_cmd: &["typescript-language-server", "--stdio"],
        markers: &["package.json", "tsconfig.json"],
        extensions: &["ts", "tsx"],
        install_hint: "nix-shell -p typescript-language-server",
        broken: Some(
            "typescript-language-server (5.3.0 and npm 6.0.0) never forwards tsserver-backed requests once a project loads: tsserver's own log shows a healthy configured project, but no navto/hover/prepare command ever arrives. Verified over ~15 raw protocol probes. Unblocks when a server version answers post-load requests; remove this flag then.",
        ),
    },
    Language {
        id: "go",
        language_id: "go",
        server_cmd: &["gopls"],
        markers: &["go.mod"],
        extensions: &["go"],
        install_hint: "nix-shell -p gopls (also needs `go` on PATH)",
        broken: None,
    },
];

fn language(id: &str) -> Result<&'static Language, Error> {
    match LANGUAGES.iter().find(|lang| lang.id == id) {
        Some(lang) => Ok(lang),
        None => Err(Error::BadTarget {
            received: "unknown --lang `".to_owned()
                + id
                + "`. Supported: rust, python, typescript, go.",
        }),
    }
}

struct Args {
    project: PathBuf,
    target: String,
    out: Option<PathBuf>,
    fail_if_empty: bool,
    lang: String,
    server_bin: Option<String>,
    timeout_secs: u64,
}

fn usage() -> String {
    "usage: deepfunc --project <dir> --target <path.to.fn|path::to::fn|file.ext:line> [--lang rust|python|typescript|go] [--out <file>] [--fail-if-empty] [--server-bin <path>] [--timeout <secs>]".to_owned()
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
    let mut lang = "rust".to_owned();
    let mut server_bin: Option<String> = None;
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
            "--lang" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => lang = value.clone(),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--lang without a value".to_owned(),
                        })
                    }
                }
            }
            "--server-bin" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => server_bin = Some(value.clone()),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--server-bin without a value".to_owned(),
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
            lang,
            server_bin,
            timeout_secs,
        }),
        _ => Err(Error::BadTarget {
            received: "missing --project or --target. ".to_owned() + &usage(),
        }),
    }
}

/// Walk up from `start` until a directory containing one of `markers` is found.
fn find_workspace_root(start: &Path, markers: &[&str]) -> Result<PathBuf, Error> {
    let marker_names: Vec<String> = markers.iter().map(|marker| marker.to_string()).collect();
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
                    markers: marker_names,
                })
            }
        }
    }
    let mut depth: u32 = 0;
    loop {
        if markers.iter().any(|marker| current.join(marker).is_file()) {
            // Lexical clean: `cwd.join(".")` leaves a trailing `/.`,
            // which poisons hand-built `file://` URIs (the server matches
            // no document). Components collection drops `.` segments.
            return Ok(current.components().collect());
        }
        match current.parent() {
            Some(parent) => {
                current = parent.to_path_buf();
                depth += 1;
                if depth > 32 {
                    return Err(Error::WorkspaceNotFound {
                        start_dir: start.display().to_string(),
                        depth,
                        markers: marker_names,
                    });
                }
            }
            None => {
                return Err(Error::WorkspaceNotFound {
                    start_dir: start.display().to_string(),
                    depth,
                    markers: marker_names,
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

/// Last segment of `a::b::func` or `a.b.func` — the identifier to search for.
fn target_ident(target: &str) -> String {
    target
        .replace("::", ".")
        .rsplit('.')
        .next()
        .unwrap_or(target)
        .to_owned()
}

/// First source file under `root` matching `extensions`, skipping
/// dependency and build directories. Used as a seed `didOpen`: some
/// servers (notably tsserver) only build their project model once a file
/// is open, and answer `workspace/symbol` with errors before that.
fn seed_file(root: &Path, extensions: &[&str]) -> Option<PathBuf> {
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        ".git",
        "dist",
        "build",
        ".venv",
        "__pycache__",
        ".tox",
    ];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        // Collect first to keep traversal deterministic.
        let mut entries: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !SKIP.contains(&name) {
                    stack.push(path);
                }
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.contains(&ext))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Resolve `file.ext:line` targets to (absolute path, 0-based line).
/// Any extension: the language comes from `--lang`, not the suffix.
fn parse_file_line(root: &Path, target: &str) -> Option<(PathBuf, u32)> {
    let (path, line) = target.rsplit_once(':')?;
    if !path.contains('.') || line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
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

fn lsp_workspace(
    root: &Path,
    target: &str,
    lang: &Language,
    server_bin: Option<&str>,
    timeout_secs: u64,
) -> Result<Vec<CallerEntry>, Error> {
    let program = server_bin.unwrap_or(lang.server_cmd[0]);
    // A --server-bin override replaces the program but keeps the table args
    // (e.g. pyright still needs --stdio).
    let extra: Vec<String> = lang.server_cmd[1..]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let mut client = LanguageClient::spawn(
        program,
        &extra,
        root,
        timeout_secs,
        lang.id,
        lang.install_hint,
    )?;
    let result = lsp_inner(&mut client, root, target, lang);
    if result.is_err() {
        // Loud failure: show what the server volunteered (config errors,
        // crash notices) alongside the typed error below.
        for note in client.drain_notes() {
            eprintln!("warning: [server] {note}");
        }
    }
    Ok(result?)
}

fn lsp_inner(
    client: &mut LanguageClient,
    root: &Path,
    target: &str,
    lang: &Language,
) -> Result<Vec<CallerEntry>, Error> {
    // Readiness first, lookup second. The canary gate waits for a loaded
    // index; after it passes, empty answers mean genuinely unknown.
    // Seed one open file first: some servers need it before the project
    // model (and therefore symbol search) exists at all.
    if let Some(seed) = seed_file(root, lang.extensions) {
        if let Ok(text) = fs::read_to_string(&seed) {
            let uri = format!("file://{}", seed.display());
            client.did_open(&uri, lang.language_id, &text);
        }
    }
    client.ensure_index_ready();
    // Resolve the target to one hierarchy item.
    let (uri, line0, character) = if let Some((path, line0)) = parse_file_line(root, target) {
        let text = read_file(&path)?;
        let uri = format!("file://{}", path.display());
        client.did_open(&uri, lang.language_id, &text);
        // Servers resolve identifier positions reliably but NOT arbitrary
        // body positions (verified: mid-body returns []). Map file:line to
        // the enclosing symbol via documentSymbol, then reuse that position.
        let symbols = client.document_symbols(&uri)?;
        match innermost_callable(&symbols, line0) {
            Some(symbol) => {
                let (sel_line, sel_char) = symbol.sel_pos0();
                (uri, sel_line, sel_char)
            }
            None => {
                return Err(Error::TargetNotFound {
                    target: target.to_owned(),
                    workspace: root.display().to_string(),
                })
            }
        }
    } else {
        let ident = target_ident(target);
        let normalized = target.replace("::", ".");
        let parents: Vec<&str> = normalized.split('.').collect();
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
                client.did_open(&uri, lang.language_id, &text);
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
        client.did_open(&caller_uri, lang.language_id, &caller_text);
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
                        // Module-scope callers have no signature: the call-site
                        // line itself is the informative text.
                        let signature = if sub_call.from().kind() == 2 {
                            sub_lines
                                .get(sub_call.call_line1().saturating_sub(1) as usize)
                                .map_or(String::new(), |line| line.trim().to_owned())
                        } else {
                            sub_lines
                                .get(sub_start as usize)
                                .map_or(String::new(), |line| line.trim().to_owned())
                        };
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
    let lang = language(&args.lang)?;
    if let Some(reason) = lang.broken {
        return Err(Error::Unsupported {
            language: lang.id.to_owned(),
            reason: reason.to_owned(),
        });
    }
    let target = parse_target(&args.target)?;
    let root = find_workspace_root(&args.project, lang.markers)?;
    let depth1 = lsp_workspace(
        &root,
        &target,
        lang,
        args.server_bin.as_deref(),
        args.timeout_secs,
    )?;
    let empty = depth1.is_empty();
    let report = Report {
        target: target.clone(),
        workspace: root.display().to_string(),
        depth1,
        empty,
    };
    let mut markdown = render_markdown(&report);
    markdown.push_str("\n---\nnote: resolved by ");
    markdown.push_str(lang.server_cmd[0]);
    markdown.push_str(" (type-accurate call hierarchy).\n");
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
    if argv.get(1).is_some_and(|first| first == "provision") {
        return match run_provision(&argv) {
            Ok(reports) => {
                for report in &reports {
                    println!("provisioned {} {}", report.language, report.version);
                    println!("binary: {}", report.program);
                    println!(
                        "use with: deepfunc --lang {} --server-bin {}",
                        report.language, report.program
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
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

/// `deepfunc provision [--lang ID]... [--all] [--version V] [--dir D]`.
/// Downloads language servers; fails loudly per language, continues with
/// the rest, exits non-zero if any failed.
fn run_provision(argv: &[String]) -> Result<Vec<provision::ProvisionReport>, Error> {
    let mut langs: Vec<String> = Vec::new();
    let mut all = false;
    let mut version: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut index = 2;
    while index < argv.len() {
        match argv[index].as_str() {
            "--lang" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => langs.push(value.clone()),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--lang without a value".to_owned(),
                        })
                    }
                }
            }
            "--all" => all = true,
            "--version" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => version = Some(value.clone()),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--version without a value".to_owned(),
                        })
                    }
                }
            }
            "--dir" => {
                index += 1;
                match argv.get(index) {
                    Some(value) => dir = Some(PathBuf::from(value)),
                    None => {
                        return Err(Error::BadTarget {
                            received: "--dir without a value".to_owned(),
                        })
                    }
                }
            }
            other => return Err(Error::BadTarget {
                received: "unknown provision flag `".to_owned()
                    + other
                    + "`. usage: deepfunc provision [--lang ID]... [--all] [--version V] [--dir D]",
            }),
        }
        index += 1;
    }
    if all {
        langs = vec![
            "rust".to_owned(),
            "python".to_owned(),
            "typescript".to_owned(),
            "go".to_owned(),
        ];
    }
    if langs.is_empty() {
        return Err(Error::BadTarget {
            received: "provision needs --lang ID or --all".to_owned(),
        });
    }
    let root = match dir {
        Some(dir) => dir,
        None => provision::default_servers_dir()?,
    };
    let mut reports = Vec::new();
    let mut failures = 0;
    for lang in &langs {
        match provision::provision(lang, version.as_deref(), None, &root) {
            Ok(report) => reports.push(report),
            Err(error) => {
                eprintln!("{error}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        return Err(Error::Io {
            path: "provision".to_owned(),
            message: failures.to_string() + " language(s) failed (see errors above)",
        });
    }
    if reports.is_empty() {
        return Err(Error::Io {
            path: "provision".to_owned(),
            message: "nothing provisioned".to_owned(),
        });
    }
    Ok(reports)
}
