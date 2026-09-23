//! deepFunc MCP server — acquire/release/callers/provision.
//!
//! Transport is stdio; spawn from an MCP client (see `~/mcp/deepfunc/run.sh`).
//! Language servers stay resident across calls: `acquire` loads a workspace
//! and pins it for an explicit TTL, `callers` serves type-accurate context
//! from the holding in seconds, `release` drops it early. TTL expiry is
//! swept on every call. Memory per holding (server RSS) plus system
//! free/total ships with every acquire so the model makes informed
//! keep-or-release choices.
//!
//! Result shapes follow rmcp's guidance: success is unstructured text
//! (no `structuredContent`, which chokes record-expecting clients);
//! CLI failures (the typed E01–E08 errors) come back as TOOL-level errors
//! so the diagnostics stay visible instead of rendering as opaque -32603.
//! Only infrastructure failures (spawn, join, timeout) are protocol errors.

#![forbid(unsafe_code)]

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars;
use rmcp::{tool, tool_router, ErrorData};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Suggested TTL for active work, in seconds. Advisory only: the model
/// always picks explicitly (no default in the tool schema).
const SUGGESTED_TTL_SECS: u64 = 1800;

/// One resident language server.
struct Holding {
    client: Mutex<deepfunc_cli::lsp::LanguageClient>,
    lang_id: String,
    workspace: String,
    program: String,
    last_used: Instant,
    ttl: Duration,
}

/// Shared server state: holdings by `workspace + lang`, plus the CLI
/// binary path for the subprocess-backed `provision` tool.
struct State {
    holdings: Mutex<HashMap<String, Holding>>,
    bin: String,
}

#[derive(Clone)]
pub struct DeepFuncMcp {
    state: Arc<State>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CallersReq {
    /// Workspace directory (or subdir) containing the project marker.
    /// Must match an acquired holding: call acquire first.
    pub project: String,
    /// Target function: `path.to.fn`, `path::to::fn`, or `file.ext:line`.
    pub target: String,
    /// Language: rust (default), python, go. Typescript is wired but
    /// blocked (E08: its server never answers post-load requests).
    pub lang: Option<String>,
    /// Seconds for the run. Default 180. Warm holdings answer in seconds.
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AcquireReq {
    /// Workspace directory (or subdir) containing the project marker.
    pub project: String,
    /// Language: rust, python, go.
    pub lang: Option<String>,
    /// REQUIRED: seconds to hold after last use. Suggested 1800 for
    /// active work. Re-acquire to extend. Expiry is swept on every call.
    pub ttl_secs: u64,
    /// Seconds to allow for initial load. Default 180.
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReleaseReq {
    /// Workspace to release. Omit to release everything held.
    pub project: Option<String>,
    /// Language scope for the release. Omit with project to release all
    /// languages held for it (or everything, if project is omitted too).
    pub lang: Option<String>,
}

fn fail(message: String) -> ErrorData {
    ErrorData::internal_error(message, None)
}

/// Holding key: canonical workspace display plus language.
fn holding_key(workspace: &str, lang_id: &str) -> String {
    workspace.to_owned() + "\0" + lang_id
}

fn holding_expired_at(last_used: Instant, ttl: Duration) -> bool {
    last_used.elapsed() >= ttl
}

fn holding_expired(holding: &Holding) -> bool {
    holding_expired_at(holding.last_used, holding.ttl)
}

fn fmt_mb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_048_576.0)
}

/// Server RSS in bytes, via sysinfo. None when the pid is gone.
fn server_memory(pid: Option<u32>) -> Option<u64> {
    let pid = pid?;
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        false,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.memory())
}

/// (total, available) system memory in bytes.
fn system_memory() -> Option<(u64, u64)> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    Some((system.total_memory(), system.available_memory()))
}

/// CLI outcome split for tool-level reporting: stdout on success,
/// stderr (typed E-codes) as a VISIBLE tool error on failure.
enum CliOutcome {
    ToolError(String),
    InfraError(String),
}

fn run_cli(bin: &str, args: &[String]) -> Result<String, CliOutcome> {
    let output = match std::process::Command::new(bin).args(args).output() {
        Ok(output) => output,
        Err(error) => {
            return Err(CliOutcome::InfraError(
                "failed to spawn deepfunc-cli (".to_owned()
                    + bin
                    + "): "
                    + &error.to_string()
                    + ". Set DEEPFUNC_BIN to the CLI binary.",
            ));
        }
    };
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(CliOutcome::ToolError(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn into_tool_result(outcome: Result<String, CliOutcome>) -> Result<CallToolResult, ErrorData> {
    match outcome {
        Ok(report) => Ok(CallToolResult::success(vec![ContentBlock::text(report)])),
        Err(CliOutcome::ToolError(message)) => {
            Ok(CallToolResult::error(vec![ContentBlock::text(message)]))
        }
        Err(CliOutcome::InfraError(message)) => Err(fail(message)),
    }
}

fn lock_holdings(state: &State) -> std::sync::MutexGuard<'_, HashMap<String, Holding>> {
    state
        .holdings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drop expired holdings. Returns names dropped (for reporting).
fn sweep_expired(state: &State) -> Vec<String> {
    let mut holdings = lock_holdings(state);
    let expired: Vec<String> = holdings
        .iter()
        .filter(|(_, holding)| holding_expired(holding))
        .map(|(key, _)| key.clone())
        .collect();
    for key in &expired {
        holdings.remove(key);
    }
    expired
}

fn holdings_line(state: &State) -> String {
    let holdings = lock_holdings(state);
    if holdings.is_empty() {
        return "holdings (0)".to_owned();
    }
    let mut parts = Vec::new();
    for holding in holdings.values() {
        parts.push(format!(
            "{} [{}] ttl {}s",
            holding.workspace,
            holding.lang_id,
            holding.ttl.as_secs()
        ));
    }
    parts.sort();
    format!("holdings ({}): {}", parts.len(), parts.join("; "))
}

fn memory_line(pid: Option<u32>) -> String {
    let holding = match server_memory(pid) {
        Some(bytes) => fmt_mb(bytes) + " MB rss",
        None => "rss unknown".to_owned(),
    };
    match system_memory() {
        Some((total, available)) => {
            holding
                + " | system: "
                + &fmt_mb(available)
                + " MB free / "
                + &fmt_mb(total)
                + " MB total"
        }
        None => holding + " | system memory unknown",
    }
}

/// Acquire (blocking): reuse a live holding or spawn a fresh server.
/// Returns the human report, or a caller-visible error message.
fn acquire_inner(
    state: &Arc<State>,
    project: &str,
    lang_id: Option<&str>,
    ttl_secs: u64,
    timeout_secs: u64,
) -> Result<String, String> {
    let lang =
        deepfunc_cli::language(lang_id.unwrap_or("rust")).map_err(|error| error.to_string())?;
    if let Some(reason) = lang.broken {
        return Err(deepfunc_core::Error::Unsupported {
            language: lang.id.to_owned(),
            reason: reason.to_owned(),
        }
        .to_string());
    }
    let root = deepfunc_cli::find_workspace_root(std::path::Path::new(project), lang.markers)
        .map_err(|error| error.to_string())?;
    let workspace = root.display().to_string();
    let key = holding_key(&workspace, lang.id);
    sweep_expired(state);
    let started = Instant::now();
    {
        let mut holdings = lock_holdings(state);
        if let Some(holding) = holdings.get_mut(&key) {
            let alive = holding
                .client
                .lock()
                .map(|mut client| client.is_alive())
                .unwrap_or(false);
            if alive {
                holding.ttl = Duration::from_secs(ttl_secs);
                holding.last_used = Instant::now();
                let report = "acquired ".to_owned()
                    + lang.id
                    + " "
                    + &workspace
                    + " (already warm, ttl "
                    + &ttl_secs.to_string()
                    + "s, server "
                    + &holding.program
                    + ")\n"
                    + &memory_line(
                        holding
                            .client
                            .lock()
                            .ok()
                            .and_then(|client| client.child_id()),
                    )
                    + "\n"
                    + &holdings_line(state);
                return Ok(report);
            }
            holdings.remove(&key);
        }
    }
    // Fresh spawn: the CLI timeout needs headroom inside our budget.
    let cli_timeout = timeout_secs.saturating_sub(15).max(30);
    let client = deepfunc_cli::spawn_client(&root, lang, None, cli_timeout)
        .map_err(|error| error.to_string())?;
    let pid = client.child_id();
    {
        let mut holdings = lock_holdings(state);
        holdings.insert(
            key,
            Holding {
                client: Mutex::new(client),
                lang_id: lang.id.to_owned(),
                workspace: workspace.clone(),
                program: lang.server_cmd[0].to_owned(),
                last_used: Instant::now(),
                ttl: Duration::from_secs(ttl_secs),
            },
        );
    }
    Ok("acquired ".to_owned()
        + lang.id
        + " "
        + &workspace
        + " (fresh load "
        + &started.elapsed().as_secs().to_string()
        + "s, ttl "
        + &ttl_secs.to_string()
        + "s)\n"
        + &memory_line(pid)
        + "\n"
        + &holdings_line(state))
}

/// Serve callers from a holding (blocking). No silent cold-spawn: without
/// a live holding the model must pick a TTL via acquire first.
fn callers_inner(
    state: &Arc<State>,
    project: &str,
    target: &str,
    lang_id: Option<&str>,
) -> Result<String, String> {
    let lang =
        deepfunc_cli::language(lang_id.unwrap_or("rust")).map_err(|error| error.to_string())?;
    if let Some(reason) = lang.broken {
        return Err(deepfunc_core::Error::Unsupported {
            language: lang.id.to_owned(),
            reason: reason.to_owned(),
        }
        .to_string());
    }
    let root = deepfunc_cli::find_workspace_root(std::path::Path::new(project), lang.markers)
        .map_err(|error| error.to_string())?;
    let workspace = root.display().to_string();
    let key = holding_key(&workspace, lang.id);
    sweep_expired(state);
    // Phase 1: liveness gate under one lock acquisition. The client guard
    // must drop before any map mutation, so decide first, act after.
    enum Fate {
        Live,
        Gone(String),
    }
    let fate = {
        let mut holdings = lock_holdings(state);
        match holdings.get_mut(&key) {
            None => Fate::Gone(
                "no holding for ".to_owned()
                    + &workspace
                    + " ["
                    + lang.id
                    + "]: call acquire first with an explicit ttl_secs ("
                    + &SUGGESTED_TTL_SECS.to_string()
                    + " suggested for active work).",
            ),
            Some(holding) => {
                if holding_expired(holding) {
                    holdings.remove(&key);
                    Fate::Gone(
                        "holding for ".to_owned()
                            + &workspace
                            + " ["
                            + lang.id
                            + "] lapsed: re-acquire with an explicit ttl_secs.",
                    )
                } else {
                    let alive = holding
                        .client
                        .lock()
                        .map(|mut client| client.is_alive())
                        .unwrap_or(false);
                    if alive {
                        Fate::Live
                    } else {
                        holdings.remove(&key);
                        Fate::Gone("language server died: re-acquire to respawn.".to_owned())
                    }
                }
            }
        }
    };
    match fate {
        Fate::Live => (),
        Fate::Gone(message) => return Err(message),
    }
    // Phase 2: serve. Re-lock; disappearance between phases (concurrent
    // release) degrades to the acquire-first error, never a panic.
    let mut holdings = lock_holdings(state);
    let holding = match holdings.get_mut(&key) {
        Some(holding) => holding,
        None => {
            return Err("holding vanished mid-call (concurrent release?): re-acquire.".to_owned());
        }
    };
    let mut client = match holding.client.lock() {
        Ok(client) => client,
        Err(_) => {
            return Err(
                "holding lock poisoned by a panicked worker: restart the MCP server.".to_owned(),
            );
        }
    };
    match deepfunc_cli::build_markdown(&mut client, &root, target, lang) {
        Ok((markdown, _)) => {
            holding.last_used = Instant::now();
            Ok(markdown)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Release holdings (blocking): drop makes the shutdown handshake.
fn release_inner(state: &Arc<State>, project: Option<&str>, lang: Option<&str>) -> String {
    sweep_expired(state);
    let mut holdings = lock_holdings(state);
    let keys: Vec<String> = holdings
        .keys()
        .filter(|key| {
            let mut parts = key.split('\0');
            let workspace = parts.next().unwrap_or("");
            let lang_id = parts.next().unwrap_or("");
            if let Some(project) = project {
                if workspace != project {
                    return false;
                }
            }
            if let Some(lang) = lang {
                if lang_id != lang {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();
    let count = keys.len();
    for key in &keys {
        holdings.remove(key);
    }
    drop(holdings);
    if count == 0 {
        "nothing held matching that scope. ".to_owned() + &holdings_line(state)
    } else {
        "released ".to_owned()
            + &count.to_string()
            + ": "
            + &keys.join(", ")
            + ". "
            + &holdings_line(state)
    }
}

#[tool_router(server_handler)]
impl DeepFuncMcp {
    /// Caller context for one function: depth-1 caller bodies plus
    /// depth-2 caller signatures, as one markdown document for LLM
    /// context. Serves from a resident holding (see acquire): warm calls
    /// answer in seconds. Without a holding this fails loudly — it will
    /// not cold-spawn on your behalf because it cannot pick a TTL for you.
    #[tool(
        description = "Caller context for a function (depth-1 bodies, depth-2 signatures) as markdown. Requires an acquired holding: call acquire first. Params: project (workspace dir), target (path.to.fn or file.ext:line), lang (rust|python|go, default rust), timeout_secs (default 180)."
    )]
    async fn callers(
        &self,
        Parameters(req): Parameters<CallersReq>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = Arc::clone(&self.state);
        let budget = req.timeout_secs.unwrap_or(180).max(30);
        tracing::info!(
            target = req.target.as_str(),
            project = req.project.as_str(),
            budget,
            "callers"
        );
        let worker = tokio::task::spawn_blocking(move || {
            callers_inner(&state, &req.project, &req.target, req.lang.as_deref())
        });
        match tokio::time::timeout(Duration::from_secs(budget), worker).await {
            Ok(joined) => match joined {
                Ok(Ok(report)) => Ok(CallToolResult::success(vec![ContentBlock::text(report)])),
                Ok(Err(message)) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                "deepfunc call timed out after ".to_owned()
                    + &budget.to_string()
                    + "s. The holding stays; retry with a larger timeout_secs.",
            )])),
        }
    }

    /// Load a workspace's language server and pin it for an explicit TTL.
    /// The first call pays full load (~30-60s); later calls ride the warm
    /// index in seconds. Re-acquire to extend. Reports the holding's RSS
    /// plus system free/total so retention stays an informed choice.
    #[tool(
        description = "Acquire (warm + pin) a workspace language server. Params: project (workspace dir), lang (rust|python|go, default rust), ttl_secs REQUIRED seconds to hold after last use (1800 suggested for active work), timeout_secs (default 180). Re-acquire to extend TTL. Reports memory held + system free/total."
    )]
    async fn acquire(
        &self,
        Parameters(req): Parameters<AcquireReq>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = Arc::clone(&self.state);
        let budget = req.timeout_secs.unwrap_or(180).max(30);
        tracing::info!(
            project = req.project.as_str(),
            ttl_secs = req.ttl_secs,
            budget,
            "acquire"
        );
        let worker = tokio::task::spawn_blocking(move || {
            acquire_inner(
                &state,
                &req.project,
                req.lang.as_deref(),
                req.ttl_secs,
                budget,
            )
        });
        match tokio::time::timeout(Duration::from_secs(budget), worker).await {
            Ok(joined) => match joined {
                Ok(Ok(report)) => Ok(CallToolResult::success(vec![ContentBlock::text(report)])),
                Ok(Err(message)) => Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                "acquire timed out after ".to_owned()
                    + &budget.to_string()
                    + "s. Nothing is held; retry with a larger timeout_secs.",
            )])),
        }
    }

    /// Drop holdings now instead of waiting out their TTL. Omit project
    /// to release everything held.
    #[tool(
        description = "Release held language servers now. Params: project (omit for all), lang (omit with project for all its languages). Reports what was dropped."
    )]
    async fn release(
        &self,
        Parameters(req): Parameters<ReleaseReq>,
    ) -> Result<CallToolResult, ErrorData> {
        let state = Arc::clone(&self.state);
        tracing::info!(
            project = req.project.as_deref().unwrap_or("<all>"),
            "release"
        );
        let worker = tokio::task::spawn_blocking(move || {
            release_inner(&state, req.project.as_deref(), req.lang.as_deref())
        });
        match worker.await {
            Ok(report) => Ok(CallToolResult::success(vec![ContentBlock::text(report)])),
            Err(error) => Err(fail(
                "deepfunc worker failed: ".to_owned() + &error.to_string(),
            )),
        }
    }

    /// Download a language server when E01 says it is missing. SLOW
    /// (minutes for first downloads): raise the client MCP timeout.
    #[tool(
        description = "Download a language server (rust|python|typescript|go) into a directory. Params: lang, version (optional pin override), dir (optional servers root). Prints the binary path; use it with callers via --server-bin. SLOW minutes on first download: raise client mcp_timeout."
    )]
    async fn provision(
        &self,
        Parameters(req): Parameters<ProvisionReq>,
    ) -> Result<CallToolResult, ErrorData> {
        let bin = self.state.bin.clone();
        tracing::info!(lang = req.lang.as_str(), "provision");
        let mut args = vec!["provision".to_owned(), "--lang".to_owned(), req.lang];
        if let Some(version) = req.version {
            args.push("--version".to_owned());
            args.push(version);
        }
        if let Some(dir) = req.dir {
            args.push("--dir".to_owned());
            args.push(dir);
        }
        let worker = tokio::task::spawn_blocking(move || run_cli(&bin, &args));
        match tokio::time::timeout(Duration::from_secs(900), worker).await {
            Ok(joined) => match joined {
                Ok(outcome) => into_tool_result(outcome),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                "provision timed out after 900s. Retry; downloads resume partially (npm/go caches, rerun overwrites).".to_owned(),
            )])),
        }
    }
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ProvisionReq {
    /// Language server to download: rust, python, typescript, go.
    pub lang: String,
    /// Pinned version override (defaults per language, see DESIGN.md).
    pub version: Option<String>,
    /// Servers root override (default ~/.local/share/deepfunc/servers).
    pub dir: Option<String>,
}

/// Install the stderr tracing subscriber. Level from DEEPFUNC_LOG
/// (trace|debug|info|warn|error), default warn.
fn init_logging() {
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

#[tokio::main]
async fn main() {
    init_logging();
    use rmcp::{transport::stdio, ServiceExt};
    let bin = std::env::var("DEEPFUNC_BIN").unwrap_or_else(|_| "deepfunc-cli".to_owned());
    let service = DeepFuncMcp {
        state: Arc::new(State {
            holdings: Mutex::new(HashMap::new()),
            bin,
        }),
    };
    let server = match service.serve(stdio()).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!("deepfunc-mcp failed to start: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = server.waiting().await {
        eprintln!("deepfunc-mcp exited: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{into_tool_result, run_cli, CliOutcome};
    use rmcp::model::CallToolResult;

    fn is_error(result: &CallToolResult) -> bool {
        result.is_error.unwrap_or(false)
    }

    #[test]
    fn run_cli_reports_all_three_outcomes() {
        // Success: stdout passes through. Shell choice is per-platform:
        // /bin/sh always exists on unix (even NixOS, where /bin holds
        // nothing else); cmd.exe on Windows.
        #[cfg(unix)]
        let ok = run_cli("/bin/sh", &["-c".to_owned(), "echo hi".to_owned()]);
        #[cfg(windows)]
        let ok = run_cli("cmd", &["/C".to_owned(), "echo hi".to_owned()]);
        assert!(ok.is_ok());
        if let Ok(text) = ok {
            assert!(text.contains("hi"));
        }
        // Tool error: non-zero exit surfaces stderr.
        #[cfg(unix)]
        let failed = run_cli(
            "/bin/sh",
            &["-c".to_owned(), "echo oops >&2; exit 3".to_owned()],
        );
        #[cfg(windows)]
        let failed = run_cli(
            "cmd",
            &["/C".to_owned(), "echo oops 1>&2 & exit 3".to_owned()],
        );
        assert!(matches!(failed, Err(CliOutcome::ToolError(_))));
        if let Err(CliOutcome::ToolError(message)) = failed {
            assert!(message.contains("oops"));
        }
        // Infra error: missing binary.
        let missing = run_cli("/nonexistent-deepfunc-test-binary", &[]);
        assert!(matches!(missing, Err(CliOutcome::InfraError(_))));
        if let Err(CliOutcome::InfraError(message)) = missing {
            assert!(message.contains("DEEPFUNC_BIN"));
        }
    }

    #[test]
    fn into_tool_result_maps_shapes() {
        let ok = into_tool_result(Ok("report".to_owned()));
        assert!(ok.is_ok());
        if let Ok(result) = ok {
            assert!(!is_error(&result));
            assert_eq!(result.content.len(), 1);
            assert!(result.structured_content.is_none());
        }
        let tool_err = into_tool_result(Err(CliOutcome::ToolError("E04 ...".to_owned())));
        assert!(tool_err.is_ok());
        if let Ok(result) = tool_err {
            assert!(is_error(&result));
            assert!(result.structured_content.is_none());
        }
        assert!(into_tool_result(Err(CliOutcome::InfraError("x".to_owned()))).is_err());
    }

    #[test]
    fn request_shapes_deserialize() {
        let callers: super::CallersReq =
            serde_json::from_str(r#"{"project":".","target":"a::b"}"#).unwrap_or_default();
        assert_eq!(callers.project, ".");
        assert!(callers.lang.is_none());
        let provision: super::ProvisionReq =
            serde_json::from_str(r#"{"lang":"go","timeout_secs":1}"#).unwrap_or_default();
        assert_eq!(provision.lang, "go");
        // Unknown fields are tolerated (client forwards extras).
        let extra: super::CallersReq = serde_json::from_str(
            r#"{"project":".","target":"a","lang":"rust","timeout_secs":30,"scan":true}"#,
        )
        .unwrap_or_default();
        assert_eq!(extra.timeout_secs, Some(30));
        // ttl_secs is REQUIRED: the model must pick explicitly.
        let acquire: Result<super::AcquireReq, _> =
            serde_json::from_str(r#"{"project":".","lang":"rust"}"#);
        assert!(acquire.is_err());
        let acquire: super::AcquireReq =
            serde_json::from_str(r#"{"project":".","ttl_secs":1800}"#).unwrap_or_default();
        assert_eq!(acquire.ttl_secs, 1800);
        assert!(acquire.lang.is_none());
        let release: super::ReleaseReq = serde_json::from_str(r#"{}"#).unwrap_or_default();
        assert!(release.project.is_none());
        assert!(release.lang.is_none());
    }

    #[test]
    fn holding_keys_scope_by_workspace_and_lang() {
        assert_eq!(
            super::holding_key("/w/proj", "rust"),
            "/w/proj\0rust".to_owned()
        );
        assert!(super::holding_key("/w/proj", "rust") != super::holding_key("/w/proj", "go"));
        assert!(super::holding_key("/w/a", "go") != super::holding_key("/w/b", "go"));
    }

    #[test]
    fn expiry_boundaries_hold() {
        assert!(super::holding_expired_at(
            std::time::Instant::now(),
            std::time::Duration::ZERO
        ));
        assert!(!super::holding_expired_at(
            std::time::Instant::now(),
            std::time::Duration::from_secs(3600)
        ));
    }

    #[test]
    fn fmt_mb_scales() {
        assert_eq!(super::fmt_mb(0), "0.0");
        assert_eq!(super::fmt_mb(1_048_576), "1.0");
        assert_eq!(super::fmt_mb(412_500_000), "393.4");
    }

    #[test]
    fn memory_helpers_behave() {
        // System memory exists on every supported platform.
        let system = super::system_memory();
        assert!(system.is_some());
        if let Some((total, _)) = system {
            assert!(total > 0);
        }
        // No pid, no reading.
        assert!(super::server_memory(None).is_none());
    }

    #[test]
    fn empty_state_releases_nothing() {
        let state = std::sync::Arc::new(super::State {
            holdings: std::sync::Mutex::new(std::collections::HashMap::new()),
            bin: "deepfunc-cli".to_owned(),
        });
        let report = super::release_inner(&state, None, None);
        assert!(report.contains("nothing held"));
        let swept = super::sweep_expired(&state);
        assert!(swept.is_empty());
        assert!(super::holdings_line(&state).contains("(0)"));
    }
}
