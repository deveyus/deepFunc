//! deepFunc MCP server — one tool (`callers`) over the deepfunc-cli binary.
//!
//! Transport is stdio; spawn from an MCP client (see `~/mcp/deepfunc/run.sh`).
//! The rust-analyzer run is blocking and slow, so each call executes the CLI
//! in `spawn_blocking` under a tool-level timeout. The CLI binary is located
//! via `DEEPFUNC_BIN`, falling back to `deepfunc-cli` on PATH.

#![forbid(unsafe_code)]

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::schemars;
use rmcp::{tool, tool_router, ErrorData};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone)]
pub struct DeepFuncMcp {
    bin: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallersReq {
    /// Workspace directory (or subdir) containing the project marker.
    pub project: String,
    /// Target function: `path.to.fn`, `path::to::fn`, or `file.ext:line`.
    pub target: String,
    /// Language: rust (default), python, go. Typescript is wired but
    /// blocked (E08: its server never answers post-load requests).
    pub lang: Option<String>,
    /// Seconds for the whole run, including server load. Default 180.
    pub timeout_secs: Option<u64>,
}

fn fail(message: String) -> ErrorData {
    ErrorData::internal_error(message, None)
}

fn run_cli(bin: &str, args: &[String]) -> Result<String, ErrorData> {
    let output = match std::process::Command::new(bin).args(args).output() {
        Ok(output) => output,
        Err(error) => {
            return Err(fail(
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
        Err(fail(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}

#[tool_router(server_handler)]
impl DeepFuncMcp {
    /// Caller context for one function in rust/python/go: depth-1 caller
    /// bodies plus depth-2 caller signatures, as one markdown document for
    /// LLM context. Type-accurate via the language's LSP server. SLOW on
    /// cold workspaces (~30-60s for server load): raise the client MCP
    /// timeout (opencode `experimental.mcp_timeout`) if calls time out.
    #[tool(
        description = "Caller context for a function (depth-1 bodies, depth-2 signatures) as markdown. Params: project (workspace dir), target (path.to.fn or file.ext:line), lang (rust|python|go, default rust), timeout_secs (default 180). SLOW ~30-60s cold: raise client mcp_timeout."
    )]
    async fn callers(
        &self,
        Parameters(req): Parameters<CallersReq>,
    ) -> Result<Json<String>, ErrorData> {
        let bin = self.bin.clone();
        let budget = req.timeout_secs.unwrap_or(180).max(30);
        let mut args = vec![
            "--project".to_owned(),
            req.project,
            "--target".to_owned(),
            req.target,
            "--lang".to_owned(),
            req.lang.unwrap_or_else(|| "rust".to_owned()),
            "--timeout".to_owned(),
            budget.saturating_sub(15).max(30).to_string(),
        ];
        let worker = tokio::task::spawn_blocking(move || run_cli(&bin, &args));
        match tokio::time::timeout(Duration::from_secs(budget), worker).await {
            Ok(joined) => match joined {
                Ok(result) => result.map(Json),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Err(fail(
                "deepfunc call timed out after ".to_owned()
                    + &budget.to_string()
                    + "s. Retry with a larger timeout_secs.",
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
    ) -> Result<Json<String>, ErrorData> {
        let bin = self.bin.clone();
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
                Ok(result) => result.map(Json),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Err(fail(
                "provision timed out after 900s. Retry; downloads resume partially (npm/go caches, rerun overwrites).".to_owned(),
            )),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProvisionReq {
    /// Language server to download: rust, python, typescript, go.
    pub lang: String,
    /// Pinned version override (defaults per language, see DESIGN.md).
    pub version: Option<String>,
    /// Servers root override (default ~/.local/share/deepfunc/servers).
    pub dir: Option<String>,
}

#[tokio::main]
async fn main() {
    use rmcp::{transport::stdio, ServiceExt};
    let bin = std::env::var("DEEPFUNC_BIN").unwrap_or_else(|_| "deepfunc-cli".to_owned());
    let service = DeepFuncMcp { bin };
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
