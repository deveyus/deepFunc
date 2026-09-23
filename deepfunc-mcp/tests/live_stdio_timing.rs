//! Live end-to-end timing through the real MCP stdio transport.
//!
//! Ignored by default: needs `rust-analyzer` on PATH and spawns a real
//! language server (slow, ~1min). Run explicitly:
//!
//! ```bash
//! cargo test -p deepfunc-mcp --test live_stdio_timing -- --ignored --nocapture
//! ```
//!
//! Measures what an agent actually feels: JSON-RPC framing, holdings,
//! and engine walk, timestamped at the pipe. Prints a latency table;
//! asserts every call succeeds.
//!
//! Harness style matches the workspace tests: plumbing returns Option
//! (`?` propagates), content uses assert. No expect/unwrap/panic
//! anywhere (workspace lints deny all three, tests included).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// One JSON-RPC exchange over piped stdio. None when the pipe breaks.
fn exchange(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    body: &str,
) -> Option<(String, Duration)> {
    let started = Instant::now();
    writeln!(stdin, "{body}").ok()?;
    stdin.flush().ok()?;
    let mut line = String::new();
    stdout.read_line(&mut line).ok()?;
    Some((line, started.elapsed()))
}

fn tool_call(id: i64, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-06-18",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    })
    .to_string()
}

/// Fixture workspace: gamma calls beta calls alpha. Small enough that
/// RA loads in seconds, so transport/engine overhead stands out from
/// index load.
fn fixture_workspace() -> Option<(tempfile::TempDir, String)> {
    let dir = tempfile::tempdir().ok()?;
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fix\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .ok()?;
    std::fs::create_dir(dir.path().join("src")).ok()?;
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn alpha() {}\npub fn beta() { alpha(); }\npub fn gamma() { beta(); }\n",
    )
    .ok()?;
    let project = dir.path().to_str()?.to_owned();
    Some((dir, project))
}

fn spawn_server() -> Option<(Child, ChildStdin, BufReader<ChildStdout>)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_deepfunc-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    Some((child, stdin, BufReader::new(stdout)))
}

fn run() -> Option<()> {
    let (_workspace, project) = fixture_workspace()?;
    let (mut server, mut stdin, mut stdout) = spawn_server()?;

    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "timing", "version": "0"},
        },
    })
    .to_string();
    let (_, init_rtt) = exchange(&mut stdin, &mut stdout, &init)?;
    println!("initialize: {:?}", init_rtt);

    let notified = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}";
    writeln!(stdin, "{notified}").ok()?;
    stdin.flush().ok()?;

    let acquire = tool_call(
        2,
        "acquire",
        serde_json::json!({
            "project": project, "lang": "rust",
            "ttl_secs": 1800, "timeout_secs": 300,
        }),
    );
    let (body, acquire_rtt) = exchange(&mut stdin, &mut stdout, &acquire)?;
    println!("acquire (cold load): {:?}", acquire_rtt);
    assert!(body.contains("\"isError\":false"), "acquire failed: {body}");

    for (id, target) in [(3, "fix::beta"), (4, "fix::alpha")] {
        let call = tool_call(
            id,
            "callers",
            serde_json::json!({
                "project": project, "target": target,
                "lang": "rust", "timeout_secs": 120,
            }),
        );
        let (body, rtt) = exchange(&mut stdin, &mut stdout, &call)?;
        println!("callers {target} (warm): {:?}", rtt);
        assert!(body.contains("\"isError\":false"), "callers failed: {body}");
        if target == "fix::beta" {
            assert!(body.contains("gamma"), "depth-1 body missing: {body}");
        }
    }

    let release = tool_call(5, "release", serde_json::json!({"project": project}));
    let (body, release_rtt) = exchange(&mut stdin, &mut stdout, &release)?;
    println!("release: {:?}", release_rtt);
    assert!(body.contains("\"isError\":false"), "release failed: {body}");

    drop(stdin);
    // Exiting stdin lets a healthy server shut down on its own.
    // Force-kill only if wedged (10s grace).
    for _ in 0..100 {
        if server.try_wait().ok()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if server.try_wait().ok()?.is_none() {
        let killed = server.kill().is_ok();
        assert!(killed, "could not kill wedged server");
    }
    Some(())
}

#[test]
#[ignore = "needs rust-analyzer on PATH; spawns live servers"]
fn live_transport_latency_table() {
    assert!(run().is_some(), "harness plumbing failed mid-run");
}
