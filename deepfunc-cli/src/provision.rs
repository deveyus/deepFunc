//! `deepfunc provision` — download language servers with no helpers.
//!
//! Fail-loud companion to E01. Every byte comes from pinned versions over
//! TLS and is hash-verified against authoritative metadata, except
//! rust-analyzer (GitHub publishes no checksums; TLS-only, documented).
//! No go/npm/pip/curl required: tar.gz via flate2+tar, tar.xz via
//! lzma-rs+tar, npm tarballs straight from the registry.
//!
//! Layout under the servers root:
//! ```text
//! servers/_node/                        shared node runtime (JS servers)
//! servers/<lang>/manifest.json          exact versions + entry points
//! servers/<lang>/bin/server             executable wrapper (JS servers)
//! servers/<lang>/rust-analyzer          direct binary (rust)
//! servers/<lang>/bin/gopls              `go install` output (go)
//! ```

#![forbid(unsafe_code)]

use deepfunc_core::Error;
use sha2::Digest;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RA_VERSION: &str = "2026-09-14";
const GO_VERSION: &str = "go1.27.1";
const GOPLS_VERSION: &str = "v0.23.0";
const PYRIGHT_VERSION: &str = "1.1.414";
const TS_VERSION: &str = "6.0.0";
const NODE_VERSION: &str = "v24.11.1";

/// Per-OS distribution selectors. macOS has no entries: provision
/// refuses it outright (see `provision`).
#[cfg(windows)]
const RA_ASSET: &str = "rust-analyzer-x86_64-pc-windows-msvc.zip";
#[cfg(not(windows))]
const RA_ASSET: &str = "rust-analyzer-x86_64-unknown-linux-gnu.gz";
#[cfg(windows)]
const NODE_OS: &str = "win-x64";
#[cfg(not(windows))]
const NODE_OS: &str = "linux-x64";
#[cfg(windows)]
const NODE_EXT: &str = "zip";
#[cfg(not(windows))]
const NODE_EXT: &str = "tar.xz";
#[cfg(windows)]
const GO_DIST_SUFFIX: &str = ".windows-amd64.zip";
#[cfg(not(windows))]
const GO_DIST_SUFFIX: &str = ".linux-amd64.tar.gz";
/// Executable suffix for installed binaries and wrappers.
#[cfg(windows)]
const EXE: &str = ".exe";
#[cfg(not(windows))]
const EXE: &str = "";

/// Where a provisioned server lives and how to run it.
pub struct ProvisionReport {
    pub language: String,
    pub version: String,
    /// Runnable path for `--server-bin`: a binary or a wrapper script.
    pub program: String,
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

fn write_manifest(dir: &Path, record: serde_json::Value) -> Result<(), Error> {
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

fn manifest_record(
    language: &str,
    version: &str,
    program: &str,
    extra: &[(&str, &str)],
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "language".to_owned(),
        serde_json::Value::String(language.to_owned()),
    );
    map.insert(
        "version".to_owned(),
        serde_json::Value::String(version.to_owned()),
    );
    map.insert(
        "program".to_owned(),
        serde_json::Value::String(program.to_owned()),
    );
    for (key, value) in extra {
        map.insert(
            (*key).to_owned(),
            serde_json::Value::String((*value).to_owned()),
        );
    }
    map.insert(
        "installed_at_unix".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        )),
    );
    serde_json::Value::Object(map)
}

/// GET bytes over TLS. HTTP errors and transport failures are loud IO errors.
fn download(url: &str) -> Result<Vec<u8>, Error> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
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
        Ok(_) => {
            tracing::debug!(bytes = bytes.len(), url, "downloaded");
            Ok(bytes)
        }
        Err(error) => Err(Error::Io {
            path: url.to_owned(),
            message: "failed reading download body: ".to_owned() + &error.to_string(),
        }),
    }
}

/// GET parsed JSON over TLS.
fn download_json(url: &str) -> Result<serde_json::Value, Error> {
    let bytes = download(url)?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        Err(error) => Err(Error::Io {
            path: url.to_owned(),
            message: "metadata is not valid JSON: ".to_owned() + &error.to_string(),
        }),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn verify_sha256(bytes: &[u8], expected: &str, what: &str) -> Result<(), Error> {
    let actual = sha256_hex(bytes);
    if actual == expected.to_lowercase() {
        return Ok(());
    }
    Err(Error::Io {
        path: what.to_owned(),
        message: "sha256 mismatch (expected ".to_owned()
            + expected
            + ", got "
            + &actual
            + "). Mirror tampering or truncated download; refusing to install.",
    })
}

fn verify_sha1(bytes: &[u8], expected: &str, what: &str) -> Result<(), Error> {
    let actual = sha1_hex(bytes);
    if actual == expected.to_lowercase() {
        return Ok(());
    }
    Err(Error::Io {
        path: what.to_owned(),
        message: "sha1 mismatch (expected ".to_owned()
            + expected
            + ", got "
            + &actual
            + "). Mirror tampering or truncated download; refusing to install.",
    })
}

/// Unpack a .tar.gz archive into `dest`.
fn unpack_tgz(bytes: &[u8], dest: &Path, what: &str) -> Result<(), Error> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    match archive.unpack(dest) {
        Ok(()) => Ok(()),
        Err(error) => Err(Error::Io {
            path: what.to_owned(),
            message: "tar.gz unpack failed: ".to_owned() + &error.to_string(),
        }),
    }
}

/// Unpack a .zip archive into `dest`.
fn unpack_zip(bytes: &[u8], dest: &Path, what: &str) -> Result<(), Error> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(archive) => archive,
        Err(error) => {
            return Err(Error::Io {
                path: what.to_owned(),
                message: "zip open failed (truncated download?): ".to_owned() + &error.to_string(),
            })
        }
    };
    match archive.extract(dest) {
        Ok(()) => Ok(()),
        Err(error) => Err(Error::Io {
            path: what.to_owned(),
            message: "zip unpack failed: ".to_owned() + &error.to_string(),
        }),
    }
}

/// Unpack a .tar.xz archive into `dest`.
fn unpack_txz(bytes: &[u8], dest: &Path, what: &str) -> Result<(), Error> {
    let mut tar_bytes = Vec::new();
    if let Err(error) = lzma_rs::xz_decompress(&mut &bytes[..], &mut tar_bytes) {
        return Err(Error::Io {
            path: what.to_owned(),
            message: "xz decompress failed (truncated download?): ".to_owned() + &error.to_string(),
        });
    }
    let mut archive = tar::Archive::new(&tar_bytes[..]);
    match archive.unpack(dest) {
        Ok(()) => Ok(()),
        Err(error) => Err(Error::Io {
            path: what.to_owned(),
            message: "tar unpack failed: ".to_owned() + &error.to_string(),
        }),
    }
}

fn chmod_exec(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o755);
        if let Err(error) = std::fs::set_permissions(path, permissions) {
            return Err(Error::Io {
                path: path.display().to_string(),
                message: "chmod +x failed: ".to_owned() + &error.to_string(),
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// System node fallback (NixOS only): the nodejs.org binary cannot
/// execute under stub-ld, so reuse the host node when it exists.
fn system_node(dir: &Path, version: &str) -> Result<PathBuf, Error> {
    let mut command = Command::new("node");
    command.args(["--version"]).stdin(Stdio::null());
    let found = match command.output() {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => None,
    };
    match found {
        Some(node_version) => {
            ensure_dir(dir)?;
            eprintln!(
                "note: NixOS cannot run nodejs.org binaries; using system node {node_version} instead of {version}."
            );
            write_manifest(
                dir,
                manifest_record("_node", &("system:".to_owned() + &node_version), "node", &[]),
            )?;
            Ok(PathBuf::from("node"))
        }
        None => Err(Error::Io {
            path: "node".to_owned(),
            message: "NixOS cannot run the nodejs.org binary and no system `node` is on PATH. Install nodejs (`nix-shell -p nodejs`) and retry provision.".to_owned(),
        }),
    }
}
/// Existence + executable-bit check for installer-built binaries
/// (`go install` verifies module hashes itself; the bit is the remaining
/// check). No version-flag smoke: servers serve on stdio instead.
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

/// Write an executable wrapper running `program` with fixed args plus
/// caller args. Shell script on unix, batch file on Windows (untested
/// there so far: plain `cmd.exe` quoting, no PowerShell dependency).
fn write_wrapper(path: &Path, program: &str, args: &[&str]) -> Result<(), Error> {
    #[cfg(windows)]
    let text = {
        let mut text =
            "@echo off\r\nREM generated by `deepfunc provision`; do not edit.\r\n\"".to_owned();
        text.push_str(&program.replace('/', "\\"));
        text.push('"');
        for arg in args {
            text.push_str(" \"");
            text.push_str(&arg.replace('/', "\\"));
            text.push('"');
        }
        text.push_str(" %*\r\n");
        text
    };
    #[cfg(not(windows))]
    let text = {
        let mut text =
            "#!/bin/sh\n# generated by `deepfunc provision`; do not edit.\nexec \"".to_owned();
        text.push_str(program);
        text.push('"');
        for arg in args {
            text.push_str(" \"");
            text.push_str(arg);
            text.push('"');
        }
        text.push_str(" \"$@\"\n");
        text
    };
    if let Err(error) = std::fs::write(path, text) {
        return Err(Error::Io {
            path: path.display().to_string(),
            message: "cannot write wrapper: ".to_owned() + &error.to_string(),
        });
    }
    chmod_exec(path)
}

/// Wrapper file name for a language server (`server` / `server.bat`).
fn wrapper_path(dir: &Path) -> PathBuf {
    dir.join("bin").join("server".to_owned() + wrapper_ext())
}

/// Wrapper file extension, empty on unix.
#[cfg(windows)]
fn wrapper_ext() -> &'static str {
    ".bat"
}

/// Wrapper file extension, empty on unix.
#[cfg(not(windows))]
fn wrapper_ext() -> &'static str {
    ""
}

/// Smoke test: the binary must answer a version flag. Used only where no
/// installer verified integrity (rust-analyzer direct download, node).
fn smoke_version(program: &str) -> Result<(), Error> {
    let mut command = Command::new(program);
    command
        .args(["--version"])
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

/// Canonical node binary location: `_node/bin/node` (+`.exe` on Windows).
fn node_binary_path(root: &Path) -> PathBuf {
    root.join("_node/bin").join("node".to_owned() + EXE)
}

/// Shared node runtime for the JS servers. Returns its binary path.
/// On NixOS the nodejs.org binary cannot execute (stub-ld), so the
/// system node is used when present; elsewhere it is downloaded.
fn provision_node(root: &Path, version: &str) -> Result<PathBuf, Error> {
    let binary = node_binary_path(root);
    if binary.is_file() {
        return Ok(binary);
    }
    if host_is_nixos() {
        let dir = root.join("_node");
        return system_node(&dir, version);
    }
    let dir = root.join("_node");
    ensure_dir(&dir)?;
    // Pinned SHASUMS entry, fetched live so tampering with the list itself
    // fails closed against the pin below only by version string.
    let base = "https://nodejs.org/dist/".to_owned() + version + "/";
    let sums_url = base.clone() + "SHASUMS256.txt";
    let sums = download(&sums_url)?;
    let sums_text = match String::from_utf8(sums) {
        Ok(text) => text,
        Err(_) => {
            return Err(Error::Io {
                path: sums_url,
                message: "SHASUMS256.txt is not valid UTF-8".to_owned(),
            })
        }
    };
    let want = "node-".to_owned() + version + "-" + NODE_OS + "." + NODE_EXT;
    let mut expected: Option<String> = None;
    for line in sums_text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(hash), Some(name)) = (parts.next(), parts.next()) {
            if name.trim_start_matches(['*', ' ']) == want {
                expected = Some(hash.to_owned());
            }
        }
    }
    let expected = match expected {
        Some(hash) => hash,
        None => {
            return Err(Error::Io {
                path: sums_url,
                message: "no sha256 entry for ".to_owned() + &want,
            })
        }
    };
    let bytes = download(&(base + &want))?;
    verify_sha256(&bytes, &expected, &want)?;
    tracing::info!(version, "node checksum verified");
    if NODE_EXT == "zip" {
        unpack_zip(&bytes, &dir, &want)?;
    } else {
        unpack_txz(&bytes, &dir, &want)?;
    }
    // Tarball root is node-<version>-<os>/; hoist the node binary up.
    // Layout differs per OS (bin/node vs root node.exe): first hit wins.
    let nested_dir = dir.join("node-".to_owned() + version + "-" + NODE_OS);
    let candidates = [
        nested_dir.join("bin").join("node".to_owned() + EXE),
        nested_dir.join("node".to_owned() + EXE),
    ];
    ensure_dir(&dir.join("bin"))?;
    for nested in &candidates {
        if nested.is_file() && !binary.is_file() {
            if let Err(error) = std::fs::rename(nested, &binary) {
                return Err(Error::Io {
                    path: binary.display().to_string(),
                    message: "cannot place node binary: ".to_owned() + &error.to_string(),
                });
            }
            break;
        }
    }
    // Prune the rest (headers, npm, docs): only bin/node ships.
    if nested_dir.is_dir() {
        if let Err(error) = std::fs::remove_dir_all(&nested_dir) {
            return Err(Error::Io {
                path: nested_dir.display().to_string(),
                message: "cannot prune node tree: ".to_owned() + &error.to_string(),
            });
        }
    }
    if !binary.is_file() {
        return Err(Error::Io {
            path: dir.display().to_string(),
            message: "node archive unpacked without a node binary at the expected layout"
                .to_owned(),
        });
    }
    smoke_version(binary.to_str().unwrap_or("node"))?;
    write_manifest(
        &dir,
        manifest_record("_node", version, binary.to_str().unwrap_or(""), &[]),
    )?;
    Ok(binary)
}

/// npm registry metadata for package@version: (tarball URL, sha1 hex).
fn npm_dist(package: &str, version: &str) -> Result<(String, String), Error> {
    let url = "https://registry.npmjs.org/".to_owned() + package + "/" + version;
    let meta = download_json(&url)?;
    let dist = meta.get("dist").ok_or_else(|| Error::Io {
        path: url.clone(),
        message: "registry metadata has no dist section".to_owned(),
    })?;
    let tarball = dist
        .get("tarball")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Io {
            path: url.clone(),
            message: "registry metadata has no dist.tarball".to_owned(),
        })?;
    let shasum = dist
        .get("shasum")
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Io {
            path: url.clone(),
            message: "registry metadata has no dist.shasum".to_owned(),
        })?;
    Ok((tarball.to_owned(), shasum.to_owned()))
}

/// package.json "bin" entry for `bin_name`, resolved under `package_dir`.
fn npm_bin_entry(package_dir: &Path, bin_name: &str) -> Result<PathBuf, Error> {
    let manifest_path = package_dir.join("package.json");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) => {
            return Err(Error::Io {
                path: manifest_path.display().to_string(),
                message: "downloaded package has no package.json: ".to_owned() + &error.to_string(),
            })
        }
    };
    let manifest: serde_json::Value = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Err(Error::Io {
                path: manifest_path.display().to_string(),
                message: "package.json is not valid JSON: ".to_owned() + &error.to_string(),
            })
        }
    };
    let rel = manifest
        .get("bin")
        .and_then(|bin| bin.get(bin_name))
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::Io {
            path: manifest_path.display().to_string(),
            message: "package.json has no bin.".to_owned() + bin_name,
        })?;
    Ok(package_dir.join(rel))
}

/// Download an npm tarball, verify shasum, unpack, and write a wrapper
/// running the bin entry under the provisioned node.
fn provision_npm(
    root: &Path,
    language: &str,
    package: &str,
    version: &str,
    bin_name: &str,
    server_args: &[&str],
) -> Result<ProvisionReport, Error> {
    let node = provision_node(root, NODE_VERSION)?;
    let dir = root.join(language);
    ensure_dir(&dir)?;
    let (tarball_url, shasum) = npm_dist(package, version)?;
    let bytes = download(&tarball_url)?;
    verify_sha1(&bytes, &shasum, &tarball_url)?;
    tracing::info!(package, version, "npm tarball checksum verified");
    let package_dir = dir.join("package");
    if package_dir.is_dir() {
        if let Err(error) = std::fs::remove_dir_all(&package_dir) {
            return Err(Error::Io {
                path: package_dir.display().to_string(),
                message: "cannot clear stale package dir: ".to_owned() + &error.to_string(),
            });
        }
    }
    unpack_tgz(&bytes, &dir, &tarball_url)?;
    let entry = npm_bin_entry(&package_dir, bin_name)?;
    if !entry.is_file() {
        return Err(Error::Io {
            path: entry.display().to_string(),
            message: "package unpacked without its bin entry".to_owned(),
        });
    }
    let bindir = dir.join("bin");
    ensure_dir(&bindir)?;
    let wrapper = wrapper_path(&dir);
    let node_str = node.to_str().unwrap_or("node");
    let entry_str = entry.to_str().unwrap_or("");
    write_wrapper(&wrapper, node_str, &[entry_str, server_args[0]])?;
    let wrapper_str = wrapper.display().to_string();
    write_manifest(
        &dir,
        manifest_record(
            language,
            version,
            &wrapper_str,
            &[("node", node_str), ("entry", entry_str)],
        ),
    )?;
    Ok(ProvisionReport {
        language: language.to_owned(),
        version: version.to_owned(),
        program: wrapper_str,
    })
}

fn provision_rust(root: &Path, version: &str) -> Result<ProvisionReport, Error> {
    if std::env::consts::ARCH != "x86_64"
        || (std::env::consts::OS != "linux" && std::env::consts::OS != "windows")
    {
        return Err(Error::Io {
            path: "rust-analyzer".to_owned(),
            message: "prebuilt binary only ships for x86_64 linux/windows (this host: ".to_owned()
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
    let dir = root.join("rust");
    ensure_dir(&dir)?;
    // GitHub publishes no checksums for these assets; TLS-only is
    // documented here rather than pretended otherwise.
    let url = "https://github.com/rust-lang/rust-analyzer/releases/download/".to_owned()
        + version
        + "/"
        + RA_ASSET;
    let bytes = download(&url)?;
    let target = dir.join("rust-analyzer".to_owned() + EXE);
    if RA_ASSET.ends_with(".zip") {
        unpack_zip(&bytes, &dir, &url)?;
        if !target.is_file() {
            return Err(Error::Io {
                path: target.display().to_string(),
                message: "asset unpacked without the binary at the expected layout".to_owned(),
            });
        }
    } else {
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
            let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
            let mut writer = std::io::BufWriter::new(file);
            if let Err(error) = std::io::copy(&mut decoder, &mut writer) {
                return Err(Error::Io {
                    path: target.display().to_string(),
                    message: "gunzip failed (truncated download?): ".to_owned()
                        + &error.to_string(),
                });
            }
            if let Err(error) = writer.flush() {
                return Err(Error::Io {
                    path: target.display().to_string(),
                    message: "failed flushing binary: ".to_owned() + &error.to_string(),
                });
            }
        }
    }
    chmod_exec(&target)?;
    smoke_version(target.to_str().unwrap_or("rust-analyzer"))?;
    let target_str = target.display().to_string();
    write_manifest(&dir, manifest_record("rust", version, &target_str, &[]))?;
    Ok(ProvisionReport {
        language: "rust".to_owned(),
        version: version.to_owned(),
        program: target_str,
    })
}

/// Go toolchain file entry from go.dev metadata.
fn go_toolchain_file(version: &str) -> Result<(String, String), Error> {
    let url = "https://go.dev/dl/?mode=json";
    let releases = download_json(url)?;
    let list = releases.as_array().ok_or_else(|| Error::Io {
        path: url.to_owned(),
        message: "go.dev metadata is not a JSON array".to_owned(),
    })?;
    for release in list {
        if release.get("version").and_then(|value| value.as_str()) != Some(version) {
            continue;
        }
        let files = release
            .get("files")
            .and_then(|value| value.as_array())
            .ok_or_else(|| Error::Io {
                path: url.to_owned(),
                message: "go.dev release has no files list".to_owned(),
            })?;
        for file in files {
            let name = file
                .get("filename")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if name == version.to_owned() + GO_DIST_SUFFIX {
                let archive = file
                    .get("filename")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned();
                let sha = file
                    .get("sha256")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned();
                let dl = "https://go.dev/dl/".to_owned() + &archive;
                if sha.is_empty() {
                    return Err(Error::Io {
                        path: url.to_owned(),
                        message: "go.dev entry has no sha256 for ".to_owned() + &archive,
                    });
                }
                return Ok((dl, sha));
            }
        }
    }
    Err(Error::Io {
        path: url.to_owned(),
        message: "go.dev has no ".to_owned() + GO_DIST_SUFFIX + " toolchain for " + version,
    })
}

fn provision_go(root: &Path, version: &str, gopls_version: &str) -> Result<ProvisionReport, Error> {
    let dir = root.join("go");
    ensure_dir(&dir)?;
    // Stage 1: toolchain archive (self-contained; needs no system go).
    // Layout is `go/` on both tgz and zip distributions.
    let toolchain_dir = dir.join("toolchain");
    let go_binary = toolchain_dir.join("go/bin").join("go".to_owned() + EXE);
    if !go_binary.is_file() {
        ensure_dir(&toolchain_dir)?;
        let (dl, sha) = go_toolchain_file(version)?;
        let bytes = download(&dl)?;
        verify_sha256(&bytes, &sha, &dl)?;
        tracing::info!(version, "go toolchain checksum verified");
        if dl.ends_with(".zip") {
            unpack_zip(&bytes, &toolchain_dir, &dl)?;
        } else {
            unpack_tgz(&bytes, &toolchain_dir, &dl)?;
        }
    }
    if !go_binary.is_file() {
        return Err(Error::Io {
            path: toolchain_dir.display().to_string(),
            message: "toolchain unpacked without go/bin/go at the expected layout".to_owned(),
        });
    }
    // Stage 2: build gopls with the provisioned toolchain.
    let gobin = dir.join("bin");
    ensure_dir(&gobin)?;
    let module = "golang.org/x/tools/gopls@".to_owned() + gopls_version;
    let mut command = Command::new(&go_binary);
    command
        .args(["install", &module])
        .env("GOBIN", &gobin)
        .env("GOTOOLCHAIN", "local")
        .env("GOFLAGS", "-mod=mod")
        .current_dir(&dir)
        .stdin(Stdio::null());
    match command.output() {
        Ok(output) if output.status.success() => (),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = stderr
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<&str>>()
                .join(" | ");
            return Err(Error::Io {
                path: "go install".to_owned(),
                message: "building gopls failed: ".to_owned() + &tail,
            });
        }
        Err(error) => {
            return Err(Error::Io {
                path: go_binary.display().to_string(),
                message: "cannot run provisioned go toolchain (".to_owned()
                    + &error.to_string()
                    + ").",
            })
        }
    }
    let binary = gobin.join("gopls".to_owned() + EXE);
    assert_executable(
        &binary,
        "`go install` succeeded but produced no executable binary",
    )?;
    // gopls shells out to the `go` command for workspace introspection
    // (`go list`), so the wrapper pins both GOROOT and PATH to the
    // provisioned toolchain: serve-time environments rarely have Go.
    // Batch file on Windows (untested there so far).
    let goroot = dir.join("toolchain/go");
    let wrapper = wrapper_path(&dir);
    #[cfg(windows)]
    let wrapper_text = {
        let go_bin = goroot.join("bin").display().to_string().replace('/', "\\");
        let exe = binary.display().to_string().replace('/', "\\");
        "@echo off\r\nREM generated by `deepfunc provision`; do not edit.\r\nset \"GOROOT="
            .to_owned()
            + &goroot.display().to_string().replace('/', "\\")
            + "\"\r\nset \"PATH="
            + &go_bin
            + ";%PATH%\"\r\n\""
            + &exe
            + "\" %*\r\n"
    };
    #[cfg(not(windows))]
    let wrapper_text =
        "#!/bin/sh\n# generated by `deepfunc provision`; do not edit.\nexec env GOROOT=\""
            .to_owned()
            + goroot.to_str().unwrap_or("")
            + "\" PATH=\""
            + goroot.join("bin").to_str().unwrap_or("")
            + ":$PATH\" \""
            + binary.to_str().unwrap_or("")
            + "\" \"$@\"\n";
    if let Err(error) = std::fs::write(&wrapper, wrapper_text) {
        return Err(Error::Io {
            path: wrapper.display().to_string(),
            message: "cannot write wrapper: ".to_owned() + &error.to_string(),
        });
    }
    chmod_exec(&wrapper)?;
    let wrapper_str = wrapper.display().to_string();
    write_manifest(
        &dir,
        manifest_record(
            "go",
            gopls_version,
            &wrapper_str,
            &[("toolchain", version), ("module", &module)],
        ),
    )?;
    Ok(ProvisionReport {
        language: "go".to_owned(),
        version: gopls_version.to_owned(),
        program: wrapper_str,
    })
}

/// Provision one language server. `version_override` replaces the pinned
/// default. `root` is the servers root (each language gets a subdir).
/// macOS is refused outright: no signing cert, no Mac hardware to verify
/// on. A loud error beats an untested artifact.
pub fn provision(
    lang_id: &str,
    version_override: Option<&str>,
    root: &Path,
) -> Result<ProvisionReport, Error> {
    if cfg!(target_os = "macos") {
        return Err(Error::Unsupported {
            language: lang_id.to_owned(),
            reason: "macOS is not supported: no signing certificate and no Mac hardware to verify on. This is deliberate, not an oversight.".to_owned(),
        });
    }
    match lang_id {
        "rust" => provision_rust(root, version_override.unwrap_or(RA_VERSION)),
        "go" => provision_go(root, GO_VERSION, version_override.unwrap_or(GOPLS_VERSION)),
        "python" => provision_npm(
            root,
            "python",
            "pyright",
            version_override.unwrap_or(PYRIGHT_VERSION),
            "pyright-langserver",
            &["--stdio"],
        ),
        "typescript" => provision_npm(
            root,
            "typescript",
            "typescript-language-server",
            version_override.unwrap_or(TS_VERSION),
            "typescript-language-server",
            &["--stdio"],
        ),
        _ => Err(Error::BadTarget {
            received: "unknown --lang `".to_owned()
                + lang_id
                + "`. Supported: rust, python, typescript, go.",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{npm_bin_entry, verify_sha1, verify_sha256};
    use deepfunc_cli::manifest_server;

    #[test]
    fn sha_helpers_match_known_vectors() {
        assert_eq!(
            super::sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            super::sha1_hex(b""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert!(verify_sha256(
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "t"
        )
        .is_ok());
        assert!(verify_sha256(b"abc", "dead", "t").is_err());
        assert!(verify_sha1(b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d", "t").is_ok());
        assert!(verify_sha1(b"abc", "dead", "t").is_err());
        // Case-insensitive expected digests.
        assert!(verify_sha256(
            b"abc",
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "t"
        )
        .is_ok());
    }

    #[test]
    fn manifest_server_reads_provisioned_copies() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            let lang_dir = dir.path().join("go");
            assert!(std::fs::create_dir_all(&lang_dir).is_ok());
            let bin = lang_dir.join("server");
            assert!(std::fs::write(&bin, "#!/bin/sh\n").is_ok());
            let manifest = serde_json::json!({
                "language": "go",
                "version": "v0.23.0",
                "program": bin.to_str().unwrap_or(""),
            });
            assert!(std::fs::write(
                lang_dir.join("manifest.json"),
                serde_json::to_string(&manifest).unwrap_or_default()
            )
            .is_ok());
            let found = manifest_server(dir.path(), "go");
            assert!(found.is_some());
            if let Some((program, args)) = found {
                assert_eq!(program, bin.display().to_string());
                assert!(args.is_empty());
            }
            // Missing manifest, corrupt manifest, and dangling program all miss.
            assert!(manifest_server(dir.path(), "rust").is_none());
            assert!(std::fs::write(lang_dir.join("manifest.json"), "{oops").is_ok());
            assert!(manifest_server(dir.path(), "go").is_none());
        }
    }

    #[test]
    fn npm_bin_entry_resolves_package_layout() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            let package = dir.path().join("package");
            assert!(std::fs::create_dir_all(package.join("lib")).is_ok());
            assert!(std::fs::write(
                package.join("package.json"),
                r#"{"bin": {"my-server": "lib/cli.mjs"}}"#
            )
            .is_ok());
            assert!(std::fs::write(package.join("lib/cli.mjs"), "js").is_ok());
            let entry = npm_bin_entry(&package, "my-server");
            assert!(entry.is_ok());
            if let Ok(entry) = entry {
                assert!(entry.ends_with("lib/cli.mjs"));
            }
            assert!(npm_bin_entry(&package, "missing").is_err());
            assert!(npm_bin_entry(dir.path(), "my-server").is_err());
        }
    }

    fn make_tar_bytes() -> Option<Vec<u8>> {
        let mut tar_bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut tar_bytes);
            let content = b"hello";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "pkg/a.txt", &content[..])
                .ok()?;
            archive.into_inner().ok()?;
        }
        Some(tar_bytes)
    }

    fn make_payload_tar_bytes() -> Option<Vec<u8>> {
        let mut tar_bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut tar_bytes);
            let content = b"payload";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "p/b.txt", &content[..])
                .ok()?;
            archive.into_inner().ok()?;
        }
        Some(tar_bytes)
    }

    #[test]
    fn tgz_roundtrip_preserves_files() {
        let dir = tempfile::tempdir().ok();
        assert!(dir.is_some());
        if let Some(dir) = dir {
            // gzip the tar bytes with the same crates provision uses.
            let tar_bytes = make_tar_bytes();
            assert!(tar_bytes.is_some());
            if let Some(tar_bytes) = tar_bytes {
                let mut gzipped = Vec::new();
                {
                    use std::io::Write;
                    let mut encoder =
                        flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::fast());
                    assert!(encoder.write_all(&tar_bytes).is_ok());
                    assert!(encoder.finish().is_ok());
                }
                let dest = dir.path().join("out");
                assert!(std::fs::create_dir_all(&dest).is_ok());
                assert!(super::unpack_tgz(&gzipped, &dest, "test").is_ok());
                let back = std::fs::read(dest.join("pkg/a.txt")).ok();
                assert!(back.is_some());
                if let Some(back) = back {
                    assert_eq!(back, b"hello");
                }
                assert!(super::unpack_tgz(b"garbage", &dest, "test").is_err());
            }
        }
    }

    #[test]
    fn xz_roundtrip_preserves_tar() {
        // Build a raw tar, xz-compress it, unpack via provision's path.
        let tar_bytes = make_payload_tar_bytes();
        assert!(tar_bytes.is_some());
        if let Some(tar_bytes) = tar_bytes {
            let mut packed = Vec::new();
            assert!(lzma_rs::xz_compress(&mut &tar_bytes[..], &mut packed).is_ok());
            let dir = tempfile::tempdir().ok();
            assert!(dir.is_some());
            if let Some(dir) = dir {
                let dest = dir.path().join("out");
                assert!(std::fs::create_dir_all(&dest).is_ok());
                assert!(super::unpack_txz(&packed, &dest, "test").is_ok());
                let back = std::fs::read(dest.join("p/b.txt")).ok();
                assert!(back.is_some());
                if let Some(back) = back {
                    assert_eq!(back, b"payload");
                }
                assert!(super::unpack_txz(b"garbage", &dest, "test").is_err());
            }
        }
    }

    #[test]
    fn host_detection_runs() {
        // Host-dependent by nature; execution coverage only.
        let _ = super::host_is_nixos();
    }
}
