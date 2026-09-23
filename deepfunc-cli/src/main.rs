//! deepFunc CLI — multi-language caller context over LSP.
//!
//! Thin binary over the `deepfunc_cli` library: arg parsing, output,
//! provisioning commands, process exit. All resolution lives in the lib
//! so the MCP server reuses it.

#![forbid(unsafe_code)]

mod provision;

#[cfg(kani)]
mod harness;

use deepfunc_cli::{build_markdown, find_workspace_root, init_logging, language, spawn_client};
use deepfunc_core::{parse_target, Error};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

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
    tracing::info!(target = target.as_str(), lang = lang.id, "deepfunc run");
    let root = find_workspace_root(&args.project, lang.markers)?;
    let mut client = spawn_client(&root, lang, args.server_bin.as_deref(), args.timeout_secs)?;
    let built = build_markdown(&mut client, &root, &target, lang);
    client.shutdown();
    let (markdown, empty) = built?;
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
    init_logging();
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
    let (langs, version, root) = deepfunc_cli::parse_provision_args(argv)?;
    let mut reports = Vec::new();
    let mut failures = 0;
    for lang in &langs {
        match provision::provision(lang, version.as_deref(), &root) {
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

#[cfg(test)]
mod tests {
    use super::{parse_args, parse_u64};
    use std::path::Path;

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once("deepfunc".to_owned())
            .chain(parts.iter().map(|part| part.to_string()))
            .collect()
    }

    #[test]
    fn parse_args_happy_path() {
        let parsed = parse_args(&argv(&[
            "--project",
            ".",
            "--target",
            "a::b",
            "--lang",
            "go",
            "--timeout",
            "42",
            "--fail-if-empty",
        ]));
        assert!(parsed.is_ok());
        if let Ok(args) = parsed {
            assert_eq!(args.lang, "go");
            assert_eq!(args.timeout_secs, 42);
            assert!(args.fail_if_empty);
            assert!(args.server_bin.is_none());
        }
    }

    #[test]
    fn parse_args_rejects_problems() {
        assert!(parse_args(&argv(&[])).is_err());
        assert!(parse_args(&argv(&["--project", "."])).is_err());
        assert!(parse_args(&argv(&["--target", "x"])).is_err());
        assert!(parse_args(&argv(&["--project"])).is_err());
        assert!(parse_args(&argv(&["--target"])).is_err());
        assert!(parse_args(&argv(&["--out"])).is_err());
        assert!(parse_args(&argv(&["--lang"])).is_err());
        assert!(parse_args(&argv(&["--server-bin"])).is_err());
        assert!(parse_args(&argv(&["--timeout"])).is_err());
        assert!(parse_args(&argv(&["--nope"])).is_err());
        assert!(parse_args(&argv(&[
            "--project",
            ".",
            "--target",
            "x",
            "--timeout",
            "nan"
        ]))
        .is_err());
        assert!(parse_args(&argv(&["--help"])).is_err());
        let with_bin = parse_args(&argv(&[
            "--project",
            ".",
            "--target",
            "x",
            "--server-bin",
            "/bin/srv",
            "--out",
            "o.md",
        ]));
        assert!(with_bin.is_ok());
        if let Ok(with_bin) = with_bin {
            assert_eq!(with_bin.server_bin, Some("/bin/srv".to_owned()));
            assert_eq!(with_bin.out, Some(Path::new("o.md").to_path_buf()));
        }
    }

    #[test]
    fn parse_u64_accepts_digits() {
        assert_eq!(parse_u64("30", "--timeout").unwrap_or_default(), 30);
        assert!(parse_u64("x", "--timeout").is_err());
        assert!(parse_u64("", "--timeout").is_err());
    }
}
