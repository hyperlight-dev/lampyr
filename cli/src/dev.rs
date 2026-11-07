// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{customer_workspace, host::Host, project::Project, service};

pub fn run(
    directory: &Path,
    root: Option<&Path>,
    port: i32,
    metrics: bool,
    host: &Host,
) -> Result<()> {
    ensure!(
        (1..=65535).contains(&port),
        "port must be between 1 and 65535"
    );
    let directory = directory.canonicalize().context("resolve dev directory")?;
    let workspace = directory.join(customer_workspace::MANIFEST).try_exists()?;
    let directories = if workspace {
        customer_workspace::root(&directory)?;
        customer_workspace::services(&directory)?
    } else {
        vec![directory.clone()]
    };
    ensure!(
        !directories.is_empty(),
        "Workspace contains no Lampyr Services"
    );
    let projects = directories
        .iter()
        .map(Project::load)
        .collect::<Result<Vec<_>>>()?;
    let mut names = BTreeSet::new();
    for (directory, project) in directories.iter().zip(&projects) {
        crate::project::validate_name(&project.name)?;
        ensure!(
            names.insert(project.name.as_str()),
            "duplicate service name {}",
            project.name
        );
        service::memory_mib(&project.resources.memory)?;
        crate::adapter::diagnose(directory, project)?;
    }
    for project in &projects {
        for service in project.bindings.values() {
            ensure!(
                names.contains(service.as_str()),
                "{} binds missing local service {service}; no deployed fallback is allowed",
                project.name
            );
        }
    }
    let mut exposed = customer_workspace::exposed_services(&directory)?.unwrap_or_else(|| {
        projects
            .iter()
            .map(|project| project.name.clone())
            .collect()
    });
    exposed.retain(|name| names.contains(name.as_str()));
    if exposed.is_empty() {
        eprintln!(
            "warning: no services are exposed; add a service to ingress.services in lampyr.workspace.yaml, or call it from an exposed service through a binding"
        );
    }
    let root = service::platform_root(&directory, root)?;
    let binary = crate::assets::native_path(&root);
    let helper = root.join("build/ly-dev");
    ensure!(
        helper.is_file(),
        "local supervisor is missing at {}; install the matching Lampyr platform payload (source builds: go build -o build/ly-dev ./cmd/ly-dev)",
        helper.display()
    );
    let protocol = host
        .capture(host.command(&helper)?.arg("--protocol-version"))
        .context("local supervisor is outdated; install or rebuild matching ly and ly-dev")?;
    ensure!(
        protocol == "3",
        "local supervisor is outdated; install or rebuild matching ly and ly-dev"
    );
    let build = directory.join(".lampyr/dev");
    std::fs::create_dir_all(&build)?;
    let session = tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(build)?;
    let mut services = Vec::new();
    let platform = format!("linux/{}", service::architecture()?);
    for (directory, project) in directories.iter().zip(&projects) {
        let initrd = host.step(&format!("Building {}", project.name), || {
            service::prepare_rootfs(directory, &root, project, &platform, host)
        })?;
        let snapshot = session.path().join(&project.name);
        host.step(
            &format!("Preparing native snapshot for {}", project.name),
            || {
                let mut save = host.command(&binary)?;
                save.current_dir(directory)
                    .args(["snapshot", "save", "--initrd"])
                    .arg(&initrd)
                    .args([
                        "--scratch-mb",
                        &service::memory_mib(&project.resources.memory)?.to_string(),
                    ])
                    .arg("--output")
                    .arg(&snapshot);
                if let Some(warmup) = &project.snapshot_warmup {
                    save.args(["--warm-exec", warmup]);
                }
                host.run(&mut save)
            },
        )?;
        services.push(json!({
            "name": project.name,
            "command": project.command,
            "snapshot": snapshot,
            "execution": project.effective_execution()?,
            "bindings": project.bindings,
        }));
    }
    let plan = session.path().join("plan.json");
    std::fs::write(
        &plan,
        serde_json::to_vec(&json!({
            "binary": binary,
            "listen": format!("127.0.0.1:{port}"),
            "schemaVersion": 3,
            "exposedServices": exposed,
            "metrics": metrics,
            "services": services,
        }))?,
    )?;
    println!("native dev: CPU quotas, signed Grants and durable event delivery are not emulated");
    host.interactive(host.command(helper)?.arg(&plan))
}
