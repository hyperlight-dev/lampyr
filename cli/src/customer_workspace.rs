// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const MANIFEST: &str = "lampyr.workspace.yaml";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Workspace {
    api_version: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ingress: Option<Ingress>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ingress {
    #[serde(default)]
    services: Vec<String>,
}

fn parse(source: &[u8]) -> Result<Workspace> {
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_slice(source).context("decode lampyr.workspace.yaml")?;
    ensure!(
        value
            .get("ingress")
            .and_then(|ingress| ingress.get("service"))
            .is_none(),
        "ingress.service was replaced by ingress.services: [SERVICE]"
    );
    let workspace: Workspace =
        serde_yaml_ng::from_slice(source).context("decode lampyr.workspace.yaml")?;
    ensure!(
        workspace.api_version == "lampyr.dev/v1alpha1" && workspace.kind == "Workspace",
        "unsupported Lampyr Workspace manifest"
    );
    Ok(workspace)
}

pub fn exposed_services(directory: &Path) -> Result<Option<BTreeSet<String>>> {
    let directory = directory
        .canonicalize()
        .context("resolve service or workspace directory")?;
    let Some(root) = find(&directory)? else {
        return Ok(None);
    };
    let source = std::fs::read(root.join(MANIFEST)).context("read lampyr.workspace.yaml")?;
    let workspace = parse(&source)?;
    let mut names = BTreeSet::new();
    let mut member = directory == root;
    let members = services(&root)
        .context("read Workspace services; service directories must be under services/")?;
    for path in members {
        member |= path.canonicalize()? == directory;
        let project = crate::project::Project::load(&path)?;
        crate::project::validate_name(&project.name)?;
        ensure!(
            names.insert(project.name.clone()),
            "duplicate service name {}",
            project.name
        );
    }
    ensure!(
        member,
        "service directory is not a member of its enclosing Workspace"
    );
    let mut exposed = BTreeSet::new();
    for name in workspace
        .ingress
        .map_or_else(Vec::new, |ingress| ingress.services)
    {
        ensure!(
            names.contains(&name),
            "exposed service {name:?} is not in this workspace"
        );
        ensure!(
            exposed.insert(name.clone()),
            "duplicate exposed service {name:?}"
        );
    }
    Ok(Some(exposed))
}

pub fn scaffold(directory: &Path) -> Result<PathBuf> {
    let runtime = crate::service::platform_root(Path::new("."), None)?;
    let directory = std::path::absolute(directory)?;
    if directory.try_exists()? {
        ensure!(
            std::fs::read_dir(&directory)?.next().transpose()?.is_none(),
            "new Workspace directory must be empty: {}",
            directory.display()
        );
    } else {
        std::fs::create_dir_all(&directory)?;
    }
    let manifest = Workspace {
        api_version: "lampyr.dev/v1alpha1".into(),
        kind: "Workspace".into(),
        ingress: Some(Ingress {
            services: vec!["app".into()],
        }),
    };
    std::fs::write(
        directory.join(MANIFEST),
        serde_yaml_ng::to_string(&manifest)?,
    )?;
    std::fs::create_dir(directory.join("services"))?;
    std::fs::create_dir(directory.join("infra"))?;
    let mut entries = 0;
    let mut bytes = 0;
    copy_tree(
        &runtime.join("k8s"),
        &directory.join("k8s"),
        0,
        &mut entries,
        &mut bytes,
    )?;
    crate::project::initialize(&directory.join("services/app"), Some("app"), "nodejs")?;
    Ok(directory)
}

pub fn discover(start: &Path) -> Result<PathBuf> {
    find(start)?.context("no Lampyr Workspace found; run `ly scaffold workspace DIRECTORY`")
}

pub fn root(path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().context("resolve Lampyr Workspace")?;
    let root = discover(&path)?;
    ensure!(
        path == root,
        "--workspace must point to the directory containing {MANIFEST}"
    );
    Ok(root)
}

pub fn find(start: &Path) -> Result<Option<PathBuf>> {
    let start = std::path::absolute(start)?;
    for directory in start.ancestors() {
        let path = directory.join(MANIFEST);
        match std::fs::read(&path) {
            Ok(source) => {
                parse(&source)?;
                return Ok(Some(directory.to_path_buf()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read lampyr.workspace.yaml"),
        }
    }
    Ok(None)
}

pub fn services(root: &Path) -> Result<Vec<PathBuf>> {
    let mut services = std::fs::read_dir(root.join("services"))?
        .map(|entry| {
            let entry = entry?;
            Ok((entry.file_type()?, entry.path()))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    services.retain(|(kind, path)| kind.is_dir() && path.join("lampyr.yaml").is_file());
    services.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(services.into_iter().map(|(_, path)| path).collect())
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    ensure!(
        depth <= 8,
        "Kubernetes manifest templates are too deeply nested"
    );
    let source_kind = std::fs::symlink_metadata(source)?.file_type();
    ensure!(
        source_kind.is_dir() && !source_kind.is_symlink(),
        "Lampyr runtime has no Kubernetes manifest templates"
    );
    std::fs::create_dir(destination)?;
    let mut children = std::fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(std::fs::DirEntry::file_name);
    for entry in children {
        *entries += 1;
        ensure!(
            *entries <= 256,
            "Kubernetes manifest templates contain too many entries"
        );
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target, depth + 1, entries, bytes)?;
        } else {
            ensure!(
                kind.is_file(),
                "Kubernetes manifest templates may not contain links"
            );
            *bytes = bytes
                .checked_add(entry.metadata()?.len())
                .context("Kubernetes manifest template size overflow")?;
            ensure!(
                *bytes <= 16 * 1024 * 1024,
                "Kubernetes manifest templates exceed 16 MiB"
            );
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
use std::collections::BTreeSet;
