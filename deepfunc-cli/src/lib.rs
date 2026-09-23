//! deepFunc library — shared resolution engine for the CLI and MCP server.
//!
//! Pure-ish plumbing over `deepfunc_core` and the `lsp` client: language
//! table, workspace discovery, target resolution, and report building.
//! No arg parsing, no printing, no process exit here.

#![forbid(unsafe_code)]

pub mod lsp;

use deepfunc_core::{caller_from_range, CallerEntry, CallerRef, Error, Report};
use lsp::{innermost_callable, uri_to_path, LanguageClient};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// A supported language: server command, workspace markers, and install help.
pub struct Language {
    pub id: &'static str,
    pub language_id: &'static str,
    pub server_cmd: &'static [&'static str],
    pub markers: &'static [&'static str],
    pub extensions: &'static [&'static str],
    pub install_hint: &'static str,
    /// Set when the path is wired but verified broken: fail loudly (E08)
    /// instead of returning guesses. Remove when the server is fixed.
    pub broken: Option<&'static str>,
}

pub const LANGUAGES: &[Language] = &[
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

pub fn language(id: &str) -> Result<&'static Language, Error> {
    match LANGUAGES.iter().find(|lang| lang.id == id) {
        Some(lang) => Ok(lang),
        None => Err(Error::BadTarget {
            received: "unknown --lang `".to_owned()
                + id
                + "`. Supported: rust, python, typescript, go.",
        }),
    }
}

/// Default servers root: `$XDG_DATA_HOME/deepfunc/servers`, falling back
/// to `~/.local/share/deepfunc/servers`, and on Windows to
/// `%LOCALAPPDATA%/deepfunc/servers`.
pub fn default_servers_dir() -> Result<PathBuf, Error> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("deepfunc/servers"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join(".local/share/deepfunc/servers"));
        }
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            return Ok(PathBuf::from(local).join("deepfunc/servers"));
        }
    }
    Err(Error::Io {
        path: "servers dir".to_owned(),
        message: "cannot determine servers dir: set $XDG_DATA_HOME, $HOME (or %LOCALAPPDATA% on Windows), or pass --dir".to_owned(),
    })
}

/// Resolve a previously provisioned server: (program, no extra args —
/// provision wrappers carry their own). Returns None when no manifest
/// exists, it is corrupt, or the recorded program is gone. A corrupt
/// manifest is ignored (fresh provision overwrites it).
pub fn manifest_server(root: &Path, lang_id: &str) -> Option<(String, Vec<String>)> {
    let text = std::fs::read_to_string(root.join(lang_id).join("manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    let program = manifest.get("program")?.as_str()?.to_owned();
    if program.is_empty() || !Path::new(&program).is_file() {
        return None;
    }
    Some((program, Vec::new()))
}

/// Walk up from `start` until a directory containing one of `markers` is found.
pub fn find_workspace_root(start: &Path, markers: &[&str]) -> Result<PathBuf, Error> {
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

pub fn read_file(path: &Path) -> Result<String, Error> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) => Err(Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        }),
    }
}

/// Workspace-relative display path; falls back to the absolute path.
pub fn display_path(root: &Path, absolute: &Path) -> String {
    match absolute.strip_prefix(root) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => absolute.display().to_string(),
    }
}

/// Last segment of `a::b::func` or `a.b.func` — the identifier to search for.
pub fn target_ident(target: &str) -> String {
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
pub fn seed_file(root: &Path, extensions: &[&str]) -> Option<PathBuf> {
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
pub fn parse_file_line(root: &Path, target: &str) -> Option<(PathBuf, u32)> {
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

/// Spawn a language server client: resolve the program (explicit
/// `--server-bin`, table program on PATH, provisioned manifest),
/// start it, seed one open file, and wait for index readiness.
/// Server resolution order means E01 fires only when all miss.
pub fn spawn_client(
    root: &Path,
    lang: &Language,
    server_bin: Option<&str>,
    timeout_secs: u64,
) -> Result<LanguageClient, Error> {
    // Server resolution, in order (E01 only when all miss):
    // - `--server-bin "program [args...]"` (whitespace-split; no spaces
    //   in server paths): explicit and complete, e.g. provision wrappers.
    // - table program on PATH, plus table args.
    // - previously provisioned manifest (complete wrapper command).
    let (program, extra): (String, Vec<String>) = match server_bin {
        Some(cmdline) => {
            let mut parts = cmdline.split_whitespace();
            match parts.next() {
                Some(program) => (
                    program.to_owned(),
                    parts.map(|arg| arg.to_owned()).collect(),
                ),
                None => {
                    return Err(Error::BadTarget {
                        received: "--server-bin without a value".to_owned(),
                    })
                }
            }
        }
        None => {
            let table_program = lang.server_cmd[0].to_owned();
            let on_path = std::env::var_os("PATH").is_some_and(|paths| {
                std::env::split_paths(&paths).any(|dir| dir.join(&table_program).is_file())
            });
            if on_path {
                (
                    table_program,
                    lang.server_cmd[1..]
                        .iter()
                        .map(|arg| arg.to_string())
                        .collect(),
                )
            } else {
                match default_servers_dir()
                    .ok()
                    .and_then(|root| manifest_server(&root, lang.id))
                {
                    Some((program, _)) => (program, Vec::new()),
                    None => {
                        return Err(Error::ServerNotFound {
                            language: lang.id.to_owned(),
                            server: lang.server_cmd.join(" "),
                            detail: "not on PATH and no provisioned copy".to_owned(),
                            install_hint: lang.install_hint.to_owned(),
                        })
                    }
                }
            }
        }
    };
    let mut client = LanguageClient::spawn(
        &program,
        &extra,
        root,
        timeout_secs,
        lang.id,
        lang.install_hint,
    )?;
    // Readiness first, lookup second. The canary gate waits for a loaded
    // index; after it passes, empty answers mean genuinely unknown.
    // Seed one open file first: some servers need it before the project
    // model (and therefore symbol search) exists at all.
    if let Some(seed) = seed_file(root, lang.extensions) {
        if let Ok(text) = fs::read_to_string(&seed) {
            if let Ok(uri) = lsp::file_uri(&seed) {
                client.did_open(&uri, lang.language_id, &text);
            }
        }
    }
    client.ensure_index_ready();
    Ok(client)
}

/// Resolve one target to depth-1 callers with depth-2 references, using
/// an already-ready client. Server diagnostics collected during the run
/// print to stderr on failure.
pub fn resolve_report(
    client: &mut LanguageClient,
    root: &Path,
    target: &str,
    lang: &Language,
) -> Result<Vec<CallerEntry>, Error> {
    let result = resolve_inner(client, root, target, lang);
    if result.is_err() {
        // Loud failure: show what the server volunteered (config errors,
        // crash notices) alongside the typed error below.
        for note in client.drain_notes() {
            eprintln!("warning: [server] {note}");
        }
    }
    result
}

fn resolve_inner(
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
            if let Ok(uri) = lsp::file_uri(&seed) {
                client.did_open(&uri, lang.language_id, &text);
            }
        }
    }
    client.ensure_index_ready();
    // Resolve the target to one hierarchy item.
    let (uri, line0, character) = if let Some((path, line0)) = parse_file_line(root, target) {
        let text = read_file(&path)?;
        let uri = lsp::file_uri(&path)?;
        client.did_open(&uri, lang.language_id, &text);
        // Servers resolve identifier positions reliably but NOT arbitrary
        // body positions (verified: mid-body returns []), and symbol ranges
        // include doc comments whose positions resolve to nothing (verified:
        // range.start on a doc line returns null). So map file:line to a
        // NAME via documentSymbol, then re-resolve through workspace/symbol
        // whose positions are identifier-based — the same proven path as
        // path targets.
        let symbols = client.document_symbols(&uri, line0)?;
        tracing::debug!(
            count = symbols.len(),
            line0,
            "document symbols for target file"
        );
        let name = match innermost_callable(&symbols, line0) {
            Some(symbol) => symbol.name.clone(),
            None => {
                return Err(Error::TargetNotFound {
                    target: target.to_owned(),
                    workspace: root.display().to_string(),
                })
            }
        };
        tracing::debug!(name = name.as_str(), "file target mapped to symbol");
        let candidates = client.workspace_symbols(&name)?;
        let wanted = path.display().to_string();
        let mut picked: Option<(String, u32, u32)> = None;
        for symbol in &candidates {
            if symbol.name() != name {
                continue;
            }
            let same_file = match uri_to_path(symbol.uri()) {
                Some(back) => back == wanted,
                None => symbol.uri() == uri,
            };
            if same_file {
                picked = Some((symbol.uri().to_owned(), symbol.line0(), symbol.char0()));
                break;
            }
        }
        match picked {
            Some(entry) => entry,
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
    tracing::debug!(name = target_item.name(), "target resolved");
    let incoming = client.incoming_calls(target_item)?;
    tracing::info!(depth1 = incoming.len(), "direct callers found");
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
    Ok(depth1)
}

/// Build the full markdown report for one target: parse, resolve,
/// render, engine footer. Returns the markdown and whether the report
/// is empty (no callers). Does NOT shut the client down: the CLI shuts
/// down explicitly, holdings keep the client alive across calls.
pub fn build_markdown(
    client: &mut LanguageClient,
    root: &Path,
    target_raw: &str,
    lang: &Language,
) -> Result<(String, bool), Error> {
    let target = deepfunc_core::parse_target(target_raw)?;
    let depth1 = resolve_report(client, root, &target, lang)?;
    let empty = depth1.is_empty();
    let report = Report {
        target: target.clone(),
        workspace: root.display().to_string(),
        depth1,
        empty,
    };
    let mut markdown = deepfunc_core::render_markdown(&report);
    markdown.push_str("\n---\nnote: resolved by ");
    markdown.push_str(lang.server_cmd[0]);
    markdown.push_str(" (type-accurate call hierarchy).\n");
    Ok((markdown, empty))
}

/// Install the stderr tracing subscriber. Level from DEEPFUNC_LOG
/// (trace|debug|info|warn|error), default warn. Idempotent: late calls
/// (tests) are ignored.
pub fn init_logging() {
    let level = std::env::var("DEEPFUNC_LOG").unwrap_or_default();
    let max = match level.to_ascii_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "info" => tracing::Level::INFO,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::WARN,
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(max)
        .with_writer(std::io::stderr)
        .try_init();
}

/// `deepfunc provision [--lang ID]... [--all] [--version V] [--dir D]`
/// argument parsing shared by the binary and embedding users.
/// Returns (languages, version override, servers root).
pub fn parse_provision_args(
    argv: &[String],
) -> Result<(Vec<String>, Option<String>, PathBuf), Error> {
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
        None => default_servers_dir()?,
    };
    Ok((langs, version, root))
}

#[cfg(test)]
mod tests {
    use super::{display_path, find_workspace_root, language, parse_file_line, target_ident};
    use std::path::Path;

    #[test]
    fn language_table_resolves_all() {
        for id in ["rust", "python", "typescript", "go"] {
            let resolved = language(id);
            assert!(resolved.is_ok());
            if let Ok(lang) = resolved {
                assert_eq!(lang.id, id);
                assert!(!lang.server_cmd.is_empty());
                assert!(!lang.markers.is_empty());
                assert!(!lang.install_hint.is_empty());
            }
        }
        assert!(language("cobol").is_err());
        assert!(language("").is_err());
        // Only typescript carries the loud broken flag.
        assert!(matches!(language("typescript"), Ok(lang) if lang.broken.is_some()));
        assert!(matches!(language("rust"), Ok(lang) if lang.broken.is_none()));
    }

    #[test]
    fn parse_provision_args_splits_request() {
        let parsed = super::parse_provision_args(&[
            "deepfunc".to_owned(),
            "provision".to_owned(),
            "--lang".to_owned(),
            "go".to_owned(),
            "--version".to_owned(),
            "v1".to_owned(),
            "--dir".to_owned(),
            "/tmp/x".to_owned(),
        ]);
        assert!(parsed.is_ok());
        if let Ok((langs, version, root)) = parsed {
            assert_eq!(langs, vec!["go".to_owned()]);
            assert_eq!(version, Some("v1".to_owned()));
            assert_eq!(root, std::path::Path::new("/tmp/x").to_path_buf());
        }
        assert!(
            super::parse_provision_args(&["deepfunc".to_owned(), "provision".to_owned()]).is_err()
        );
        assert!(super::parse_provision_args(&[
            "deepfunc".to_owned(),
            "provision".to_owned(),
            "--lang".to_owned(),
        ])
        .is_err());
        assert!(super::parse_provision_args(&[
            "deepfunc".to_owned(),
            "provision".to_owned(),
            "--nope".to_owned(),
        ])
        .is_err());
        let all = super::parse_provision_args(&[
            "deepfunc".to_owned(),
            "provision".to_owned(),
            "--all".to_owned(),
        ]);
        assert!(all.is_ok());
        if let Ok((langs, _, _)) = all {
            assert_eq!(langs.len(), 4);
        }
    }

    #[test]
    fn workspace_root_walks_up_and_cleans() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            let nested = dir.path().join("a/b");
            assert!(std::fs::create_dir_all(&nested).is_ok());
            assert!(std::fs::write(dir.path().join("Cargo.toml"), "[package]").is_ok());
            let root = find_workspace_root(&nested, &["Cargo.toml"]);
            assert!(root.is_ok());
            if let Ok(root) = root {
                assert_eq!(root, dir.path());
                assert!(!root.display().to_string().ends_with('.'));
            }
        }
    }

    #[test]
    fn workspace_root_reports_markers() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            let error = find_workspace_root(dir.path(), &["Cargo.toml"]);
            assert!(error.is_err());
            if let Err(error) = error {
                let text = format!("{error}");
                assert!(text.contains("E02"));
                assert!(text.contains("Cargo.toml"));
            }
            let missing = find_workspace_root(&dir.path().join("nope"), &["go.mod"]);
            assert!(missing.is_err());
            if let Err(error) = missing {
                assert!(format!("{error}").contains("go.mod"));
            }
        }
    }

    #[test]
    fn target_ident_takes_last_segment() {
        assert_eq!(target_ident("a::b::func"), "func");
        assert_eq!(target_ident("os.path.join"), "join");
        assert_eq!(target_ident("solo"), "solo");
    }

    #[test]
    fn parse_file_line_accepts_any_extension() {
        let root = Path::new("/w");
        let parsed = parse_file_line(root, "src/a.go:12");
        assert!(parsed.is_some());
        if let Some((path, line)) = parsed {
            assert_eq!(line, 11);
            assert!(path.ends_with("src/a.go"));
        }
        assert!(parse_file_line(root, "crate::m::f").is_none());
        assert!(parse_file_line(root, "a.rs:0").is_none());
        assert!(parse_file_line(root, "a.rs:").is_none());
        assert!(parse_file_line(root, "a.rs:1x").is_none());
        assert!(parse_file_line(root, "nodot:12").is_none());
    }

    #[test]
    fn display_path_relativizes() {
        let root = Path::new("/w");
        assert_eq!(display_path(root, Path::new("/w/a.rs")), "a.rs");
        assert_eq!(display_path(root, Path::new("/other/a.rs")), "/other/a.rs");
    }

    #[test]
    fn seed_file_finds_sources_and_skips_build_dirs() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            assert!(std::fs::create_dir_all(dir.path().join("target")).is_ok());
            assert!(std::fs::write(dir.path().join("target/skip.rs"), "fn skip() {}").is_ok());
            assert!(std::fs::write(dir.path().join("main.rs"), "fn main() {}").is_ok());
            let found = super::seed_file(dir.path(), &["rs"]);
            assert!(found.is_some());
            if let Some(found) = found {
                assert!(found.ends_with("main.rs"));
            }
            assert!(super::seed_file(dir.path(), &["go"]).is_none());
        }
    }
}
