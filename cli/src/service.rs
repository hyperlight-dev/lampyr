// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::Seek;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::assets;
use crate::host::Host;
use crate::project::Project;

pub fn platform_root(project: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return validate_root(path);
    }
    let current = std::env::current_dir()?;
    let executable = std::env::current_exe()?;
    let executable = executable
        .parent()
        .context("Lampyr executable has no parent directory")?;
    for start in [project, current.as_path(), executable] {
        let start = std::path::absolute(start)?;
        for directory in start.ancestors() {
            if is_root(directory)? {
                return Ok(directory.to_path_buf());
            }
        }
    }
    crate::platform::installed_root()
}

fn validate_root(path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().context("resolve platform root")?;
    ensure!(
        is_root(&path)?,
        "{} is not a Lampyr platform root",
        path.display()
    );
    Ok(path)
}

fn is_root(path: &Path) -> Result<bool> {
    match std::fs::metadata(path.join(".lampyr-platform")) {
        Ok(metadata) if metadata.is_file() => return Ok(true),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect platform root"),
    }
    Ok(path.join("cli/Cargo.toml").is_file()
        && path.join("providers").is_dir()
        && path.join("k8s/fiberd-tenant.yaml").is_file())
}

pub fn prepare_rootfs(
    directory: &Path,
    root: &Path,
    project: &Project,
    platform: &str,
    host: &Host,
) -> Result<PathBuf> {
    let dockerfile = rootfs_dockerfile(directory)?;
    ensure!(
        !matches!(
            project.application,
            Some(crate::project::Application::Standalone { .. })
        ) || dockerfile == "Dockerfile.rootfs",
        "standalone applications must use customer-owned Dockerfile.rootfs without an adapter"
    );
    assets::setup(root, host)?;
    let base = assets::runtime_base(root, &project.runtime, platform, host)?;
    let build = directory.join(".lampyr/build");
    std::fs::create_dir_all(&build)?;
    let temporary = tempfile::Builder::new()
        .prefix(".rootfs-")
        .tempdir_in(&build)?;
    let image_id = temporary.path().join("image.id");
    let mut command = rootfs_build_command(
        docker_builder(directory, host)?,
        project,
        platform,
        &base,
        &image_id,
        dockerfile,
    );
    host.run(&mut command)?;
    let image_id = std::fs::read_to_string(image_id).context("read built rootfs image ID")?;
    let image_id = image_id.trim();
    ensure!(
        crate::image::is_sha256_digest(image_id),
        "Docker returned an invalid rootfs image ID"
    );
    validate_hooks(directory, project, image_id, platform, host)?;
    let destination = build.join("rootfs.cpio");
    export_cpio(directory, image_id, &destination, host)?;
    Ok(destination)
}

fn rootfs_build_command(
    mut command: Command,
    project: &Project,
    platform: &str,
    base: &str,
    image_id: &Path,
    dockerfile: &str,
) -> Command {
    command.args(["--build-context", base]);
    if project.adapter_managed {
        command.arg("--build-arg").arg(format!(
            "LAMPYR_REQUIRE_EVENT={}",
            u8::from(project.protocols.events)
        ));
    }
    command.arg("--iidfile").arg(image_id).args([
        "--platform",
        platform,
        "--load",
        "--tag",
        &format!("lampyr-dev-{}", project.name),
        "--file",
        dockerfile,
        "--target",
        "rootfs",
        ".",
    ]);
    command
}

fn validate_hooks(
    directory: &Path,
    project: &Project,
    image: &str,
    platform: &str,
    host: &Host,
) -> Result<()> {
    if !project.adapter_managed {
        return Ok(());
    }
    let entrypoint = match project.runtime.as_str() {
        "nodejs" => "/usr/bin/node",
        "python" => "/usr/local/bin/python3",
        "go" | "rust" | "dotnet" => &project.command,
        _ => bail!("runtime has no packaged handler check"),
    };
    let mut command = host.command("docker")?;
    command.current_dir(directory).args([
        "run",
        "--rm",
        "--platform",
        platform,
        "--network=none",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--env",
        "PYTHONDONTWRITEBYTECODE=1",
        "--entrypoint",
        entrypoint,
        image,
    ]);
    if matches!(project.runtime.as_str(), "nodejs" | "python") {
        command.arg(&project.command);
    }
    command.arg("--check");
    if project.protocols.events {
        command.arg("--require-event");
    }
    host.run(&mut command)
        .context("validate packaged handler hooks")
}

pub fn rootfs_dockerfile(directory: &Path) -> Result<&'static str> {
    let dockerfile = match std::fs::symlink_metadata(directory.join(".lampyr-adapter")) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir(),
                ".lampyr-adapter must be a real directory"
            );
            ".lampyr-adapter/Dockerfile.rootfs"
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "Dockerfile.rootfs",
        Err(error) => return Err(error).context("inspect service adapter"),
    };
    ensure!(
        directory.join(dockerfile).is_file(),
        "rootfs build file {dockerfile} is missing"
    );
    Ok(dockerfile)
}

fn export_cpio(directory: &Path, image: &str, destination: &Path, host: &Host) -> Result<()> {
    let parent = destination
        .parent()
        .context("service rootfs has no parent directory")?;
    let container = host.capture(host.command("docker")?.current_dir(directory).args([
        "create",
        "--entrypoint=/",
        image,
    ]))?;
    ensure!(
        !container.is_empty(),
        "Docker did not return a rootfs container ID"
    );
    let converted = (|| -> Result<()> {
        let mut archive = tempfile::tempfile_in(parent)?;
        host.file(
            host.command("docker")?
                .current_dir(directory)
                .arg("export")
                .arg(&container),
            archive.try_clone()?,
        )?;
        archive.rewind()?;
        let mut output = tempfile::NamedTempFile::new_in(parent)?;
        hyperlight_unikraft::cpio::from_tar(archive, output.as_file_mut())
            .context("convert service rootfs to CPIO")?;
        output.as_file().sync_all()?;
        output
            .persist(destination)
            .map_err(|error| error.error)
            .context("persist service rootfs")
            .map(|_| ())
    })();
    let cleanup = host.cleanup(
        host.command("docker")?
            .current_dir(directory)
            .args(["rm", &container]),
    );
    match (converted, cleanup) {
        (Err(error), Err(cleanup)) => {
            bail!("{error:#}\nrootfs container cleanup also failed: {cleanup:#}")
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(crate) fn docker_builder(directory: &Path, host: &Host) -> Result<Command> {
    let builder = host.capture(host.command("docker")?.current_dir(directory).args([
        "context",
        "inspect",
        "--format",
        "{{.Name}}",
    ]))?;
    ensure!(
        !builder.is_empty(),
        "current Docker context has no builder name"
    );
    let mut command = host.command("docker")?;
    command
        .current_dir(directory)
        .args(["buildx", "build", "--builder", &builder]);
    if !host.verbose {
        command.arg("--progress=plain");
    }
    Ok(command)
}

pub(crate) fn export_image(
    directory: &Path,
    image: &str,
    rootfs: &Path,
    host: &Host,
) -> Result<()> {
    std::fs::create_dir(rootfs)?;
    let container = host.capture(host.command("docker")?.current_dir(directory).args([
        "create",
        "--entrypoint=/",
        image,
    ]))?;
    ensure!(
        !container.is_empty(),
        "Docker did not return a rootfs container ID"
    );
    let exported = (|| -> Result<()> {
        let mut file =
            tempfile::tempfile_in(rootfs.parent().context("image export has no parent")?)?;
        host.file(
            host.command("docker")?
                .current_dir(directory)
                .arg("export")
                .arg(&container),
            file.try_clone()?,
        )?;
        file.rewind()?;
        let mut archive = tar::Archive::new(file);
        archive.set_preserve_permissions(true);
        archive
            .unpack(rootfs)
            .context("extract service filesystem")?;
        Ok(())
    })();
    let cleanup = host.cleanup(
        host.command("docker")?
            .current_dir(directory)
            .args(["rm", &container]),
    );
    match (exported, cleanup) {
        (Err(error), Err(cleanup)) => {
            bail!("{error:#}\nrootfs container cleanup also failed: {cleanup:#}")
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }
    Ok(())
}

pub fn memory_mib(value: &str) -> Result<u64> {
    let memory = if let Some(number) = value.strip_suffix("Mi") {
        number.parse::<u64>()?
    } else if let Some(number) = value.strip_suffix("Gi") {
        number
            .parse::<u64>()?
            .checked_mul(1024)
            .context("service memory overflows")?
    } else {
        bail!("service memory must use Mi or Gi units")
    };
    ensure!(memory > 0, "service memory must be positive");
    Ok(memory)
}

pub fn architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        _ => bail!("unsupported host architecture"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn prepared_rootfs_builds_keep_their_digest_after_another_build_retags() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let docker = directory.path().join("docker");
        std::fs::write(&docker, include_str!("../tests/fixtures/rootfs-docker.sh")).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (adapter_managed, recipe) in [
            (true, "adapter/Dockerfile.rootfs"),
            (false, "standalone/project/Dockerfile.rootfs"),
        ] {
            let project = Project {
                name: "test".to_owned(),
                adapter_managed,
                ..Project::default()
            };
            let image = format!(
                "ghcr.io/hyperlight-dev/hyperlight-unikraft/node@sha256:{}",
                "12".repeat(32)
            );
            let other_image = image.replace(&"12".repeat(32), &"34".repeat(32));
            let dockerfile = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../templates/services/nodejs")
                .join(recipe);
            for immutable_from in [false, true] {
                let source = std::fs::read_to_string(&dockerfile).unwrap();
                let source = if immutable_from {
                    source.replace("hluk-node-rootfs:latest", &image)
                } else {
                    source
                };
                let prepared_dockerfile = directory.path().join("prepared.Dockerfile");
                std::fs::write(&prepared_dockerfile, &source).unwrap();
                let context_image = if immutable_from { &other_image } else { &image };
                let base = format!("hluk-node-rootfs:latest=docker-image://{context_image}");
                let mut builder = Command::new(&docker);
                builder.args(["buildx", "build", "--builder", "fixture"]);
                let prepared = rootfs_build_command(
                    builder,
                    &project,
                    "linux/amd64",
                    &base,
                    &directory.path().join("image.id"),
                    prepared_dockerfile.to_str().unwrap(),
                );
                let arguments: Vec<_> = prepared.get_args().map(|arg| arg.to_os_string()).collect();
                let state = directory.path().join("rootfs-tag");
                let log = directory.path().join("docker.log");

                // Pause after the build path prepares its recipe and arguments.
                let output = Command::new(&docker)
                    .args(["tag", &other_image, "hluk-node-rootfs:latest"])
                    .env("DOCKER_STATE", &state)
                    .env("DOCKER_LOG", &log)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(std::fs::read_to_string(&state).unwrap(), other_image);

                let output = Command::new(prepared.get_program())
                    .args(arguments)
                    .env("DOCKER_STATE", &state)
                    .env("DOCKER_LOG", &log)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), image);
                assert_eq!(
                    std::fs::read_to_string(prepared_dockerfile).unwrap(),
                    source
                );
            }
        }
    }
}
