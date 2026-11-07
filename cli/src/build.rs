// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::host::Host;
use crate::image::{self, Metadata};
use crate::project::{Project, validate_service_name};
use crate::service;

pub struct Options<'a> {
    pub directory: &'a Path,
    pub platform_root: Option<&'a Path>,
    pub output: Option<&'a Path>,
    pub target: &'a str,
    pub context: Option<&'a str>,
}

pub struct Built {
    pub directory: PathBuf,
    pub metadata: Metadata,
    pub metadata_id: String,
    pub image: String,
}

pub fn build(options: Options<'_>, host: &Host) -> Result<Built> {
    let target = image::TARGET;
    ensure!(
        options.target == target,
        "target {:?} is not supported",
        options.target
    );
    if options.context.is_none() {
        ensure!(
            cfg!(all(target_os = "linux", target_arch = "x86_64")),
            "local linux-amd64-kvm snapshots require a Linux amd64 host; use --context for a matching cluster builder"
        );
    }
    let directory = options
        .directory
        .canonicalize()
        .context("resolve service directory")?;
    let project = Project::load(&directory)?;
    println!("application {}", project.application_model());
    crate::adapter::diagnose(&directory, &project)?;
    validate_service_name(&project.name)?;
    service::memory_mib(&project.resources.memory)?;
    let root = service::platform_root(&directory, options.platform_root)?;
    let destination = std::path::absolute(
        options
            .output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| directory.join(".lampyr/artifact").join(target)),
    )?;
    validate_output(&destination, &directory, &root)?;
    let mut ignored = vec![PathBuf::from(".git"), PathBuf::from(".lampyr")];
    if let Ok(relative) = destination.strip_prefix(&directory) {
        ignored.push(relative.to_owned());
    }
    let source = image::directory_digest(&directory, &ignored)?;
    let initrd = host.step("Building the application rootfs", || {
        service::prepare_rootfs(&directory, &root, &project, "linux/amd64", host)
    })?;
    let parent = destination
        .parent()
        .context("artifact output has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".artifact-building-")
        .tempdir_in(parent)?;
    std::fs::write(
        staging.path().join(".lampyr-artifact"),
        format!("{}\n", image::SCHEMA),
    )?;
    let snapshot = staging.path().join("snapshot");
    // The helper's facts let deploy seal a fiberd template. Without a
    // helper the artifact still runs locally, so only deploy refuses it.
    let helper = crate::fiberd_template::helper(&root)?;
    if helper.is_none() {
        // Once per run, so a Workspace build does not repeat it per service.
        static NOTE: std::sync::Once = std::sync::Once::new();
        NOTE.call_once(|| eprintln!("{}", crate::fiberd_template::missing_helper_note(&root)));
    }
    let facts = host.step("Creating the Hyperlight snapshot", || {
        create_snapshot(
            &directory,
            &root,
            &initrd,
            &snapshot,
            &project,
            options.context,
            helper.as_deref(),
            host,
        )
    })?;
    std::fs::copy(root.join("build/kernel"), staging.path().join("kernel"))?;
    let artifacts = BTreeMap::from([
        ("rootfs".to_owned(), image::file_digest(&initrd)?),
        (
            "snapshot".to_owned(),
            image::directory_digest(&snapshot, &[])?,
        ),
        (
            "kernel".to_owned(),
            image::file_digest(&staging.path().join("kernel"))?,
        ),
    ]);
    let vendor = image::snapshot_vendor(&snapshot)?;
    let mut metadata = Metadata::create(project, source, artifacts, vendor)?;
    if let Some(facts) = facts {
        crate::fiberd_template::record_facts(&mut metadata, &facts)?;
    }
    let mut encoded = serde_json::to_vec_pretty(&metadata)?;
    encoded.push(b'\n');
    std::fs::write(staging.path().join("metadata.json"), encoded)?;
    let metadata_id = image::file_digest(&staging.path().join("metadata.json"))?;
    let image = metadata.local_image(staging.path())?;
    host.step("Packaging your local OCI image", || {
        host.run(
            service::docker_builder(&directory, host)?
                .args([
                    "--platform",
                    &format!("linux/{}", metadata.target.arch),
                    "--load",
                    "--tag",
                    &image,
                    "--file",
                ])
                .arg(directory.join("Dockerfile"))
                .arg(staging.path()),
        )
    })?;
    publish(staging.path(), &destination, &directory, &root)?;
    Ok(Built {
        directory: destination,
        metadata,
        metadata_id,
        image,
    })
}

#[allow(clippy::too_many_arguments)]
fn create_snapshot(
    directory: &Path,
    root: &Path,
    initrd: &Path,
    snapshot: &Path,
    project: &Project,
    context: Option<&str>,
    helper: Option<&Path>,
    host: &Host,
) -> Result<Option<String>> {
    let binary = root.join("build/hluk");
    let memory = service::memory_mib(&project.resources.memory)?.to_string();
    if let Some(context) = context {
        ensure!(!context.is_empty(), "snapshot context must not be empty");
        let temporary = tempfile::Builder::new()
            .prefix("snapshot-")
            .rand_bytes(16)
            .tempdir_in(directory.join(".lampyr/build"))?;
        let suffix = temporary
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .context("snapshot staging directory has no name")?;
        let name = format!("lampyr-{suffix}").to_ascii_lowercase();
        let pod = snapshot_pod(&name);
        let resource = temporary.path().join("pod.json");
        std::fs::write(&resource, serde_json::to_vec(&pod)?)?;
        let command = || -> Result<std::process::Command> {
            let mut command = crate::kube::kubectl_command(host, directory)?;
            command.args(["--context", context, "--namespace", "kube-system"]);
            Ok(command)
        };
        host.run(command()?.args(["create", "--filename"]).arg(&resource))?;
        let result = (|| -> Result<Option<String>> {
            host.run(command()?.args([
                "wait",
                "--for=condition=Ready",
                &format!("pod/{name}"),
                "--timeout=180s",
            ]))?;
            let mut inputs = vec![(binary.as_path(), "hluk"), (initrd, "rootfs.cpio")];
            inputs.extend(helper.map(|helper| (helper, "hluk-helper")));
            for (local, remote) in inputs {
                host.run(
                    command()?
                        .arg("cp")
                        .arg(local)
                        .arg(format!("{name}:/work/{remote}")),
                )
                .with_context(|| format!("upload snapshot input {remote} to builder Pod {name}"))?;
            }
            host.run(command()?.args(["exec", &name, "--", "chmod", "+x", "/work/hluk"]))?;
            // The builder Pod is a KVM node of the cluster, so its helper
            // reports the facts a grant Pod's helper will.
            let facts = helper
                .map(|_| {
                    host.run(command()?.args([
                        "exec",
                        &name,
                        "--",
                        "chmod",
                        "+x",
                        "/work/hluk-helper",
                    ]))?;
                    host.capture(command()?.args([
                        "exec",
                        &name,
                        "--",
                        "/work/hluk-helper",
                        "--version",
                    ]))
                })
                .transpose()?;
            let mut save = command()?;
            save.args(["exec", &name, "--", "/work/hluk"]);
            host.run(&mut snapshot_save_command(
                save,
                Path::new("/work/rootfs.cpio"),
                Path::new("/work/snapshot"),
                &memory,
                project.snapshot_warmup.as_deref(),
            ))
            .with_context(|| format!("create Hyperlight snapshot in builder Pod {name}"))?;
            host.run(&mut snapshot_download_command(command()?, &name, snapshot))
                .with_context(|| format!("download Hyperlight snapshot from builder Pod {name}"))?;
            Ok(facts)
        })();
        let cleanup = host.cleanup(command()?.args([
            "delete",
            "pod",
            &name,
            "--ignore-not-found=true",
            "--wait=false",
        ]));
        match (result, cleanup) {
            (Err(error), Err(cleanup)) => {
                bail!("{error:#}\nsnapshot builder cleanup also failed: {cleanup:#}")
            }
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Ok(facts), Ok(())) => Ok(facts),
        }
    } else {
        let mut save = host.command(binary)?;
        save.current_dir(directory);
        host.run(&mut snapshot_save_command(
            save,
            initrd,
            snapshot,
            &memory,
            project.snapshot_warmup.as_deref(),
        ))?;
        helper
            .map(|helper| host.capture(host.command(helper)?.arg("--version")))
            .transpose()
    }
}

fn snapshot_save_command(
    mut command: std::process::Command,
    initrd: &Path,
    snapshot: &Path,
    memory: &str,
    warmup: Option<&str>,
) -> std::process::Command {
    command
        .args(["snapshot", "save", "--initrd"])
        .arg(initrd)
        .args(["--scratch-mb", memory, "--output"])
        .arg(snapshot);
    if let Some(code) = warmup {
        command.args(["--warm-exec", code]);
    }
    command
}

fn snapshot_download_command(
    mut command: std::process::Command,
    pod: &str,
    destination: &Path,
) -> std::process::Command {
    command
        .args(["cp", "--retries=3"])
        .arg(format!("{pod}:/work/snapshot"))
        .arg(destination);
    command
}

fn snapshot_pod(name: &str) -> serde_json::Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {
            "name": name, "namespace": "kube-system",
            "labels": { "app.kubernetes.io/managed-by": "lampyr", "app.kubernetes.io/name": "lampyr-snapshot-builder" }
        },
        "spec": {
            "activeDeadlineSeconds": 3600, "restartPolicy": "Never", "terminationGracePeriodSeconds": 0,
            "nodeSelector": { "kubernetes.io/os": "linux", "kubernetes.io/arch": "amd64" },
            "containers": [{
                "name": "builder", "image": "ubuntu:24.04",
                "command": ["/bin/sh", "-c", "mkdir -p /work && sleep 3600"],
                "securityContext": { "privileged": true },
                "resources": { "requests": { "cpu": "250m", "memory": "1Gi" }, "limits": { "memory": "2Gi" } },
                "volumeMounts": [{ "name": "kvm", "mountPath": "/dev/kvm" }]
            }],
            "volumes": [{ "name": "kvm", "hostPath": { "path": "/dev/kvm", "type": "CharDevice" } }]
        }
    })
}

pub fn validate_output(output: &Path, project: &Path, root: &Path) -> Result<()> {
    let resolved = resolve_output(output)?;
    ensure!(
        resolved.components().count() > 2
            && !project.starts_with(&resolved)
            && !root.starts_with(&resolved),
        "artifact output would replace a project or platform root"
    );
    let metadata = match std::fs::symlink_metadata(output) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect artifact output"),
    };
    ensure!(
        metadata.is_dir() && !metadata.is_symlink(),
        "artifact output must be a real directory"
    );
    let marker = output.join(".lampyr-artifact");
    let marker_metadata = std::fs::symlink_metadata(&marker)
        .context("refusing to replace an unowned artifact output directory")?;
    ensure!(
        marker_metadata.is_file() && marker_metadata.len() < 1024,
        "artifact ownership marker must be a regular file"
    );
    ensure!(
        std::fs::read(marker)
            .context("refusing to replace an unowned artifact output directory")?
            == format!("{}\n", image::SCHEMA).as_bytes(),
        "refusing to replace an unowned artifact output directory"
    );
    for entry in std::fs::read_dir(output)? {
        let name = entry?.file_name();
        ensure!(
            [
                ".lampyr-artifact",
                "metadata.json",
                "kernel",
                "snapshot",
                // Where earlier builds kept the helper's facts, now in
                // metadata.json; replacing the output drops it.
                "fiberd-helper-facts",
            ]
            .iter()
            .any(|allowed| name == *allowed),
            "artifact output contains unrelated file {:?}",
            name
        );
    }
    Ok(())
}

fn resolve_output(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("artifact output has no parent")?;
            let name = path.file_name().context("artifact output has no name")?;
            Ok(resolve_output(parent)?.join(name))
        }
        Err(error) => Err(error).context("resolve artifact output"),
    }
}

fn publish(staging: &Path, destination: &Path, project: &Path, root: &Path) -> Result<()> {
    validate_output(destination, project, root)?;
    if !destination.try_exists()? {
        return std::fs::rename(staging, destination).context("publish artifact");
    }
    let previous = tempfile::Builder::new()
        .prefix(".artifact-previous-")
        .tempdir_in(
            destination
                .parent()
                .context("artifact output has no parent")?,
        )?;
    let backup = previous.path().join("artifact");
    std::fs::rename(destination, &backup)?;
    if let Err(error) = std::fs::rename(staging, destination) {
        if let Err(rollback) = std::fs::rename(&backup, destination) {
            let preserved = previous.keep();
            bail!(
                "publish artifact failed: {error}; restore previous artifact failed: {rollback}; previous artifact preserved at {}",
                preserved.display()
            );
        }
        return Err(error).context("publish artifact");
    }
    previous.close().context("remove previous owned artifact")?;
    Ok(())
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn adapter_warmup_is_forwarded_to_local_and_cluster_snapshot_commands() {
        let directory = tempfile::tempdir().unwrap();
        for runtime in ["nodejs", "python", "go", "rust", "dotnet"] {
            let service = directory.path().join(runtime);
            crate::project::initialize(&service, Some("example"), runtime).unwrap();
            let project = Project::load(&service).unwrap();
            let warmup = project.snapshot_warmup.as_deref();
            assert_eq!(
                warmup,
                (runtime == "nodejs").then_some(
                    "const server = require('fastify')({bodyLimit: 1048576, forceCloseConnections: true, exposeHeadRoutes: false}); server.register(require('@fastify/websocket'), {options: {maxPayload: 1048576, perMessageDeflate: false}}); server.get('/*', {wsHandler: () => {}}, () => ({})); server.route({method: ['HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'], url: '/*', handler: () => ({})}); server.ready().then(() => server.close())"
                )
            );
            for cluster in [false, true] {
                let mut command =
                    std::process::Command::new(if cluster { "kubectl" } else { "hluk" });
                if cluster {
                    command.args([
                        "--context",
                        "test-cluster",
                        "exec",
                        "snapshot-builder",
                        "--",
                        "/work/hluk",
                    ]);
                }
                let command = snapshot_save_command(
                    command,
                    Path::new("rootfs with spaces.cpio"),
                    Path::new("snapshot directory"),
                    "2048",
                    warmup,
                );
                let arguments = command
                    .get_args()
                    .map(|argument| argument.to_str().unwrap())
                    .collect::<Vec<_>>();
                let start = arguments.iter().position(|arg| *arg == "snapshot").unwrap();
                let mut expected = vec![
                    "snapshot",
                    "save",
                    "--initrd",
                    "rootfs with spaces.cpio",
                    "--scratch-mb",
                    "2048",
                    "--output",
                    "snapshot directory",
                ];
                if let Some(code) = warmup {
                    expected.extend(["--warm-exec", code]);
                }
                assert_eq!(&arguments[start..], expected);
            }
        }
    }

    #[test]
    fn snapshot_download_has_bounded_retries_and_preserves_cluster_selection() {
        let mut command = std::process::Command::new("kubectl");
        command.args(["--context", "aks-lampyr", "--namespace", "kube-system"]);
        command.current_dir("service directory");
        let command = snapshot_download_command(
            command,
            "lampyr-snapshot-test",
            Path::new("output directory/snapshot"),
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--context",
                "aks-lampyr",
                "--namespace",
                "kube-system",
                "cp",
                "--retries=3",
                "lampyr-snapshot-test:/work/snapshot",
                "output directory/snapshot",
            ],
        );
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("service directory"))
        );
    }
}
