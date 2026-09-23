//! Probe: what does rust-analyzer actually emit on serverStatus?
//!
//! Ignored by default: needs `rust-analyzer` on PATH. Run explicitly:
//!
//! ```bash
//! cargo test -p deepfunc-cli --test ra_notifications -- --ignored --nocapture
//! ```
//!
//! Spawns RA raw over stdio against a 3-function fixture, logs every
//! server->client notification for 30s with timestamps. Answers whether
//! `quiescent` ever arrives (our retry loops wait on it).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Read one LSP frame. None on EOF or malformed headers.
fn read_frame(reader: &mut BufReader<std::process::ChildStdout>) -> Option<String> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

fn send(stdin: &mut std::process::ChildStdin, body: &str) -> Option<()> {
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).ok()?;
    stdin.flush().ok()?;
    Some(())
}

#[test]
#[ignore = "needs rust-analyzer on PATH; observational probe"]
fn log_ra_notifications_for_30s() {
    let dir = tempfile::tempdir().ok();
    assert!(dir.is_some(), "tempdir failed");
    if let Some(dir) = dir {
        let manifest = "[package]\nname = \"fix\"\nversion = \"0.0.0\"\nedition = \"2021\"\n";
        assert!(std::fs::write(dir.path().join("Cargo.toml"), manifest).is_ok());
        assert!(std::fs::create_dir(dir.path().join("src")).is_ok());
        let lib = "pub fn alpha() {}\npub fn beta() { alpha(); }\npub fn gamma() { beta(); }\n";
        assert!(std::fs::write(dir.path().join("src/lib.rs"), lib).is_ok());
        let root_uri = format!("file://{}", dir.path().to_str().unwrap_or("/tmp"));

        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        assert!(child.is_some(), "rust-analyzer not on PATH?");
        if let Some(mut child) = child {
            let mut stdin = child.stdin.take();
            let stdout = child.stdout.take();
            assert!(stdin.is_some() && stdout.is_some(), "no pipes");
            if let (Some(mut stdin), Some(stdout)) = (stdin, stdout) {
                let mut reader = BufReader::new(stdout);
                let started = Instant::now();
                let init = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"processId\":null,\"rootUri\":\"{root_uri}\",\"capabilities\":{{\"experimental\":{{\"serverStatusNotification\":true}}}}}}}}",
                );
                assert!(send(&mut stdin, &init).is_some(), "init send failed");
                // Consume the initialize response.
                assert!(read_frame(&mut reader).is_some(), "no init response");
                let initialized = "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}";
                assert!(send(&mut stdin, initialized).is_some(), "initd failed");
                // Blocking reads would hang on an idle-but-open pipe, so
                // a reader thread forwards frames; the main loop times out.
                let (frames_tx, frames_rx) = std::sync::mpsc::channel::<Option<String>>();
                std::thread::spawn(move || loop {
                    let frame = read_frame(&mut reader);
                    let done = frame.is_none();
                    if frames_tx.send(frame).is_err() || done {
                        break;
                    }
                });
                println!("--- notifications (t=ms method quiescent?) ---");
                while started.elapsed() < std::time::Duration::from_secs(30) {
                    match frames_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(Some(frame)) => {
                            let elapsed_ms = started.elapsed().as_millis();
                            let parsed = serde_json::from_str::<serde_json::Value>(&frame).ok();
                            let method = parsed
                                .as_ref()
                                .and_then(|value| {
                                    value
                                        .get("method")?
                                        .as_str()
                                        .map(std::string::ToString::to_string)
                                })
                                .unwrap_or_else(|| "(response)".to_owned());
                            let quiescent = parsed
                                .as_ref()
                                .and_then(|value| value.get("params")?.get("quiescent")?.as_bool())
                                .map(|quiescent| {
                                    if quiescent {
                                        "quiescent=true"
                                    } else {
                                        "quiescent=false"
                                    }
                                })
                                .unwrap_or("");
                            println!("t={elapsed_ms}ms {method} {quiescent}");
                        }
                        Ok(None) | Err(_) => {
                            let elapsed_ms = started.elapsed().as_millis();
                            println!("t={elapsed_ms}ms <quiet 500ms>");
                        }
                    }
                }
            }
            let _ignored = child.kill();
        }
    }
}
