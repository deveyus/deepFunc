//! deepFunc MCP server — `callers` + `provision` over the deepfunc-cli binary.
//!
//! Transport is stdio; spawn from an MCP client (see `~/mcp/deepfunc/run.sh`).
//! Language-server runs are blocking and slow, so each call executes the CLI
//! in `spawn_blocking` under a tool-level timeout. The CLI binary is located
//! via `DEEPFUNC_BIN`, falling back to `deepfunc-cli` on PATH.
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
use std::time::Duration;

#[derive(Clone)]
pub struct DeepFuncMcp {
    bin: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
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
    ) -> Result<CallToolResult, ErrorData> {
        let bin = self.bin.clone();
        let budget = req.timeout_secs.unwrap_or(180).max(30);
        let args = vec![
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
                Ok(outcome) => into_tool_result(outcome),
                Err(error) => Err(fail(
                    "deepfunc worker failed: ".to_owned() + &error.to_string(),
                )),
            },
            Err(_) => Ok(CallToolResult::error(vec![ContentBlock::text(
                "deepfunc call timed out after ".to_owned()
                    + &budget.to_string()
                    + "s. Retry with a larger timeout_secs.",
            )])),
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

#[cfg(test)]
mod tests {
    use super::{into_tool_result, run_cli, CliOutcome};
    use rmcp::model::CallToolResult;

    fn is_error(result: &CallToolResult) -> bool {
        result.is_error.unwrap_or(false)
    }

    #[test]
    fn run_cli_reports_all_three_outcomes() {
        // Success: stdout passes through (/bin/sh always exists, even on
        // NixOS where /bin holds nothing else).
        let ok = run_cli("/bin/sh", &["-c".to_owned(), "echo hi".to_owned()]);
        assert!(ok.is_ok());
        if let Ok(text) = ok {
            assert!(text.contains("hi"));
        }
        // Tool error: non-zero exit surfaces stderr.
        let failed = run_cli(
            "/bin/sh",
            &["-c".to_owned(), "echo oops >&2; exit 3".to_owned()],
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
    }
}
