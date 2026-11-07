// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use flate2::read::GzDecoder;

use crate::host::Host;

pub struct Options<'a> {
    pub version: &'a str,
    pub url: Option<&'a str>,
    pub check: bool,
    pub remove: bool,
}

pub fn home_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LAMPYR_HOME").filter(|value| !value.is_empty()) {
        return resolve_installation_path(Path::new(&path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("home directory is unavailable; set LAMPYR_HOME")?;
    resolve_installation_path(&PathBuf::from(home).join(".lampyr"))
}

fn resolve_installation_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let leaf = absolute
        .file_name()
        .context("installation path has no final component")?;
    let parent = absolute
        .parent()
        .context("installation path has no parent")?;
    let mut resolved = PathBuf::new();
    let mut pending = Vec::new();
    for component in parent.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                resolved.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if pending.pop().is_none() {
                    ensure!(
                        std::fs::metadata(&resolved)
                            .context("inspect installation path parent")?
                            .is_dir(),
                        "installation path parent {} must be a directory",
                        resolved.display()
                    );
                    resolved.pop();
                }
            }
            std::path::Component::Normal(name) if pending.is_empty() => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(_) => {
                        resolved = candidate.canonicalize().with_context(|| {
                            format!(
                                "resolve installation path component {}",
                                candidate.display()
                            )
                        })?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        pending.push(name.to_owned());
                    }
                    Err(error) => return Err(error).context("inspect installation path"),
                }
            }
            std::path::Component::Normal(name) => pending.push(name.to_owned()),
        }
    }

    for component in pending {
        resolved.push(component);
    }
    resolved.push(leaf);
    Ok(resolved)
}

fn version_path(version: &str) -> Result<PathBuf> {
    ensure!(
        !version.is_empty()
            && version.len() <= 64
            && !version.contains("..")
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
        "invalid platform version"
    );
    Ok(home_dir()?.join("platforms").join(version))
}

fn ensure_platform_parent_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "platform parent {} must be a real directory",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        ensure!(
            metadata.uid() == crate::artifact::libc_euid() || metadata.uid() == 0,
            "platform parent {} must be owned by the current user or root",
            path.display()
        );
        if let Some(reason) = crate::permissions::others_can_write(path, &metadata) {
            ensure!(
                metadata.mode() & 0o1000 != 0,
                "platform directory {} must not be writable by other users without the sticky bit: \
                 {reason}; run `chmod go-w {}`",
                path.display(),
                path.display()
            );
        }
    }
    crate::artifact::ensure_stable_platform_ancestors(path)
}

fn create_private_directories(path: &Path) -> Result<()> {
    use std::path::Component;

    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    let mut existing = PathBuf::new();
    for component in normalized.components() {
        existing.push(component.as_os_str());
        if matches!(component, Component::Normal(_)) {
            match std::fs::symlink_metadata(&existing) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "platform parent {} must be a real directory",
                    existing.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    existing.pop();
                    break;
                }
                Err(error) => return Err(error).context("inspect platform parent directory"),
            }
        }
    }
    ensure_platform_parent_directory(&existing)?;

    let mut current = PathBuf::new();
    for component in normalized.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "platform parent {} must be a real directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error).context("create platform parent directory"),
                }
                let metadata = std::fs::symlink_metadata(&current)
                    .context("inspect platform parent directory")?;
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "platform parent {} must be a real directory",
                    current.display()
                );
            }
            Err(error) => return Err(error).context("inspect platform parent directory"),
        }
    }
    ensure_platform_parent_directory(&normalized)
}

pub fn installed_root() -> Result<PathBuf> {
    let version = std::env::var("LAMPYR_PLATFORM_VERSION")
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned());
    let path = version_path(&version)?;
    validate(&path)
        .with_context(|| format!("platform {version} is not installed; run ly setup"))?;
    Ok(path)
}

/// That `path` is a platform ly installed, of any release: enough to remove
/// it, including one from before fiberd-k8s-controller, whose manifests a
/// current ly cannot use.
fn validate_installed(path: &Path) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_dir(),
        "platform directory must not be a symlink"
    );
    ensure!(
        path.join(".lampyr-platform").is_file(),
        "platform is missing .lampyr-platform"
    );
    Ok(())
}

/// That `path` is a platform this ly can run.
fn validate(path: &Path) -> Result<()> {
    validate_installed(path)?;
    ensure!(
        path.join("k8s/fiberd-tenant.yaml").is_file(),
        "platform is missing its Kubernetes manifests"
    );
    Ok(())
}

pub fn setup(options: Options<'_>, host: &Host) -> Result<PathBuf> {
    let destination = version_path(options.version)?;
    let home = home_dir()?;
    crate::workspace::Directory::private(&home)?;
    crate::workspace::Directory::private(
        destination
            .parent()
            .context("platform destination has no parent")?,
    )?;
    if options.check {
        validate(&destination)?;
        return Ok(destination);
    }
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {
            if options.remove {
                validate_installed(&destination)?;
                std::fs::remove_dir_all(&destination)?;
            } else {
                validate(&destination)?;
            }
            return Ok(destination);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !options.remove,
                "platform {} is not installed",
                options.version
            );
        }
        Err(error) => return Err(error).context("inspect installed platform"),
    }
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        value => value,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        value => value,
    };
    let url = options.url.map(str::to_owned).or_else(|| std::env::var("LAMPYR_PLATFORM_URL").ok())
        .unwrap_or_else(|| format!(
            "https://github.com/hyperlight-dev/lampyr/releases/download/v{version}/lampyr-platform-{os}-{arch}-{version}.tar.gz",
            version = options.version
        ));
    let checksum_url =
        std::env::var("LAMPYR_PLATFORM_SHA256_URL").unwrap_or_else(|_| format!("{url}.sha256"));
    let checksum =
        download_text(&checksum_url, 4096, host).context("download platform checksum")?;
    let hash = checksum
        .split_whitespace()
        .next()
        .context("platform checksum is empty")?;
    ensure!(
        crate::image::is_sha256_digest(&format!("sha256:{hash}")),
        "invalid platform SHA-256"
    );
    let parent = destination
        .parent()
        .context("platform destination has no parent")?;
    create_private_directories(parent)?;
    let archive =
        crate::assets::download(&url, parent, hash, 1 << 30, Duration::from_secs(600), host)?;
    let staging = tempfile::Builder::new()
        .prefix(".platform-")
        .tempdir_in(parent)?;
    crate::workspace::Directory::private(staging.path())?;
    extract(archive.reopen()?, staging.path())?;
    validate(staging.path())?;
    ensure!(
        !host.cancellation.is_cancelled(),
        "platform installation cancelled"
    );
    ensure!(
        !destination.try_exists()?,
        "platform destination appeared during installation"
    );
    std::fs::rename(staging.path(), &destination).context("install verified platform payload")?;
    Ok(destination)
}

pub(crate) fn download_text(url: &str, maximum: usize, host: &Host) -> Result<String> {
    ensure!(!host.cancellation.is_cancelled(), "download cancelled");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .timeout_resolve(Some(Duration::from_secs(5)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut failure = None;
    for attempt in 0..3 {
        let result = crate::host::interruptible_http(&agent, host, || -> Result<String> {
            let mut response = agent.get(url).call().context("request text download")?;
            let mut reader = response.body_mut().as_reader();
            let mut bytes = Vec::new();
            let mut buffer = [0; 16 * 1024];
            loop {
                ensure!(!host.cancellation.is_cancelled(), "download cancelled");
                let count = reader.read(&mut buffer).context("read text download")?;
                if count == 0 {
                    break;
                }
                ensure!(
                    count <= maximum.saturating_sub(bytes.len()),
                    "text download exceeds {maximum} bytes"
                );
                bytes.extend_from_slice(&buffer[..count]);
            }
            String::from_utf8(bytes).context("downloaded text is not UTF-8")
        })?;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => failure = Some(error),
        }
        if attempt < 2 {
            let deadline = Instant::now() + Duration::from_millis(100 * (attempt + 1));
            while Instant::now() < deadline {
                ensure!(!host.cancellation.is_cancelled(), "download cancelled");
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
            }
        }
    }
    Err(failure.context("text download failed without an error")?)
}

pub fn extract(source: impl Read, destination: &Path) -> Result<()> {
    let scoped = Dir::open_ambient_dir(destination, ambient_authority())?;
    let mut archive = tar::Archive::new(GzDecoder::new(source));
    let mut total = 0_u64;
    let mut entries = 0_usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entries += 1;
        ensure!(entries <= 100_000, "platform archive has too many entries");
        let path = entry.path()?.into_owned();
        crate::workspace::relative_path(path.to_str().context("platform path must be UTF-8")?)?;
        if entry.header().entry_type().is_dir() {
            scoped.create_dir_all(&path)?;
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "platform archive may contain only regular files and directories"
        );
        let size = entry.size();
        total = total
            .checked_add(size)
            .context("platform archive size overflow")?;
        ensure!(
            size <= 1 << 30 && total <= 4_u64 << 30,
            "platform archive exceeds its size limit"
        );
        if let Some(parent) = path.parent() {
            scoped.create_dir_all(parent)?;
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(entry.header().mode()? & 0o755);
        }
        let mut file = scoped.open_with(&path, &options)?;
        ensure!(
            std::io::copy(&mut entry, &mut file)? == size,
            "platform archive entry is truncated"
        );
        file.flush()?;
    }
    protect_extracted_tree(destination)?;
    Ok(())
}

#[cfg(unix)]
fn protect_extracted_tree(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let root_metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "platform extraction root must be a real directory"
    );
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path)?;
        ensure!(
            !metadata.file_type().is_symlink() && (metadata.is_dir() || metadata.is_file()),
            "platform extraction produced an unsupported filesystem entry"
        );
        if metadata.is_dir() {
            protect_extracted_tree(&entry_path)?;
        }
        let mode = metadata.permissions().mode() & 0o777;
        std::fs::set_permissions(&entry_path, std::fs::Permissions::from_mode(mode & !0o022))?;
    }
    let mode = root_metadata.permissions().mode() & 0o777;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o022))?;
    Ok(())
}

#[cfg(not(unix))]
fn protect_extracted_tree(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn doctor(root: Option<&Path>) -> Result<()> {
    use std::io::IsTerminal;

    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let report = |ready: bool, name: &str, detail: &str| {
        println!(
            "  {} {name:<10} {detail}",
            crate::host::status_label(
                if ready {
                    crate::host::Status::Ok
                } else {
                    crate::host::Status::Fail
                },
                color
            )
        );
    };

    let mut failures = 0;
    println!("Lampyr doctor\n");
    report(true, "ly", env!("CARGO_PKG_VERSION"));
    report(
        true,
        "build",
        &format!(
            "commit {}, {}",
            env!("LAMPYR_BUILD_COMMIT"),
            env!("LAMPYR_BUILD_STATE")
        ),
    );
    if Path::new("lampyr.yaml").try_exists()? {
        match crate::project::Project::load(".") {
            Ok(project) => report(
                true,
                "application",
                &format!("{} ({})", project.application_model(), project.command),
            ),
            Err(error) => {
                report(false, "application", &format!("{error:#}"));
                failures += 1;
            }
        }
    }
    match crate::service::platform_root(Path::new("."), root) {
        Ok(root) => report(true, "platform", &root.display().to_string()),
        Err(error) => {
            report(false, "platform", &format!("{error:#}"));
            failures += 1;
        }
    }
    for tool in ["docker", "kubectl"] {
        match which::which(tool) {
            Ok(path) => report(true, tool, &path.display().to_string()),
            Err(_) => {
                report(false, tool, "not on PATH");
                failures += 1;
            }
        }
    }
    #[cfg(target_os = "linux")]
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        Ok(_) => report(true, "KVM", "/dev/kvm read/write"),
        Err(error) => {
            report(
                false,
                "KVM",
                &format!("cannot open /dev/kvm read/write: {error}"),
            );
            failures += 1;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        report(false, "execution", "not implemented for this platform");
        failures += 1;
    }
    if failures == 0 {
        println!("\nReady.");
    }
    ensure!(failures == 0, "{failures} prerequisite(s) missing");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::time::Instant;

    #[cfg(unix)]
    #[test]
    fn installation_path_resolves_safe_symlinked_ancestors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&root, &alias)?;
        let owned_target = root.join("target");
        std::fs::create_dir(&owned_target)?;
        let owned_alias = root.join("owned-alias");
        std::os::unix::fs::symlink(&owned_target, &owned_alias)?;

        assert_eq!(
            resolve_installation_path(&alias.join("lampyr"))?,
            root.join("lampyr")
        );
        assert_eq!(resolve_installation_path(&owned_alias)?, owned_alias);
        assert!(create_private_directories(&owned_alias.join("platforms")).is_err());
        let nested = root.join("nested/child");
        std::fs::create_dir_all(&nested)?;
        let nested_alias = root.join("nested-alias");
        std::os::unix::fs::symlink(&nested, &nested_alias)?;
        assert_eq!(
            resolve_installation_path(&nested_alias.join("../lampyr"))?,
            root.join("nested/lampyr")
        );
        let file = root.join("not-a-directory");
        std::fs::write(&file, "file")?;
        assert!(resolve_installation_path(&file.join("../lampyr")).is_err());
        Ok(())
    }

    #[test]
    fn text_download_checks_size_utf8_and_http_errors() -> Result<()> {
        for (status, body, maximum, succeeds) in [
            (200, &b"checksum"[..], 8, true),
            (200, &b"checksum"[..], 7, false),
            (200, &b"\xff"[..], 8, false),
            (503, &b"error"[..], 8, false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}", listener.local_addr()?);
            let server = std::thread::spawn(move || -> std::io::Result<()> {
                let (stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                        break;
                    }
                }
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )?;
                reader.get_mut().write_all(body)
            });
            let host = Host {
                verbose: false,
                cancellation: Default::default(),
            };
            let result = download_text(&url, maximum, &host);
            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            server.join().unwrap()?;
        }
        Ok(())
    }

    #[test]
    fn cancellation_interrupts_stalled_text_headers_and_bodies() -> Result<()> {
        for send_headers in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}", listener.local_addr()?);
            let host = Host {
                verbose: false,
                cancellation: Default::default(),
            };
            let cancellation = host.cancellation.clone();
            let server = std::thread::spawn(move || -> std::io::Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut reader = BufReader::new(&mut stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                        break;
                    }
                }
                if send_headers {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")?;
                }
                std::thread::sleep(Duration::from_millis(50));
                cancellation.cancel();
                // Keep the connection stalled until the client actively shuts it down.
                let mut byte = [0];
                while stream.read(&mut byte)? != 0 {}
                Ok(())
            });
            let started = Instant::now();
            let error = download_text(&url, 1024, &host).unwrap_err();
            assert!(error.to_string().contains("cancelled"), "{error:#}");
            assert!(started.elapsed() < Duration::from_secs(1));
            server.join().unwrap()?;
        }
        Ok(())
    }
}
