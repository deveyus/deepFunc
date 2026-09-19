//! `deepfunc provision` — download language servers into a directory.
//!
//! Fail-loud companion to E01: pinned versions, TLS-only sources, smoke
//! test before recording. No silent fallbacks anywhere. Each install
//! writes `servers/<lang>/manifest.json` recording the exact version.

#![forbid(unsafe_code)]

use deepfunc_core::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RA_VERSION: &str = "2026-09-14";
const GOPLS_VERSION: &str = "v0.23.0";
const PYRIGHT_VERSION: &str = "1.1.414";
const TS_VERSION: &str = "6.0.0";

/// Where a provisioned server lives and how to run it.
pub struct ProvisionReport {
    pub language: String,
    pub version: String,
    pub program: String,
}

/// Default servers root: `$XDG_DATA_HOME/deepfunc/servers`, falling back
/// to `~/.local/share/deepfunc/servers`.
pub fn default_servers_dir() -> Result<PathBuf, Error> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("deepfunc/servers"));
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            Ok(PathBuf::from(home).join(".local/share/deepfunc/servers"))
        }
        _ => Err(Error::Io {
            path: "$HOME".to_owned(),
            message: "cannot determine servers dir: set $HOME or $XDG_DATA_HOME, or pass --dir"
                .to_owned(),
        }),
    }
}

fn write_manifest(
    dir: &Path,
    language: &str,
    version: &str,
    program: &str,
    args: &[String],
) -> Result<(), Error> {
    let record = serde_json::json!({
        "language": language,
        "version": version,
        "program": program,
        "args": args,
        "installed_at_unix": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    });
    let text = match serde_json::to_string_pretty(&record) {
        Ok(text) => text,
        Err(error) => {
            return Err(Error::Io {
                path: dir.display().to_string(),
                message: "failed to encode manifest: ".to_owned() + &error.to_string(),
            })
        }
    };
    match std::fs::write(dir.join("manifest.json"), text) {
        Ok(()) => Ok(()),
        Err(error) => Err(Error::Io {
            path: dir.display().to_string(),
            message: "failed to write manifest: ".to_owned() + &error.to_string(),
        }),
    }
}

/// Run a helper tool (go, npm). Missing helper is a loud IO error naming
/// what to install; a failing helper surfaces its stderr tail.
fn run_helper(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    cwd: &Path,
) -> Result<(), Error> {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(cwd);
    command.stdin(Stdio::null());
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return Err(Error::Io {
                path: program.to_owned(),
                message: "helper not found or not executable (".to_owned()
                    + &error.to_string()
                    + "). Install it first, then retry provision.",
            })
        }
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: String = stderr
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<&str>>()
        .join(" | ");
    Err(Error::Io {
        path: program.to_owned(),
        message: "helper failed: ".to_owned() + &tail,
    })
}

fn ensure_dir(path: &Path) -> Result<(), Error> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) => Err(Error::Io {
            path: path.display().to_string(),
            message: "cannot create directory: ".to_owned() + &error.to_string(),
        }),
    }
}

fn download(url: &str) -> Result<Vec<u8>, Error> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(180))
        .build();
    let response = match agent.get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            return Err(Error::Io {
                path: url.to_owned(),
                message: "download failed with HTTP ".to_owned() + &code.to_string(),
            })
        }
        Err(error) => {
            return Err(Error::Io {
                path: url.to_owned(),
                message: "download failed: ".to_owned() + &error.to_string(),
            })
        }
    };
    let mut bytes = Vec::new();
    match response.into_reader().read_to_end(&mut bytes) {
        Ok(_) => Ok(bytes),
        Err(error) => Err(Error::Io {
            path: url.to_owned(),
            message: "failed reading download body: ".to_owned() + &error.to_string(),
        }),
    }
}

fn provision_rust(dir: &Path, version: &str) -> Result<ProvisionReport, Error> {
    if std::env::consts::ARCH != "x86_64" || std::env::consts::OS != "linux" {
        return Err(Error::Io {
            path: "rust-analyzer".to_owned(),
            message: "prebuilt binary only ships for x86_64 linux (this host: ".to_owned()
                + std::env::consts::OS
                + "/"
                + std::env::consts::ARCH
                + "). Install rust-analyzer another way and pass --server-bin.",
        });
    }
    if host_is_nixos() {
        return Err(Error::Io {
            path: "rust-analyzer".to_owned(),
            message: "NixOS cannot run generic-linux binaries (stub-ld): a direct download would not execute. Provision with `nix-shell -p rust-analyzer`, `nix profile install nixpkgs#rust-analyzer`, or run inside a flake devshell providing it, then pass --server-bin.".to_owned(),
        });
    }
    ensure_dir(dir)?;
    let url = "https://github.com/rust-lang/rust-analyzer/releases/download/".to_owned()
        + version
        + "/rust-analyzer-x86_64-unknown-linux-gnu.gz";
    let bytes = download(&url)?;
    let target = dir.join("rust-analyzer");
    // Scope the writer: the fd must close before the smoke test execs
    // the binary, or exec fails with ETXTBSY (Text file busy).
    {
        let file = match std::fs::File::create(&target) {
            Ok(file) => file,
            Err(error) => {
                return Err(Error::Io {
                    path: target.display().to_string(),
                    message: "cannot write binary: ".to_owned() + &error.to_string(),
                })
            }
        };
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut writer = std::io::BufWriter::new(file);
        if let Err(error) = std::io::copy(&mut decoder, &mut writer) {
            return Err(Error::Io {
                path: target.display().to_string(),
                message: "gunzip failed (truncated download?): ".to_owned() + &error.to_string(),
            });
        }
        if let Err(error) = writer.flush() {
            return Err(Error::Io {
                path: target.display().to_string(),
                message: "failed flushing binary: ".to_owned() + &error.to_string(),
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o755);
        if let Err(error) = std::fs::set_permissions(&target, permissions) {
            return Err(Error::Io {
                path: target.display().to_string(),
                message: "chmod +x failed: ".to_owned() + &error.to_string(),
            });
        }
    }
    smoke_test(target.to_str().unwrap_or("rust-analyzer"), &["--version"])?;
    write_manifest(dir, "rust", version, target.to_str().unwrap_or(""), &[])?;
    Ok(ProvisionReport {
        language: "rust".to_owned(),
        version: version.to_owned(),
        program: target.display().to_string(),
    })
}

/// Existence + executable-bit check for installer-provided binaries
/// (npm and `go install` verify payload integrity themselves; some of
/// these servers have no version flag and would hang serving on stdin).
fn assert_executable(path: &Path, what: &str) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => Ok(()),
            _ => Err(Error::Io {
                path: path.display().to_string(),
                message: what.to_owned(),
            }),
        }
    }
    #[cfg(not(unix))]
    {
        if path.is_file() {
            Ok(())
        } else {
            Err(Error::Io {
                path: path.display().to_string(),
                message: what.to_owned(),
            })
        }
    }
}
/// True when /etc/os-release identifies NixOS. Best-effort: unreadable
/// means "not NixOS" (provision proceeds; smoke test catches the rest).
fn host_is_nixos() -> bool {
    match std::fs::read_to_string("/etc/os-release") {
        Ok(text) => text.lines().any(|line| line.trim() == "ID=nixos"),
        Err(_) => false,
    }
}

/// Smoke test: the binary must answer a version flag. Used only for
/// direct downloads (rust-analyzer), where no installer verified
/// integrity.
fn smoke_test(program: &str, args: &[&str]) -> Result<(), Error> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return Err(Error::Io {
                path: program.to_owned(),
                message: "installed binary failed its smoke test (".to_owned()
                    + &error.to_string()
                    + ").",
            })
        }
    };
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: String = stderr.lines().take(3).collect::<Vec<&str>>().join(" | ");
    Err(Error::Io {
        path: program.to_owned(),
        message: "installed binary failed its smoke test (exit ".to_owned()
            + &output.status.to_string()
            + "): "
            + &tail,
    })
}

fn provision_go(dir: &Path, version: &str) -> Result<ProvisionReport, Error> {
    ensure_dir(dir)?;
    let gobin = dir.join("bin");
    ensure_dir(&gobin)?;
    let module = "golang.org/x/tools/gopls@".to_owned() + version;
    run_helper(
        "go",
        &["install", &module],
        &[("GOBIN", gobin.to_str().unwrap_or("."))],
        dir,
    )?;
    let binary = gobin.join("gopls");
    assert_executable(
        &binary,
        "`go install` succeeded but produced no executable binary. Check `go version` and GOPROXY reachability.",
    )?;
    write_manifest(dir, "go", version, binary.to_str().unwrap_or(""), &[])?;
    Ok(ProvisionReport {
        language: "go".to_owned(),
        version: version.to_owned(),
        program: binary.display().to_string(),
    })
}

fn provision_npm(
    dir: &Path,
    language: &str,
    package: &str,
    version: &str,
    bin_name: &str,
    extra_args: &[String],
) -> Result<ProvisionReport, Error> {
    ensure_dir(dir)?;
    let spec = package.to_owned() + "@" + version;
    run_helper(
        "npm",
        &[
            "install",
            "--prefix",
            dir.to_str().unwrap_or("."),
            &spec,
            "--no-audit",
            "--no-fund",
            "--loglevel=error",
        ],
        &[],
        dir,
    )?;
    // No version-flag smoke here: these servers serve (or error) on
    // stdio instead of answering --version. npm verified payload
    // integrity; the executable bit is the remaining check.
    let binary = dir.join("node_modules/.bin").join(bin_name);
    assert_executable(
        &binary,
        "`npm install` succeeded but produced no executable binary. Check `node --version` and npm registry reachability.",
    )?;
    write_manifest(
        dir,
        language,
        version,
        binary.to_str().unwrap_or(""),
        extra_args,
    )?;
    Ok(ProvisionReport {
        language: language.to_owned(),
        version: version.to_owned(),
        program: binary.display().to_string(),
    })
}

/// Provision one language server. `version_override` replaces the pinned
/// default; `dir_override` replaces the servers root.
pub fn provision(
    lang_id: &str,
    version_override: Option<&str>,
    dir_override: Option<&Path>,
    servers_root: &Path,
) -> Result<ProvisionReport, Error> {
    let base = match dir_override {
        Some(dir) => dir.to_path_buf(),
        None => servers_root.to_path_buf(),
    };
    let dir = base.join(lang_id);
    match lang_id {
        "rust" => provision_rust(&dir, version_override.unwrap_or(RA_VERSION)),
        "go" => provision_go(&dir, version_override.unwrap_or(GOPLS_VERSION)),
        "python" => provision_npm(
            &dir,
            "python",
            "pyright",
            version_override.unwrap_or(PYRIGHT_VERSION),
            "pyright-langserver",
            &["--stdio".to_owned()],
        ),
        "typescript" => provision_npm(
            &dir,
            "typescript",
            "typescript-language-server",
            version_override.unwrap_or(TS_VERSION),
            "typescript-language-server",
            &["--stdio".to_owned()],
        ),
        _ => Err(Error::BadTarget {
            received: "unknown --lang `".to_owned()
                + lang_id
                + "`. Supported: rust, python, typescript, go.",
        }),
    }
}
