// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::host::Host;

pub const VERSION: &str = "0.21.0";
pub(crate) const RELEASE_COMMIT: &str = "34c0d647a9f8679c0ce53b76e174894b2c5bfa7a";
pub(crate) const UNIKRAFT_COMMIT: &str = "3b7cba8816c02a497e7b5aa9c901655784a01000";
const RELEASE: &str =
    "https://github.com/hyperlight-dev/hyperlight-unikraft/releases/download/v0.21.0";
const KERNEL: &str = "https://raw.githubusercontent.com/hyperlight-dev/hyperlight-unikraft/34c0d647a9f8679c0ce53b76e174894b2c5bfa7a/kernel/elfloader_hyperlight-x86_64";
const KERNEL_HASH: &str = "fb05f5f9150b9c48de9c035b94eee0fa4c580bd5332586b48f0cee2e860cf861";

struct Binary {
    target: &'static str,
    archive_hash: &'static str,
    binary_hash: &'static str,
}

const LINUX_AMD64: Binary = Binary {
    target: "x86_64-unknown-linux-musl",
    archive_hash: "4b0536a948a9c380cbc7c47e1b04d28c66b2a3d5f25292ae1d90311b181ee722",
    binary_hash: "7ae62c08f862fc56dfc6eb470e0c17b54de0109723efa2b736192832193ffbea",
};

pub(crate) fn linux_binary_hash() -> &'static str {
    LINUX_AMD64.binary_hash
}

fn native() -> Result<Binary> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => LINUX_AMD64,
        ("linux", "aarch64") => Binary {
            target: "aarch64-unknown-linux-musl",
            archive_hash: "ded91cf0f02fea63e07e7e963fa1954fb732fb41d1753bd617c723cba077dd4b",
            binary_hash: "518108446a100aa7e1fa0d158bfc877a67f4eeb5c9dffa3ccbd0dc909826be91",
        },
        ("macos", "aarch64") => Binary {
            target: "aarch64-apple-darwin",
            archive_hash: "9077eda69f85a01e9c437b4cd1a757139b8aa1858f8d71db4e54964c6bd56caf",
            binary_hash: "d04b8fc453639551d2ed6094a6e008a719c1985e27b87bb1b9898c9fb46176d7",
        },
        ("windows", "x86_64") => Binary {
            target: "x86_64-pc-windows-msvc",
            archive_hash: "974c52a81a0d47e0c71170cd0188608d211b2eb5939696a117b1d5af98710b58",
            binary_hash: "d1d6d7de154a8a6d10a12ba4f87c5553f4fe3a059690b50dec72d21ec6a5970e",
        },
        _ => bail!("Hyperlight {VERSION} is not published for this host platform"),
    })
}

pub fn native_path(root: &Path) -> PathBuf {
    root.join("build")
        .join(if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            "hluk"
        } else if cfg!(windows) {
            "hluk-native.exe"
        } else {
            "hluk-native"
        })
}

pub fn kubectl(host: &Host) -> Result<PathBuf> {
    static CACHED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(path) = CACHED.get() {
        return Ok(path.clone());
    }
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "automatic kubectl installation is available on Linux/amd64; install kubectl on this host"
    );
    let directory =
        crate::workspace::Directory::private(crate::platform::home_dir()?.join("tools"))?;
    let path = directory.root().join("kubectl-v1.35.8");
    let checksum = "874d5e72dbb819f43cff16bcd1e4f8bac5b7f2361fe1e55049b0a6c676fb0cbf";
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.root().join(".kubectl.lock"))?;
    lock.try_lock()
        .context("another command is preparing the Kubernetes client; retry when it finishes")?;
    if !matches(&path, checksum)? {
        host.step("Preparing the verified Kubernetes client", || {
            let asset = download(
                "https://dl.k8s.io/release/v1.35.8/bin/linux/amd64/kubectl",
                directory.root(),
                checksum,
                64 << 20,
                Duration::from_secs(120),
                host,
            )?;
            install(asset.reopen()?, &path, checksum)
        })?;
    }
    let _ = CACHED.set(path.clone());
    Ok(path)
}

pub fn setup(root: &Path, host: &Host) -> Result<()> {
    std::fs::create_dir_all(root.join("build"))?;
    install_binary(&LINUX_AMD64, &root.join("build/hluk"), host)?;
    let native = native()?;
    if native_path(root) != root.join("build/hluk") {
        install_binary(&native, &native_path(root), host)?;
    }
    let kernel = root.join("build/kernel");
    if !matches(&kernel, KERNEL_HASH)? {
        let file = download(
            KERNEL,
            root.join("build").as_path(),
            KERNEL_HASH,
            128 << 20,
            Duration::from_secs(120),
            host,
        )?;
        install(file.reopen()?, &kernel, KERNEL_HASH)?;
    }
    Ok(())
}

/// Prepare the pinned base and return its Docker Buildx context assignment.
pub fn runtime_base(root: &Path, runtime: &str, platform: &str, host: &Host) -> Result<String> {
    let (driver, digest) = match runtime {
        "node" | "nodejs" => (
            "node",
            "2ffd019f4e5e7b09361fac0160a87c0304d8349ec6b85c0e0a13a984843400fd",
        ),
        "python" => (
            "python",
            "eab50db06a0dfa4b6c4e58cc84d174b0efe6666b1bad346fbd9dbab61ed31bae",
        ),
        "go" => (
            "go",
            "67152c73fa2cdf770d21aca22c60278ff48253b6248d769174efc3c05251c3e4",
        ),
        "rust" => (
            "rust",
            "3db5960b4b1c0991eca6999133741fb877fdd48e261e54ac888d51bfabdef3a5",
        ),
        "dotnet" | "dotnet-aot" => (
            "dotnet-aot",
            "1172a17b1b09397c41b4ae1ba68a18e50b1cf3a96afad07206f2ddea4fae9a18",
        ),
        _ => bail!("unsupported runtime {runtime:?}"),
    };
    let image = format!("ghcr.io/hyperlight-dev/hyperlight-unikraft/{driver}@sha256:{digest}");
    let mut inspect = host.command("docker")?;
    inspect.current_dir(root).args([
        "image",
        "inspect",
        "--format",
        "{{.Os}}/{{.Architecture}}",
        &image,
    ]);
    let output = host.output(&mut inspect)?;
    let missing = output.exit != 0
        && (output.stderr.contains("No such image") || output.stderr.contains("No such object"));
    if !missing {
        host.require_success(&inspect, &output)?;
    }
    if missing || output.stdout.trim() != platform {
        host.run(host.command("docker")?.current_dir(root).args([
            "pull",
            "--platform",
            platform,
            &image,
        ]))?;
    }
    Ok(format!(
        "hluk-{driver}-rootfs:latest=docker-image://{image}"
    ))
}

fn install_binary(asset: &Binary, destination: &Path, host: &Host) -> Result<()> {
    if matches(destination, asset.binary_hash)? {
        executable(destination)?;
        return Ok(());
    }
    let directory = destination
        .parent()
        .context("runtime asset directory is missing")?;
    let windows = asset.target.contains("windows");
    let extension = if windows { "zip" } else { "tar.gz" };
    let url = format!("{RELEASE}/hluk-v{VERSION}-{}.{extension}", asset.target);
    let archive = download(
        &url,
        directory,
        asset.archive_hash,
        128 << 20,
        Duration::from_secs(120),
        host,
    )?;
    if windows {
        let mut archive = zip::ZipArchive::new(archive.reopen()?)?;
        let mut entry = archive
            .by_name("hluk.exe")
            .context("release archive has no hluk.exe")?;
        ensure!(
            entry.is_file()
                && entry
                    .unix_mode()
                    .is_none_or(|mode| mode & 0o170000 != 0o120000),
            "release binary must be a regular file"
        );
        install(&mut entry, destination, asset.binary_hash)?;
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(archive.reopen()?));
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry.path()?.as_ref() == Path::new("hluk") && entry.header().entry_type().is_file()
            {
                ensure!(!found, "release archive contains duplicate hluk binaries");
                install(&mut entry, destination, asset.binary_hash)?;
                found = true;
            }
        }
        ensure!(found, "release archive has no hluk binary");
    }
    Ok(())
}

pub(crate) fn download(
    url: &str,
    directory: &Path,
    expected: &str,
    maximum: u64,
    timeout: Duration,
    host: &Host,
) -> Result<tempfile::NamedTempFile> {
    ensure!(!host.cancellation.is_cancelled(), "download cancelled");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_resolve(Some(Duration::from_secs(5)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build()
        .into();
    crate::host::interruptible_http(&agent, host, || -> Result<tempfile::NamedTempFile> {
        let mut response = agent.get(url).call().context("download verified asset")?;
        let mut reader = response.body_mut().as_reader();
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut count = 0_u64;
        loop {
            ensure!(!host.cancellation.is_cancelled(), "download cancelled");
            let size = reader.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            count += size as u64;
            ensure!(count <= maximum, "asset download exceeds {maximum} bytes");
            digest.update(&buffer[..size]);
            file.write_all(&buffer[..size])?;
        }
        ensure!(
            hex(digest.finalize().as_slice()) == expected,
            "asset download checksum mismatch"
        );
        file.as_file().sync_all()?;
        Ok(file)
    })?
}

fn matches(path: &Path, expected: &str) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("verify cached runtime asset"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "runtime asset must be a regular file"
    );
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > 64 << 20 {
            return Ok(false);
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex(digest.finalize().as_slice()) == expected)
}

fn install(mut source: impl Read, destination: &Path, expected: &str) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(
        destination
            .parent()
            .context("asset has no parent directory")?,
    )?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= 64 << 20, "runtime asset exceeds 64 MiB");
        digest.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
    }
    ensure!(
        hex(digest.finalize().as_slice()) == expected,
        "runtime asset checksum mismatch"
    );
    file.as_file().sync_all()?;
    executable(file.path())?;
    file.persist(destination)
        .map_err(|error| error.error)
        .context("install verified runtime asset")?;
    Ok(())
}

fn executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn verified_download_obeys_the_callers_size_limit() {
        for maximum in [6, 7] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/asset", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let mut reader = BufReader::new(listener.accept().unwrap().0);
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                reader
                    .get_mut()
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\npayload")
                    .unwrap();
            });
            let directory = tempfile::tempdir().unwrap();
            let host = Host {
                verbose: false,
                cancellation: Default::default(),
            };
            let result = download(
                &url,
                directory.path(),
                &hex(Sha256::digest(b"payload").as_slice()),
                maximum,
                Duration::from_secs(5),
                &host,
            );
            server.join().unwrap();
            if maximum == 7 {
                assert_eq!(std::fs::read(result.unwrap().path()).unwrap(), b"payload");
            } else {
                assert!(result.unwrap_err().to_string().contains("exceeds"));
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            }
        }
    }
}
