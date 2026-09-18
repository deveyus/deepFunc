//! deepFunc MCP server — one tool (`callers`) over the deepfunc-cli binary.
//!
//! Transport is stdio; spawn from an MCP client (see `~/mcp/deepfunc/run.sh`).
//! The rust-analyzer run is blocking and slow, so each call executes the CLI
//! in `spawn_blocking` under a tool-level timeout. The CLI binary is located
//! via `DEEPFUNC_BIN`, falling back to `deepfunc-cli` on PATH.

#![forbid(unsafe_code)]

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};
use rmcp::schemars;
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone)]
pub struct DeepFuncMcp {
    bin: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallersReq {
    /// Workspace directory (or subdir) containing Cargo.toml.
    pub project: String,
    /// Target function: `path::to::function` or `file.rs:line`.
    pub target: String,
    /// Seconds for the whole run, including rust-analyzer load. Default 180.
    pub timeout_secs: Option<u64>,
    /// Textual fallback when rust-analyzer is unavailable (less accurate).
    pub scan: Option<bool>,
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
        Err(fail(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

#[tool_router(server_handler)]
impl DeepFuncMcp {
    /// Caller context for one Rust function: depth-1 caller bodies plus
    /// depth-2 caller signatures, as one markdown document for LLM context.
    /// Type-accurate via rust-analyzer. SLOW on cold workspaces (~30-60s
    /// for RA load): raise the client MCP timeout (opencode
    /// `experimental.mcp_timeout`) if calls time out.
    #[tool(
        description = "Caller context for a Rust function (depth-1 bodies, depth-2 signatures) as markdown. Params: project (workspace dir), target (path::to::fn or file.rs:line), timeout_secs (default 180), scan (textual fallback). SLOW ~30-60s cold: raise client mcp_timeout."
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
            "--timeout".to_owned(),
            budget.saturating_sub(15).max(30).to_string(),
        ];
        if req.scan.unwrap_or(false) {
            args.push("--scan".to_owned());
        }
        let worker = tokio::task::spawn_blocking(move || run_cli(&bin, &args));
        match tokio::time::timeout(Duration::from_secs(budget), worker).await {
            Ok(joined) => match joined {
                Ok(result) => result.map(Json),
                Err(error) => Err(fail("deepfunc worker failed: ".to_owned() + &error.to_string())),
            },
            Err(_) => Err(fail(
                "deepfunc call timed out after ".to_owned()
                    + &budget.to_string()
                    + "s. Retry with a larger timeout_secs.",
            )),
        }
    }
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
