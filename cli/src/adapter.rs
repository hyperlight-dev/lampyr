// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Explicit upgrades of customer-owned adapters. Nothing in this module starts a runtime.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{File, Permissions};
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use git2::{IndexEntry, IndexTime, MergeFileOptions, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::{
    ADAPTER_SCHEMA, AdapterMetadata, PristineMetadata, Project, rendered_template,
};

const DIRECTORY: &str = ".lampyr-adapter";
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_METADATA: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
    bytes: Vec<u8>,
    mode: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    inode: u64,
    device: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    files: BTreeMap<String, Source>,
    directories: BTreeMap<String, u32>,
    stamps: BTreeMap<String, Stamp>,
    root_mode: u32,
    root_id: (u64, u64),
}

struct StagingDir {
    directory: Option<tempfile::TempDir>,
    root: File,
    parent: File,
    name: String,
    expected_id: (u64, u64),
    expected_snapshot: Option<Snapshot>,
}

impl StagingDir {
    fn new(directory: tempfile::TempDir, parent: &File) -> Result<Self> {
        let path = directory.path().to_owned();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            let _retained = directory.keep();
            bail!("invalid staging name; temporary directory retained");
        };
        let path_metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
            Ok(_) => {
                let _retained = directory.keep();
                bail!("staging path is not a real directory; temporary directory retained");
            }
            Err(error) => {
                let _retained = directory.keep();
                return Err(error).context("could not inspect staging directory; retained");
            }
        };
        let root = match open_child(parent, &name, true) {
            Ok(root) => root,
            Err(error) => {
                let path = directory.keep();
                return Err(error).with_context(|| {
                    format!(
                        "could not establish staging ownership; retained in service directory as {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )
                });
            }
        };
        let metadata = match root.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                let path = directory.keep();
                return Err(error).with_context(|| {
                    format!(
                        "could not establish staging identity; retained in service directory as {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )
                });
            }
        };
        if (metadata.dev(), metadata.ino()) != (path_metadata.dev(), path_metadata.ino()) {
            let _retained = directory.keep();
            bail!("staging directory changed while opening; temporary directory retained");
        }
        let parent = match parent.try_clone() {
            Ok(parent) => parent,
            Err(error) => {
                let path = directory.keep();
                return Err(error).with_context(|| {
                    format!(
                        "could not pin staging parent; retained in service directory as {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )
                });
            }
        };
        Ok(Self {
            directory: Some(directory),
            root,
            parent,
            name,
            expected_id: (metadata.dev(), metadata.ino()),
            expected_snapshot: None,
        })
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("staging directory exists")
            .path()
    }

    fn expect(&mut self, snapshot: Snapshot) {
        self.expected_id = snapshot.root_id;
        self.expected_snapshot = Some(snapshot);
    }

    fn matches_named_directory(&self) -> bool {
        open_child(&self.parent, &self.name, true)
            .and_then(|current| {
                let metadata = current.metadata()?;
                ensure!(
                    (metadata.dev(), metadata.ino()) == self.expected_id,
                    "staging directory identity changed"
                );
                if let Some(expected) = &self.expected_snapshot {
                    ensure!(
                        Snapshot::read(&current)? == *expected,
                        "staging directory contents changed"
                    );
                }
                Ok(())
            })
            .is_ok()
    }

    fn retain(&mut self) -> Option<std::path::PathBuf> {
        self.directory.take().map(tempfile::TempDir::keep)
    }

    fn cleanup(&mut self) -> Result<()> {
        let Some(directory) = self.directory.take() else {
            return Ok(());
        };
        if !self.matches_named_directory() {
            let _retained = directory.keep();
            bail!(
                "staging directory ownership changed; retained without cleanup in service directory as {}",
                self.name
            );
        }
        let staging_path = directory.keep();
        let quarantine = tempfile::Builder::new()
            .prefix(".lampyr-cleanup-")
            .tempdir_in(
                staging_path
                    .parent()
                    .context("staging path has no parent")?,
            )
            .with_context(|| {
                format!(
                    "create private cleanup directory; staging data retained in service directory as {}",
                    self.name
                )
            })?;
        let quarantine_path = quarantine.keep();
        let quarantine_name = quarantine_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid cleanup directory name")?
            .to_owned();
        let quarantine_root =
            open_child(&self.parent, &quarantine_name, true).with_context(|| {
                format!(
                    "open cleanup directory; staging data retained as {}",
                    self.name
                )
            })?;
        let quarantine_metadata = std::fs::symlink_metadata(&quarantine_path)
            .context("inspect cleanup directory; staging data retained")?;
        let quarantine_identity = quarantine_root.metadata()?;
        ensure!(
            quarantine_metadata.is_dir()
                && !quarantine_metadata.file_type().is_symlink()
                && (quarantine_metadata.dev(), quarantine_metadata.ino())
                    == (quarantine_identity.dev(), quarantine_identity.ino()),
            "cleanup directory changed while opening; staging data retained"
        );
        #[cfg(test)]
        tests::AFTER_QUARANTINE.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(&quarantine_path);
            }
        });
        let moved = CString::new("owned")?;
        let source = CString::new(self.name.as_str())?;
        // renameat2 is Linux-only; elsewhere the fresh 0700 quarantine has no
        // "owned" entry to replace, so plain renameat is equivalent.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                self.parent.as_raw_fd(),
                source.as_ptr(),
                quarantine_root.as_raw_fd(),
                moved.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let result = unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                source.as_ptr(),
                quarantine_root.as_raw_fd(),
                moved.as_ptr(),
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            // Nothing was moved; don't leave an empty, unexplained quarantine.
            let quarantine = CString::new(quarantine_name.as_str())?;
            unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    quarantine.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            };
            return Err(error).with_context(|| {
                format!(
                    "claim staging directory for identity-checked cleanup; retained in service directory as {}",
                    self.name
                )
            });
        }
        let owned = open_child(&quarantine_root, "owned", true);
        let matches = owned.is_ok_and(|owned| {
            owned.metadata().is_ok_and(|metadata| {
                (metadata.dev(), metadata.ino()) == self.expected_id
                    && self
                        .expected_snapshot
                        .as_ref()
                        .is_none_or(|snapshot| Snapshot::read(&owned).is_ok_and(|s| s == *snapshot))
            })
        });
        if !matches {
            bail!(
                "staging directory changed during cleanup; retained without removal in service directory as {quarantine_name}/owned"
            );
        }
        let quarantine_dir = cap_std::fs::Dir::from_std_file(quarantine_root.try_clone()?);
        quarantine_dir.remove_dir_all("owned").with_context(|| {
            format!(
                "remove staging directory; retained in service directory as {quarantine_name}/owned"
            )
        })?;
        let current = open_child(&self.parent, &quarantine_name, true)
            .context("cleanup directory moved during removal; empty directory retained")?;
        let current_metadata = current.metadata()?;
        ensure!(
            (current_metadata.dev(), current_metadata.ino())
                == (quarantine_identity.dev(), quarantine_identity.ino()),
            "cleanup directory changed during removal; empty directory retained"
        );
        let name = CString::new(quarantine_name.as_str())?;
        let result =
            unsafe { libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("remove empty adapter cleanup directory");
            }
        }
        Ok(())
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("warning: {error:#}");
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pending {
    base_metadata_digest: String,
    target: AdapterMetadata,
    conflicts: Vec<String>,
    /// Digests of the cleanly merged files written with the conflicts (None
    /// for deletions), so finishing can detect a reverted half-upgrade.
    #[serde(default)]
    merged: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Color {
    Auto,
    Always,
    Never,
}

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        crate::image::hex(Sha256::digest(bytes).as_slice())
    )
}

/// Orders template versions semver-style (missing components are zero; a
/// prerelease sorts before its release); None when either is not numeric.
fn newer_than(recorded: &str, installed: &str) -> Option<bool> {
    use std::cmp::Ordering;
    fn parse(version: &str) -> Option<(Vec<u64>, Option<&str>)> {
        let version = version.split('+').next().unwrap_or("");
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let core = core
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        Some((core, pre))
    }
    fn identifiers(left: &str, right: &str) -> Ordering {
        for (a, b) in left.split('.').zip(right.split('.')) {
            let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        left.split('.').count().cmp(&right.split('.').count())
    }
    let (left, left_pre) = parse(recorded)?;
    let (right, right_pre) = parse(installed)?;
    let width = left.len().max(right.len());
    let pad = |core: &[u64]| {
        (0..width)
            .map(|i| core.get(i).copied().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let order = pad(&left)
        .cmp(&pad(&right))
        .then_with(|| match (left_pre, right_pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => identifiers(a, b),
        });
    Some(order == Ordering::Greater)
}

fn matches_template(metadata: &AdapterMetadata, template: &AdapterMetadata) -> bool {
    metadata.template_version == template.template_version
        && metadata.files == template.files
        && base_values(metadata, template).is_none_or(|base| base == template.own_values())
}

/// The template values `metadata` was rendered from. Metadata written before
/// `pristine` existed has no record; when its version and files match the
/// installed template, the installed values are it, otherwise it is unknown.
fn base_values(
    metadata: &AdapterMetadata,
    installed: &AdapterMetadata,
) -> Option<PristineMetadata> {
    match &metadata.pristine {
        Some(pristine) => Some(pristine.clone()),
        // Same version alone is not enough: 0.1.0 content changed on main before
        // the version fixture existed, so the tracked files must match too.
        None if metadata.template_version == installed.template_version
            && metadata.files == installed.files =>
        {
            Some(installed.own_values())
        }
        None => None,
    }
}

/// Three-way merges the customer-editable metadata fields against the target
/// template, so an upgrade keeps edits to `entrypoint` and `snapshotWarmup`.
fn merge_metadata(
    ours: &AdapterMetadata,
    target: &AdapterMetadata,
    installed: &AdapterMetadata,
) -> Result<AdapterMetadata> {
    fn pick<T: PartialEq + Clone + Serialize>(
        field: &str,
        ours: &T,
        base: &T,
        theirs: &T,
    ) -> Result<T> {
        if ours == base || ours == theirs {
            Ok(theirs.clone())
        } else if theirs == base {
            Ok(ours.clone())
        } else {
            // JSON, so the values can be pasted into adapter.json (null = remove).
            let ours = serde_json::to_string(ours)?;
            let theirs = serde_json::to_string(theirs)?;
            bail!(
                "adapter.json {field} was customized ({ours}) and the template changed it to {theirs}; to keep yours, set pristine.{field} to {theirs} (the new template value), or to take the template's, set {field} to {theirs}; then rerun --upgrade"
            )
        }
    }
    let theirs = target.own_values();
    let Some(base) = base_values(ours, installed) else {
        ensure!(
            ours.own_values() == theirs,
            "adapter.json from template {} has no recorded pristine values, so customized entrypoint or snapshotWarmup cannot be merged; set entrypoint to {} and snapshotWarmup to {} (null = remove), rerun --upgrade, then reapply your edits",
            ours.template_version,
            serde_json::to_string(&theirs.entrypoint)?,
            serde_json::to_string(&theirs.snapshot_warmup)?
        );
        let mut merged = target.clone();
        merged.pristine = Some(theirs);
        return Ok(merged);
    };
    let mut merged = target.clone();
    merged.entrypoint = pick(
        "entrypoint",
        &ours.entrypoint,
        &base.entrypoint,
        &theirs.entrypoint,
    )?;
    merged.snapshot_warmup = pick(
        "snapshotWarmup",
        &ours.snapshot_warmup,
        &base.snapshot_warmup,
        &theirs.snapshot_warmup,
    )?;
    merged.pristine = Some(theirs);
    Ok(merged)
}

fn safe_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 240
            && path.split('/').count() <= 16
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "unsafe adapter path {path:?}"
    );
    Ok(())
}

fn editable_path(path: &str) -> Result<()> {
    safe_path(path)?;
    ensure!(
        !matches!(
            path.split('/').next(),
            Some("baseline" | "pending-baseline" | "adapter.json" | "pending.json")
        ),
        "reserved adapter path {path:?}"
    );
    Ok(())
}

// openat with O_NOFOLLOW on every component rejects symlinks, including races
// replacing a checked directory. All subsequent IO stays relative to these fds.
fn open_child(parent: &File, name: &str, directory: bool) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let flags = flags | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open real adapter file/directory");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "adapter entries must be real files or directories"
    );
    ensure!(
        directory || metadata.nlink() == 1,
        "adapter files must not be hard links"
    );
    Ok(file)
}

impl Snapshot {
    fn read(root: &File) -> Result<Self> {
        let mut snapshot = Self {
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
            stamps: BTreeMap::new(),
            root_mode: root.metadata()?.permissions().mode() & 0o777,
            root_id: (root.metadata()?.dev(), root.metadata()?.ino()),
        };
        let mut total = 0;
        snapshot.walk(root, "", &mut total)?;
        Ok(snapshot)
    }

    fn walk(&mut self, root: &File, prefix: &str, total: &mut usize) -> Result<()> {
        let dir = cap_std::fs::Dir::from_std_file(root.try_clone()?);
        for entry in dir.entries()? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("adapter paths must be valid UTF-8"))?;
            let path = format!("{prefix}{name}");
            safe_path(&path)?;
            ensure!(
                self.stamps.len() < MAX_ENTRIES,
                "adapter has too many entries"
            );
            let is_dir = entry.file_type()?.is_dir();
            let mut file = open_child(root, &name, is_dir)
                .with_context(|| format!("read adapter path {path}"))?;
            let metadata = file.metadata()?;
            self.stamps.insert(
                path.clone(),
                Stamp {
                    inode: metadata.ino(),
                    device: metadata.dev(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                    changed: (metadata.ctime(), metadata.ctime_nsec()),
                },
            );
            let mode = metadata.permissions().mode() & 0o777;
            if is_dir {
                self.directories.insert(path.clone(), mode);
                self.walk(&file, &format!("{path}/"), total)?;
            } else {
                ensure!(metadata.len() <= MAX_FILE, "adapter file too large: {path}");
                let mut bytes = Vec::new();
                (&mut file).take(MAX_FILE + 1).read_to_end(&mut bytes)?;
                *total += bytes.len();
                ensure!(
                    bytes.len() as u64 <= MAX_FILE && *total <= MAX_TOTAL,
                    "adapter exceeds size limits"
                );
                self.files.insert(path, Source { bytes, mode });
            }
        }
        Ok(())
    }

    /// Reads only adapter.json, the files it tracks, their baselines and any
    /// pending state. Build/dev/deploy and lifecycle commands use this so local
    /// tooling output inside the adapter (target/, node_modules/, editor locks)
    /// never blocks them; only --upgrade walks the whole tree strictly.
    fn read_tracked(root: &File) -> Result<Self> {
        let mut snapshot = Self {
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
            stamps: BTreeMap::new(),
            root_mode: root.metadata()?.permissions().mode() & 0o777,
            root_id: (root.metadata()?.dev(), root.metadata()?.ino()),
        };
        let mut total = 0;
        snapshot.read_file(root, "adapter.json", true, &mut total)?;
        let metadata = snapshot.metadata()?;
        for path in metadata.files.keys() {
            editable_path(path)?;
            snapshot.read_file(root, path, false, &mut total)?;
            snapshot.read_file(root, &format!("baseline/{path}"), false, &mut total)?;
        }
        if snapshot.read_file(root, "pending.json", false, &mut total)? {
            let pending: Pending = decode(snapshot.bytes("pending.json")?, "pending.json")?;
            for path in pending.target.files.keys() {
                editable_path(path)?;
                snapshot.read_file(root, &format!("pending-baseline/{path}"), false, &mut total)?;
            }
        }
        match open_child(root, "pending-baseline", true) {
            Ok(_) => {
                snapshot.directories.insert("pending-baseline".into(), 0);
            }
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error).context("read adapter path pending-baseline"),
        }
        Ok(snapshot)
    }

    /// Reads one tracked file through O_NOFOLLOW components; false if absent.
    fn read_file(
        &mut self,
        root: &File,
        path: &str,
        required: bool,
        total: &mut usize,
    ) -> Result<bool> {
        safe_path(path)?;
        let opened = (|| {
            let mut directory = root.try_clone()?;
            let mut parts = path.split('/').peekable();
            while let Some(name) = parts.next() {
                if parts.peek().is_none() {
                    return open_child(&directory, name, false);
                }
                directory = open_child(&directory, name, true)?;
            }
            unreachable!("safe_path rejects empty paths")
        })();
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if !required && is_not_found(&error) => return Ok(false),
            Err(error) => return Err(error).with_context(|| format!("read adapter path {path}")),
        };
        let metadata = file.metadata()?;
        ensure!(metadata.len() <= MAX_FILE, "adapter file too large: {path}");
        let mut bytes = Vec::new();
        (&mut file).take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        *total += bytes.len();
        ensure!(
            bytes.len() as u64 <= MAX_FILE && *total <= MAX_TOTAL,
            "adapter exceeds size limits"
        );
        let mode = metadata.permissions().mode() & 0o777;
        self.files.insert(path.to_owned(), Source { bytes, mode });
        Ok(true)
    }

    fn bytes(&self, path: &str) -> Result<&[u8]> {
        self.files
            .get(path)
            .map(|file| file.bytes.as_slice())
            .with_context(|| format!("missing adapter file {path}"))
    }

    fn metadata(&self) -> Result<AdapterMetadata> {
        decode(self.bytes("adapter.json")?, "adapter.json")
    }
}

/// Quotes a path for pasting into a POSIX shell, so printed commands work for
/// directories with spaces or apostrophes.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-+=:@,".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::NotFound)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8], name: &str) -> Result<T> {
    ensure!(
        bytes.len() <= MAX_METADATA,
        "adapter metadata too large: {name}"
    );
    serde_json::from_slice(bytes).with_context(|| format!("decode adapter metadata {name}"))
}

fn validate_metadata(metadata: &AdapterMetadata, runtime: &str) -> Result<()> {
    ensure!(
        metadata.schema_version == ADAPTER_SCHEMA
            && metadata.runtime == runtime
            && matches!(runtime, "nodejs" | "python" | "go" | "rust" | "dotnet")
            && !metadata.template_version.is_empty()
            && metadata.template_version.len() <= 128
            && !metadata.template_version.chars().any(char::is_control)
            && !metadata.template_version.chars().any(char::is_whitespace)
            && metadata.entrypoint.starts_with('/')
            && metadata.entrypoint.len() <= 1024
            && !metadata
                .entrypoint
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            && !metadata.files.is_empty()
            && metadata.files.len() <= MAX_ENTRIES,
        "invalid runtime adapter schema, runtime, version or entrypoint"
    );
    // Shared with Project::load so an upgrade can never finalize metadata that
    // every later command would reject.
    for values in [metadata.own_values(), metadata.pristine_values()] {
        ensure!(
            values.entrypoint.starts_with('/')
                && values.entrypoint.len() <= 1024
                && !values
                    .entrypoint
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()),
            "invalid runtime adapter entrypoint"
        );
        ensure!(
            values
                .snapshot_warmup
                .as_ref()
                .is_none_or(|code| !code.trim().is_empty()),
            "runtime adapter snapshotWarmup must not be empty"
        );
    }
    for (path, hash) in &metadata.files {
        editable_path(path)?;
        ensure!(
            hash.strip_prefix("sha256:").is_some_and(|h| h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
            "invalid pristine digest for {path}"
        );
        ensure!(
            !Path::new(path)
                .ancestors()
                .skip(1)
                .any(|p| metadata.files.contains_key(p.to_str().unwrap_or(""))),
            "adapter file/directory collision at {path}"
        );
    }
    Ok(())
}

fn validate_baseline(snapshot: &Snapshot, metadata: &AdapterMetadata, prefix: &str) -> Result<()> {
    for (path, hash) in &metadata.files {
        ensure!(
            digest(snapshot.bytes(&format!("{prefix}/{path}"))?) == *hash,
            "pristine baseline was modified: {prefix}/{path}"
        );
    }
    for path in snapshot
        .files
        .keys()
        .filter_map(|p| p.strip_prefix(&format!("{prefix}/")))
    {
        ensure!(
            metadata.files.contains_key(path),
            "untracked pristine baseline file: {prefix}/{path}"
        );
    }
    for path in snapshot
        .directories
        .keys()
        .filter_map(|path| path.strip_prefix(&format!("{prefix}/")))
    {
        ensure!(
            metadata
                .files
                .keys()
                .any(|file| file.starts_with(&format!("{path}/"))),
            "untracked pristine baseline directory: {prefix}/{path}"
        );
    }
    Ok(())
}

/// adapter.json changed after `--write-conflicts` saved its digest.
#[derive(Debug)]
struct PendingBaseChanged;

impl std::fmt::Display for PendingBaseChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            ".lampyr-adapter/adapter.json changed while an upgrade is pending; restore it, or cancel the upgrade by deleting .lampyr-adapter/pending.json and .lampyr-adapter/pending-baseline/",
        )
    }
}

impl std::error::Error for PendingBaseChanged {}

fn pending(snapshot: &Snapshot, metadata: &AdapterMetadata) -> Result<Option<Pending>> {
    if !snapshot.files.contains_key("pending.json") {
        ensure!(
            !snapshot
                .files
                .keys()
                .chain(snapshot.directories.keys())
                .any(|p| p == "pending-baseline" || p.starts_with("pending-baseline/"))
                && !snapshot.directories.contains_key("pending.json"),
            "orphaned pending upgrade state"
        );
        return Ok(None);
    }
    let pending: Pending = decode(snapshot.bytes("pending.json")?, "pending.json")?;
    validate_metadata(&pending.target, &metadata.runtime)?;
    validate_baseline(snapshot, &pending.target, "pending-baseline")?;
    ensure!(
        !pending.conflicts.is_empty() && pending.conflicts.len() <= MAX_ENTRIES,
        "pending upgrade does not match adapter metadata"
    );
    ensure!(
        pending.base_metadata_digest == digest(snapshot.bytes("adapter.json")?),
        PendingBaseChanged
    );
    ensure!(
        pending.merged.len() <= MAX_ENTRIES,
        "pending upgrade does not match adapter metadata"
    );
    for path in pending.merged.keys() {
        editable_path(path)?;
    }
    let mut paths = BTreeSet::new();
    for path in &pending.conflicts {
        editable_path(path)?;
        ensure!(paths.insert(path), "duplicate pending conflict path");
        ensure!(
            metadata.files.contains_key(path) || pending.target.files.contains_key(path),
            "unknown pending conflict path {path}"
        );
    }
    Ok(Some(pending))
}

// Ancestors only need search permission, so a service under a parent the user
// can enter but not list (0711 homes, shared hosts) still opens. The service
// directory itself stays O_RDONLY because installs fsync it.
#[cfg(target_os = "linux")]
fn open_ancestor(parent: &File, name: &str) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open real adapter file/directory");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn open_ancestor(parent: &File, name: &str) -> Result<File> {
    open_child(parent, name, true)
}

fn open_service(directory: &Path) -> Result<File> {
    let absolute = std::path::absolute(directory)?;
    let mut names = Vec::new();
    for component in absolute.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => {
                names.push(name.to_str().context("service path must be UTF-8")?);
            }
            _ => bail!("service directory must not contain parent traversal"),
        }
    }
    let mut parent = File::open("/")?;
    for (index, name) in names.iter().enumerate() {
        parent = if index + 1 == names.len() {
            open_child(&parent, name, true)
        } else {
            open_ancestor(&parent, name)
        }
        .context("open real service directory")?;
    }
    Ok(parent)
}

fn open_adapter(directory: &Path) -> Result<(File, File)> {
    let parent = open_service(directory)?;
    let adapter = open_child(&parent, DIRECTORY, true).context("open runtime adapter")?;
    Ok((parent, adapter))
}

fn read_manifest(parent: &File) -> Result<Vec<u8>> {
    let file = open_child(parent, "lampyr.yaml", false).context("read lampyr.yaml")?;
    let mut bytes = Vec::new();
    file.take(MAX_METADATA as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_METADATA,
        "lampyr.yaml exceeds size limit"
    );
    Ok(bytes)
}

/// Validate pristine state without treating editable source differences as corruption.
pub fn metadata(directory: &Path, runtime: &str) -> Result<AdapterMetadata> {
    let (_, adapter) = open_adapter(directory)?;
    let snapshot = Snapshot::read_tracked(&adapter)?;
    let metadata = snapshot.metadata()?;
    validate_metadata(&metadata, runtime)?;
    validate_baseline(&snapshot, &metadata, "baseline")?;
    // Lifecycle commands don't need the pending upgrade, so an edited
    // adapter.json must not lock users out of status, logs, scale, or delete.
    if let Err(error) = pending(&snapshot, &metadata)
        && error.downcast_ref::<PendingBaseChanged>().is_none()
    {
        return Err(error);
    }
    Ok(metadata)
}

/// Build/dev/deploy warnings are diagnostic only and never update tracked state.
pub fn diagnose(directory: &Path, project: &Project) -> Result<()> {
    if !project.adapter_managed {
        return Ok(());
    }
    let (_, adapter) = open_adapter(directory)?;
    let snapshot = Snapshot::read_tracked(&adapter)?;
    let metadata = snapshot.metadata()?;
    validate_metadata(&metadata, &project.runtime)?;
    validate_baseline(&snapshot, &metadata, "baseline")?;
    ensure!(
        pending(&snapshot, &metadata)?.is_none(),
        "runtime adapter upgrade is pending"
    );
    let target = target(&project.runtime, &project.name)?;
    let current = target.metadata()?;
    let modified = metadata.files.iter().any(|(path, hash)| {
        snapshot.files.get(path).is_none_or(|file| {
            digest(&file.bytes) != *hash
                || snapshot
                    .files
                    .get(&format!("baseline/{path}"))
                    .is_some_and(|baseline| baseline.mode != file.mode)
        })
    }) || base_values(&metadata, &current)
        .is_some_and(|base| base != metadata.own_values());
    if modified {
        eprintln!(
            "warning: runtime adapter is modified; customized adapters receive best-effort support"
        );
    }
    if newer_than(&metadata.template_version, &current.template_version) == Some(true) {
        eprintln!(
            "warning: runtime adapter template {} is newer than this ly's {}; upgrade ly; best-effort support",
            metadata.template_version, current.template_version
        );
    } else if metadata.template_version == current.template_version
        && metadata.files != current.files
    {
        // Same version, different template files: either ly may be the newer
        // one, so don't suggest an upgrade that could undo template fixes.
        eprintln!(
            "warning: runtime adapter differs from this ly's template at the same version {}; if this ly is older than the one that scaffolded the service, upgrade ly instead of running --upgrade; best-effort support",
            current.template_version
        );
    } else if !matches_template(&metadata, &current) {
        eprintln!(
            "warning: runtime adapter is outdated or differs from the installed template (recorded {}, installed {}); run ly scaffold service {} --upgrade; best-effort support",
            metadata.template_version,
            current.template_version,
            shell_quote(&directory.to_string_lossy())
        );
    }
    Ok(())
}

fn target(runtime: &str, name: &str) -> Result<Snapshot> {
    let files = rendered_template(runtime, name)?
        .into_iter()
        .filter_map(|(path, bytes)| {
            path.strip_prefix(".lampyr-adapter/")
                .map(|path| (path.to_owned(), Source { bytes, mode: 0o644 }))
        })
        .collect();
    Ok(Snapshot {
        files,
        directories: BTreeMap::new(),
        stamps: BTreeMap::new(),
        root_mode: 0o755,
        root_id: (0, 0),
    })
}

fn has_markers(bytes: &[u8]) -> bool {
    bytes.split(|b| *b == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        matches!(
            line,
            b"<<<<<<< customer" | b"||||||| pristine" | b">>>>>>> installed"
        )
    })
}

fn encode<T: Serialize>(value: &T) -> Result<Source> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_METADATA,
        "adapter metadata exceeds size limit"
    );
    Ok(Source { bytes, mode: 0o644 })
}

fn promote(
    files: &mut BTreeMap<String, Source>,
    metadata: &AdapterMetadata,
    pristine: &Snapshot,
    prefix: &str,
) -> Result<()> {
    let modes: BTreeMap<_, _> = metadata
        .files
        .keys()
        .filter_map(|path| {
            files
                .get(&format!("baseline/{path}"))
                .map(|source| (path.clone(), source.mode))
        })
        .collect();
    files.retain(|path, _| {
        !path.starts_with("baseline/")
            && !path.starts_with("pending-baseline/")
            && path != "pending.json"
    });
    for path in metadata.files.keys() {
        files.insert(
            format!("baseline/{path}"),
            Source {
                bytes: pristine.bytes(&format!("{prefix}/{path}"))?.to_vec(),
                mode: modes
                    .get(path)
                    .copied()
                    .unwrap_or(pristine.files[&format!("{prefix}/{path}")].mode),
            },
        );
    }
    files.insert("adapter.json".into(), encode(metadata)?);
    Ok(())
}

/// Uses libgit2's merge engine, not a line alignment heuristic.
fn merge(
    repo: &Repository,
    path: &str,
    base: Option<&Source>,
    ours: Option<&Source>,
    theirs: Option<&Source>,
) -> Result<(Option<Source>, bool)> {
    fn bytes(source: Option<&Source>) -> Option<&[u8]> {
        source.map(|s| s.bytes.as_slice())
    }
    if bytes(ours) == bytes(base) && (theirs.is_some() || ours == base) {
        return Ok((
            theirs.map(|s| Source {
                bytes: s.bytes.clone(),
                mode: ours.map_or(s.mode, |o| o.mode),
            }),
            false,
        ));
    }
    if bytes(theirs) == bytes(base) || bytes(ours) == bytes(theirs) {
        return Ok((ours.cloned(), false));
    }
    let entry = |source: Option<&Source>| -> Result<IndexEntry> {
        let contents = source.map_or(&[][..], |s| s.bytes.as_slice());
        Ok(IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: contents.len() as u32,
            id: repo.blob(contents)?,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        })
    };
    let mut options = MergeFileOptions::new();
    options
        .ancestor_label("pristine")
        .our_label("customer")
        .their_label("installed")
        .style_diff3(true);
    let result = repo.merge_file_from_index(
        &entry(base)?,
        &entry(ours)?,
        &entry(theirs)?,
        Some(&mut options),
    )?;
    let structural = ours.is_none() || theirs.is_none() || base.is_none();
    let conflict = structural || !result.is_automergeable();
    let contents = if structural && result.is_automergeable() {
        // A deletion versus a modification (or independent additions) requires
        // an explicit choice even when text merging an empty blob would be clean.
        let mut contents = b"<<<<<<< customer\n".to_vec();
        for (source, separator) in [
            (ours, "\n||||||| pristine\n"),
            (base, "\n=======\n"),
            (theirs, "\n>>>>>>> installed\n"),
        ] {
            contents.extend_from_slice(source.map_or(&[], |s| s.bytes.as_slice()));
            contents.extend_from_slice(separator.as_bytes());
        }
        contents
    } else {
        result.content().to_vec()
    };
    ensure!(
        !conflict
            || (std::str::from_utf8(&contents).is_ok()
                && ![base, ours, theirs]
                    .iter()
                    .flatten()
                    .any(|s| s.bytes.contains(&0))),
        "binary adapter conflict at .lampyr-adapter/{path}; resolve the binary change manually before upgrading (nothing written)"
    );
    Ok((
        Some(Source {
            bytes: contents,
            mode: ours.or(theirs).map_or(0o644, |s| s.mode),
        }),
        conflict,
    ))
}

fn preview(path: &str, ours: Option<&Source>, theirs: Option<&Source>, color: Color) -> Result<()> {
    let colored = match color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto => std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    eprintln!("conflict: .lampyr-adapter/{path}");
    let mut patch = git2::Patch::from_buffers(
        ours.map_or(&[], |s| s.bytes.as_slice()),
        Some(Path::new("customer").join(path).as_path()),
        theirs.map_or(&[], |s| s.bytes.as_slice()),
        Some(Path::new("installed").join(path).as_path()),
        None,
    )?;
    let patch = patch.to_buf()?;
    for line in String::from_utf8_lossy(&patch).lines() {
        let code = if line.starts_with('-') {
            Some("31")
        } else if line.starts_with('+') {
            Some("32")
        } else {
            None
        };
        if let Some(code) = code.filter(|_| colored) {
            eprintln!("\x1b[{code}m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
    Ok(())
}

pub fn upgrade(directory: &Path, write_conflicts: bool, color: Color) -> Result<()> {
    let parent = open_service(directory)?;
    let manifest = read_manifest(&parent)?;
    let mut project: Project =
        serde_yaml_ng::from_slice(&manifest).context("decode lampyr.yaml")?;
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&manifest)?;
    ensure!(
        !matches!(
            project.application,
            Some(crate::project::Application::Standalone { .. })
        ),
        "standalone application files are customer-owned and cannot be upgraded"
    );
    crate::project::validate_runtime(&project.runtime)?;
    ensure!(
        project.application.is_some(),
        crate::project::MISSING_APPLICATION
    );
    crate::project::validate_name(&project.name)?;
    ensure!(
        project.command.is_empty()
            && value.get("command").is_none()
            && value.get("protocols").is_none(),
        "explicit adapter upgrades require an adapter-managed Handler Application"
    );
    let adapter = open_child(&parent, DIRECTORY, true).context("open runtime adapter")?;
    ensure!(
        unsafe { libc::flock(adapter.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another adapter upgrade is running"
    );
    let before = Snapshot::read(&adapter)?;
    let metadata = before.metadata()?;
    validate_metadata(&metadata, &project.runtime)?;
    validate_baseline(&before, &metadata, "baseline")?;
    project.command = metadata.entrypoint.clone();
    project.protocols.http = true;
    project.protocols.streaming = true;
    project.protocols.websocket = true;
    project.protocols.events = value.get("events").is_some();
    project.validate()?;
    let target = target(&project.runtime, &project.name)?;
    upgrade_to(
        directory,
        &parent,
        &adapter,
        &before,
        &metadata,
        &target,
        &manifest,
        write_conflicts,
        color,
    )
}

#[allow(clippy::too_many_arguments)]
fn upgrade_to(
    directory: &Path,
    parent: &File,
    adapter: &File,
    before: &Snapshot,
    metadata: &AdapterMetadata,
    target: &Snapshot,
    manifest: &[u8],
    write_conflicts: bool,
    color: Color,
) -> Result<()> {
    let mut files = before.files.clone();
    let mut conflicts = Vec::new();
    if let Some(pending) = pending(before, metadata)? {
        for path in &pending.conflicts {
            if let Some(file) = files.get(path) {
                ensure!(
                    !has_markers(&file.bytes),
                    "unresolved conflict markers in .lampyr-adapter/{path}; resolve before rerunning --upgrade"
                );
            }
        }
        // pending.json and pending-baseline/ are untracked, so a git restore of
        // the half-applied upgrade leaves them behind; finishing would then
        // record the target over stale files and lose the clean merges.
        for (path, expected) in &pending.merged {
            ensure!(
                files.get(path).map(|file| digest(&file.bytes)) == *expected,
                ".lampyr-adapter/{path} no longer matches the merge written by --write-conflicts; to cancel the pending upgrade delete .lampyr-adapter/pending.json and .lampyr-adapter/pending-baseline/, then rerun --upgrade"
            );
        }
        let installed = target.metadata()?;
        if !matches_template(&pending.target, &installed) {
            eprintln!(
                "warning: finalizing pinned adapter target {}; installed template {} differs; rerun --upgrade separately for the installed target",
                pending.target.template_version, installed.template_version
            );
        }
        let merged = merge_metadata(metadata, &pending.target, &installed)?;
        promote(&mut files, &merged, before, "pending-baseline")?;
    } else {
        let current = target.metadata()?;
        // An older ly must not silently undo a newer template's fixes.
        ensure!(
            newer_than(&metadata.template_version, &current.template_version) != Some(true),
            "runtime adapter template {} is newer than this ly's {}; upgrade ly instead of downgrading the adapter",
            metadata.template_version,
            current.template_version
        );
        validate_metadata(&current, &metadata.runtime)?;
        // Fail before writing conflict state if the metadata itself conflicts.
        merge_metadata(metadata, &current, &current)?;
        validate_baseline(target, &current, "baseline")?;
        let temp = tempfile::tempdir()?;
        let repo = Repository::init_bare(temp.path())?;
        for path in metadata
            .files
            .keys()
            .chain(current.files.keys())
            .collect::<BTreeSet<_>>()
        {
            let ours = before.files.get(path);
            if let Some(ours) = ours {
                ensure!(
                    !has_markers(&ours.bytes),
                    "unresolved conflict markers in .lampyr-adapter/{path}"
                );
            }
            let (merged, conflict) = merge(
                &repo,
                path,
                before.files.get(&format!("baseline/{path}")),
                ours,
                target.files.get(path),
            )?;
            if conflict {
                preview(path, ours, target.files.get(path), color)?;
                conflicts.push(path.clone());
            }
            match merged {
                Some(merged) => {
                    files.insert(path.clone(), merged);
                }
                None => {
                    files.remove(path);
                }
            }
        }
        if !conflicts.is_empty() {
            ensure!(
                write_conflicts,
                "runtime adapter conflicts; nothing written; use --write-conflicts to resolve in place"
            );
            for path in current.files.keys() {
                files.insert(
                    format!("pending-baseline/{path}"),
                    Source {
                        bytes: target.bytes(&format!("baseline/{path}"))?.to_vec(),
                        mode: before
                            .files
                            .get(&format!("baseline/{path}"))
                            .unwrap_or(&target.files[&format!("baseline/{path}")])
                            .mode,
                    },
                );
            }
            let merged = metadata
                .files
                .keys()
                .chain(current.files.keys())
                .filter(|path| !conflicts.contains(path))
                .map(|path| {
                    (
                        path.clone(),
                        files.get(path).map(|file| digest(&file.bytes)),
                    )
                })
                .collect();
            files.insert(
                "pending.json".into(),
                encode(&Pending {
                    base_metadata_digest: digest(before.bytes("adapter.json")?),
                    target: current,
                    conflicts: conflicts.clone(),
                    merged,
                })?,
            );
        } else {
            let merged = merge_metadata(metadata, &current, &current)?;
            promote(&mut files, &merged, target, "baseline")?;
        }
    }
    preserve_metadata_modes(&mut files, before);
    install(directory, parent, adapter, before, &files, manifest)?;
    ensure!(
        conflicts.is_empty(),
        "runtime adapter conflicts written; resolve markers (delete files to accept deletion), then rerun the same --upgrade"
    );
    Ok(())
}

fn preserve_metadata_modes(files: &mut BTreeMap<String, Source>, before: &Snapshot) {
    if let (Some(metadata), Some(original)) = (
        files.get_mut("adapter.json"),
        before.files.get("adapter.json"),
    ) {
        metadata.mode = original.mode;
    }
    if let Some(pending) = files.get_mut("pending.json") {
        pending.mode = before
            .files
            .get("pending.json")
            .or_else(|| before.files.get("adapter.json"))
            .map_or(pending.mode, |original| original.mode);
    }
}

fn install(
    _directory: &Path,
    parent: &File,
    adapter: &File,
    before: &Snapshot,
    files: &BTreeMap<String, Source>,
    manifest: &[u8],
) -> Result<()> {
    let named = open_child(parent, DIRECTORY, true)?;
    ensure!(
        (named.metadata()?.dev(), named.metadata()?.ino()) == before.root_id,
        "adapter directory changed concurrently; nothing installed"
    );
    if files == &before.files {
        ensure!(
            Snapshot::read(adapter)? == *before && read_manifest(parent)? == manifest,
            "adapter changed concurrently"
        );
        return Ok(());
    }
    for path in files.keys() {
        safe_path(path)?;
        ensure!(
            !Path::new(path)
                .ancestors()
                .skip(1)
                .any(|p| files.contains_key(p.to_str().unwrap_or(""))),
            "adapter file/directory collision at {path}"
        );
        ensure!(
            !before.directories.contains_key(path),
            "adapter target collides with existing directory: {path}"
        );
    }
    ensure!(
        files.len() <= MAX_ENTRIES
            && files.values().map(|s| s.bytes.len()).sum::<usize>() <= MAX_TOTAL,
        "upgraded adapter exceeds size limits"
    );
    #[cfg(target_os = "linux")]
    let staging_parent = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let staging_parent = _directory.to_owned();
    let staging_directory = tempfile::Builder::new()
        .prefix(".lampyr-upgrade-")
        .tempdir_in(&staging_parent)?;
    let mut staging = StagingDir::new(staging_directory, parent)?;
    let name = staging.name.clone();
    // Pin the staging directory by fd and do every write relative to it, so a
    // directory swapped in under its name can never receive adapter files.
    let staging_root = staging.root.try_clone()?;
    let staging_id = {
        let metadata = staging_root.metadata()?;
        (metadata.dev(), metadata.ino())
    };
    ensure!(
        staging_id == staging.expected_id,
        "staging directory identity changed; nothing installed"
    );
    #[cfg(test)]
    tests::AFTER_STAGING.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(staging.path());
        }
    });
    let same_dir = |file: &File| -> Result<bool> {
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()) == staging_id)
    };
    let staging_dir = cap_std::fs::Dir::from_std_file(staging_root.try_clone()?);
    for path in before.directories.keys() {
        if path != "baseline"
            && !path.starts_with("baseline/")
            && path != "pending-baseline"
            && !path.starts_with("pending-baseline/")
        {
            staging_dir.create_dir_all(path)?;
        }
    }
    for (path, source) in files {
        ensure!(
            source.bytes.len() as u64 <= MAX_FILE,
            "upgraded adapter file too large: {path}"
        );
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            staging_dir.create_dir_all(parent)?;
        }
        let mut file = staging_dir
            .create(path)
            .with_context(|| format!("stage adapter {path}"))?
            .into_std();
        file.write_all(&source.bytes)?;
        file.set_permissions(Permissions::from_mode(source.mode))?;
        file.sync_all()?;
    }
    for (path, mode) in before.directories.iter().rev() {
        if staging_dir.is_dir(path) {
            staging_dir.set_permissions(
                path,
                cap_std::fs::Permissions::from_std(Permissions::from_mode(*mode)),
            )?;
        }
    }
    staging_root.set_permissions(adapter.metadata()?.permissions())?;
    #[cfg(test)]
    tests::AFTER_STAGED_WRITES.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(staging.path());
        }
    });
    let staged = open_child(parent, &name, true)?;
    ensure!(
        same_dir(&staged)?,
        "staging directory replaced during installation; nothing installed"
    );
    ensure!(
        unsafe { libc::flock(staged.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "could not lock staged runtime adapter"
    );
    let staged_snapshot = Snapshot::read(&staged).context("verify staged adapter")?;
    // A concurrent writer could alter staged files after they were written;
    // install only exactly what the merge produced.
    if staged_snapshot.files != *files {
        let retained = staging.retain();
        bail!(
            "staged adapter does not match the merge result; nothing installed; staging retained as {}",
            retained
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
    }
    // Refuse what every later command would reject, such as a tracked file a
    // customer replaced with a directory.
    Snapshot::read_tracked(&staged)
        .context("upgraded adapter would not load; nothing installed")?;
    staging.expect(staged_snapshot.clone());
    let staged_metadata = staged_snapshot.metadata()?;
    validate_metadata(&staged_metadata, &staged_metadata.runtime)?;
    validate_baseline(&staged_snapshot, &staged_metadata, "baseline")?;
    pending(&staged_snapshot, &staged_metadata)?;
    for path in staged_snapshot.directories.keys().rev() {
        // cap-std opens directories O_PATH, which cannot be fsynced.
        let mut directory = staging_root.try_clone()?;
        for name in path.split('/') {
            directory = open_child(&directory, name, true)?;
        }
        directory.sync_all()?;
    }
    staged.sync_all()?;
    ensure!(
        Snapshot::read(adapter)? == *before && read_manifest(parent)? == manifest,
        "adapter changed concurrently; nothing installed"
    );
    let named = open_child(parent, DIRECTORY, true)?;
    ensure!(
        named.metadata()?.ino() == adapter.metadata()?.ino()
            && named.metadata()?.dev() == adapter.metadata()?.dev(),
        "adapter directory changed concurrently; nothing installed"
    );
    exchange(parent, &name)?;
    #[cfg(test)]
    tests::AFTER_EXCHANGE.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(&_directory.join(DIRECTORY));
        }
    });
    staging.expect(before.clone());
    // Validate the actual directory displaced by the exchange, closing the
    // check/rename window. Keep the old directory if rollback itself fails.
    let displaced = open_child(parent, &name, true).and_then(|old| {
        ensure!(
            old.metadata()?.ino() == adapter.metadata()?.ino()
                && old.metadata()?.dev() == adapter.metadata()?.dev(),
            "adapter directory replaced during installation"
        );
        Snapshot::read(&old)
    });
    let installed_is_staged = open_child(parent, DIRECTORY, true)
        .and_then(|current| {
            ensure!(same_dir(&current)?, "installed adapter identity changed");
            Snapshot::read(&current)
        })
        .is_ok_and(|current| current == staged_snapshot);
    if displaced.as_ref().is_err()
        || displaced.as_ref().is_ok_and(|old| old != before)
        || !installed_is_staged
        || !read_manifest(parent).is_ok_and(|bytes| bytes == manifest)
    {
        let installed_is_named = open_child(parent, DIRECTORY, true)
            .and_then(|current| Snapshot::read(&current))
            .is_ok_and(|current| current == staged_snapshot);
        if !installed_is_named || !staging.matches_named_directory() {
            let _retained = staging.retain();
            bail!(
                "adapter changed concurrently on both sides of installation; cannot safely roll back; previous adapter retained in service directory as {name}"
            );
        }
        if let Err(error) = exchange(parent, &name) {
            let _retained = staging.retain();
            bail!(
                "adapter changed concurrently and rollback failed: {error:#}; previous adapter retained in service directory as {name}"
            );
        }
        staging.expect(staged_snapshot.clone());
        if let Err(error) = parent.sync_all() {
            let _retained = staging.retain();
            bail!(
                "adapter changed concurrently; installation rolled back but directory sync failed: {error}; displaced adapter retained in service directory as {name}"
            );
        }
        bail!("adapter changed concurrently; installation rolled back");
    }
    finish_install(parent, staging, &staged_snapshot, File::sync_all, exchange)
}

fn finish_install(
    parent: &File,
    mut staging: StagingDir,
    installed: &Snapshot,
    sync: impl Fn(&File) -> std::io::Result<()>,
    rollback: impl Fn(&File, &str) -> Result<()>,
) -> Result<()> {
    if let Err(error) = sync(parent) {
        let name = staging.name.clone();
        let unchanged = open_child(parent, DIRECTORY, true)
            .and_then(|current| Snapshot::read(&current))
            .is_ok_and(|current| current == *installed)
            && staging.matches_named_directory();
        if !unchanged {
            let _retained = staging.retain();
            bail!(
                "sync service directory after adapter install failed: {error}; adapter changed concurrently; cannot safely roll back; previous adapter retained in service directory as {name}"
            );
        }
        if let Err(rollback_error) = rollback(parent, &name) {
            let _retained = staging.retain();
            bail!(
                "sync service directory after adapter install failed: {error}; rollback failed: {rollback_error:#}; previous adapter retained in service directory as {name}"
            );
        }
        staging.expect(installed.clone());
        if let Err(sync_error) = sync(parent) {
            let _retained = staging.retain();
            bail!(
                "sync service directory after adapter install failed: {error}; installation rolled back but rollback directory sync failed: {sync_error}; displaced adapter retained in service directory as {name}"
            );
        }
        bail!(
            "sync service directory after adapter install failed: {error}; installation rolled back"
        );
    }
    // The staging directory now holds the displaced adapter. TempDir's drop
    // ignores removal errors, which would silently leave it in the service.
    let name = staging.name.clone();
    if let Err(error) = staging.cleanup() {
        eprintln!(
            "warning: adapter upgraded, but the previous adapter copy {name} could not be removed: {error:#}; delete the retained directory from the service directory"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn exchange(parent: &File, staging: &str) -> Result<()> {
    let adapter = CString::new(DIRECTORY)?;
    let staging = CString::new(staging)?;
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            adapter.as_ptr(),
            parent.as_raw_fd(),
            staging.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomically exchange runtime adapter (original unchanged)");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn exchange(_: &File, _: &str) -> Result<()> {
    bail!("atomic adapter upgrades currently require Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(bytes: &[u8]) -> Source {
        Source {
            bytes: bytes.to_vec(),
            mode: 0o644,
        }
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        crate::project::initialize(root.path(), Some("example"), "python").unwrap();
        set_base(
            root.path(),
            "server.py",
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        );
        root
    }

    fn set_base(root: &Path, path: &str, bytes: &[u8]) {
        let adapter = root.join(DIRECTORY);
        std::fs::write(adapter.join(path), bytes).unwrap();
        std::fs::write(adapter.join("baseline").join(path), bytes).unwrap();
        let mut metadata: AdapterMetadata =
            serde_json::from_slice(&std::fs::read(adapter.join("adapter.json")).unwrap()).unwrap();
        metadata.files.insert(path.into(), digest(bytes));
        std::fs::write(
            adapter.join("adapter.json"),
            encode(&metadata).unwrap().bytes,
        )
        .unwrap();
    }

    fn snapshot(root: &Path) -> Snapshot {
        let (_, adapter) = open_adapter(root).unwrap();
        Snapshot::read(&adapter).unwrap()
    }

    fn version(root: &Path, version: &str, changes: &[(&str, Option<&[u8]>)]) -> Snapshot {
        let mut result = snapshot(root);
        result
            .files
            .retain(|p, _| p == "adapter.json" || p.starts_with("baseline/"));
        let mut metadata = result.metadata().unwrap();
        for path in metadata.files.keys() {
            result.files.insert(
                path.clone(),
                source(result.bytes(&format!("baseline/{path}")).unwrap()),
            );
        }
        metadata.template_version = version.into();
        for (path, contents) in changes {
            match contents {
                Some(contents) => {
                    metadata.files.insert((*path).into(), digest(contents));
                    result.files.insert((*path).into(), source(contents));
                    result
                        .files
                        .insert(format!("baseline/{path}"), source(contents));
                }
                None => {
                    metadata.files.remove(*path);
                    result.files.remove(*path);
                    result.files.remove(&format!("baseline/{path}"));
                }
            }
        }
        result
            .files
            .insert("adapter.json".into(), encode(&metadata).unwrap());
        result
    }

    fn with_snapshot_warmup(mut target: Snapshot, code: &str) -> Snapshot {
        let mut metadata = target.metadata().unwrap();
        metadata.snapshot_warmup = Some(code.into());
        target
            .files
            .insert("adapter.json".into(), encode(&metadata).unwrap());
        target
    }

    #[test]
    fn template_comparison_includes_entrypoint_and_snapshot_warmup() {
        let current = fixture();
        let metadata = snapshot(current.path()).metadata().unwrap();
        let mut changed = metadata.clone();
        changed.entrypoint.push_str(".custom");
        assert!(!matches_template(&metadata, &changed));
        changed = metadata.clone();
        changed.snapshot_warmup = Some("custom warmup".into());
        assert!(!matches_template(&metadata, &changed));
        changed = metadata.clone();
        changed.template_version.push_str("-custom");
        assert!(!matches_template(&metadata, &changed));
        assert!(matches_template(&metadata, &metadata));
    }

    fn run(root: &Path, target: &Snapshot, write: bool) -> Result<()> {
        let (parent, adapter) = open_adapter(root)?;
        let before = Snapshot::read(&adapter)?;
        let metadata = before.metadata()?;
        validate_metadata(&metadata, &metadata.runtime)?;
        validate_baseline(&before, &metadata, "baseline")?;
        upgrade_to(
            root,
            &parent,
            &adapter,
            &before,
            &metadata,
            target,
            &read_manifest(&parent)?,
            write,
            Color::Never,
        )
    }

    #[test]
    fn clean_transaction_preserves_edits_permissions_and_user_owned_files() {
        let root = fixture();
        let target = version(
            root.path(),
            "2.0.0",
            &[
                (
                    "server.py",
                    Some(b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n"),
                ),
                ("nested/new.py", Some(b"new\n")),
                ("lampyr_types.py", None),
            ],
        );
        let adapter = root.path().join(DIRECTORY);
        std::fs::write(
            adapter.join("server.py"),
            b"one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
        )
        .unwrap();
        std::fs::set_permissions(adapter.join("server.py"), Permissions::from_mode(0o755)).unwrap();
        std::fs::write(adapter.join("customer.py"), "custom helper").unwrap();
        let owned: Vec<_> = ["handler.py", "lampyr.yaml", "Dockerfile", ".gitignore"]
            .into_iter()
            .map(|p| (p, std::fs::read(root.path().join(p)).unwrap()))
            .collect();
        run(root.path(), &target, false).unwrap();
        let after = snapshot(root.path());
        assert_eq!(
            after.bytes("server.py").unwrap(),
            b"ONE\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n"
        );
        assert_eq!(after.files["server.py"].mode, 0o755);
        assert_eq!(after.bytes("customer.py").unwrap(), b"custom helper");
        assert_eq!(after.bytes("nested/new.py").unwrap(), b"new\n");
        assert!(!after.files.contains_key("lampyr_types.py"));
        assert!(!after.files.contains_key("baseline/lampyr_types.py"));
        assert_eq!(after.metadata().unwrap().template_version, "2.0.0");
        validate_baseline(&after, &after.metadata().unwrap(), "baseline").unwrap();
        for (path, bytes) in owned {
            assert_eq!(std::fs::read(root.path().join(path)).unwrap(), bytes);
        }
        let before = snapshot(root.path());
        run(root.path(), &target, false).unwrap();
        assert_eq!(
            snapshot(root.path()),
            before,
            "no-op upgrades must not rewrite source"
        );
    }

    type StagingHook = Box<dyn Fn(&Path)>;

    thread_local! {
        pub(super) static AFTER_STAGING: std::cell::RefCell<Option<StagingHook>> =
            const { std::cell::RefCell::new(None) };
        pub(super) static AFTER_QUARANTINE: std::cell::RefCell<Option<StagingHook>> =
            const { std::cell::RefCell::new(None) };
        pub(super) static AFTER_EXCHANGE: std::cell::RefCell<Option<StagingHook>> =
            const { std::cell::RefCell::new(None) };
        pub(super) static AFTER_STAGED_WRITES: std::cell::RefCell<Option<StagingHook>> =
            const { std::cell::RefCell::new(None) };
        pub(super) static QUARANTINE_REPLACEMENT: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn replaced_staging_directory_is_never_deleted() {
        let root = fixture();
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
        AFTER_STAGING.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|staging: &Path| {
                std::fs::rename(staging, staging.with_extension("moved")).unwrap();
                std::fs::create_dir(staging).unwrap();
                std::fs::write(staging.join("customer.txt"), b"keep me").unwrap();
            }));
        });
        let result = run(root.path(), &target, false);
        AFTER_STAGING.with(|hook| *hook.borrow_mut() = None);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("staging directory replaced")
        );
        let replacement = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.join("customer.txt").exists())
            .expect("replacement directory was deleted");
        assert_eq!(
            std::fs::read(replacement.join("customer.txt")).unwrap(),
            b"keep me"
        );
    }

    #[test]
    fn quarantine_replacement_is_not_removed_during_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix(".lampyr-upgrade-")
            .tempdir_in(root.path())
            .unwrap();
        let mut staging = StagingDir::new(temporary, &parent).unwrap();
        QUARANTINE_REPLACEMENT.with(|path| *path.borrow_mut() = None);
        AFTER_QUARANTINE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|quarantine: &Path| {
                let moved = quarantine.with_extension("moved");
                std::fs::rename(quarantine, &moved).unwrap();
                std::fs::create_dir(quarantine).unwrap();
                std::fs::write(quarantine.join("customer-data"), "must remain").unwrap();
                QUARANTINE_REPLACEMENT
                    .with(|path| *path.borrow_mut() = Some(quarantine.to_owned()));
            }));
        });

        let error = staging.cleanup().unwrap_err().to_string();
        AFTER_QUARANTINE.with(|hook| *hook.borrow_mut() = None);
        assert!(error.contains("cleanup directory changed"));
        let replacement = QUARANTINE_REPLACEMENT.with(|path| path.borrow_mut().take().unwrap());
        assert_eq!(
            std::fs::read(replacement.join("customer-data")).unwrap(),
            b"must remain"
        );
    }

    #[test]
    fn staged_contents_changed_after_exchange_are_not_accepted() {
        let root = fixture();
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
        let original = std::fs::read(root.path().join(DIRECTORY).join("server.py")).unwrap();
        AFTER_EXCHANGE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|adapter: &Path| {
                std::fs::write(adapter.join("server.py"), b"concurrent edit\n").unwrap();
            }));
        });
        let result = run(root.path(), &target, false);
        AFTER_EXCHANGE.with(|hook| *hook.borrow_mut() = None);

        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.path().join(DIRECTORY).join("server.py")).unwrap(),
            b"concurrent edit\n"
        );
        assert!(std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lampyr-upgrade-")
        }));
        assert_ne!(
            std::fs::read(root.path().join(DIRECTORY).join("server.py")).unwrap(),
            original
        );
    }

    fn edit_metadata(root: &Path, edit: impl Fn(&mut AdapterMetadata)) {
        let path = root.join(DIRECTORY).join("adapter.json");
        let mut metadata: AdapterMetadata =
            decode(&std::fs::read(&path).unwrap(), "adapter.json").unwrap();
        edit(&mut metadata);
        std::fs::write(&path, encode(&metadata).unwrap().bytes).unwrap();
    }

    fn with_entrypoint(mut target: Snapshot, entrypoint: &str) -> Snapshot {
        let mut metadata = target.metadata().unwrap();
        metadata.entrypoint = entrypoint.into();
        target
            .files
            .insert("adapter.json".into(), encode(&metadata).unwrap());
        target
    }

    #[test]
    fn upgrades_keep_customer_metadata_edits() {
        let root = fixture();
        edit_metadata(root.path(), |metadata| {
            metadata.entrypoint = "/custom-entry".into();
            metadata.snapshot_warmup = None;
        });
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
        run(root.path(), &target, false).unwrap();
        let after = snapshot(root.path()).metadata().unwrap();
        assert_eq!(after.entrypoint, "/custom-entry");
        assert_eq!(after.snapshot_warmup, None);
        assert_eq!(after.template_version, "2.0.0");
        assert_eq!(
            after.pristine_values(),
            target.metadata().unwrap().own_values()
        );
    }

    #[test]
    fn upgrades_take_template_metadata_changes_when_unedited() {
        let root = fixture();
        let target = with_entrypoint(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            "/new-entry",
        );
        run(root.path(), &target, false).unwrap();
        assert_eq!(
            snapshot(root.path()).metadata().unwrap().entrypoint,
            "/new-entry"
        );
    }

    #[test]
    fn conflicting_metadata_edits_fail_without_writing() {
        let root = fixture();
        edit_metadata(root.path(), |metadata| {
            metadata.entrypoint = "/custom-entry".into()
        });
        let target = with_entrypoint(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            "/new-entry",
        );
        let before = snapshot(root.path());
        for write in [false, true] {
            assert!(
                run(root.path(), &target, write)
                    .unwrap_err()
                    .to_string()
                    .contains("entrypoint was customized")
            );
            assert_eq!(snapshot(root.path()), before);
        }
    }

    #[test]
    fn finishing_refuses_a_reverted_half_upgrade() {
        let root = fixture();
        let adapter = root.path().join(DIRECTORY);
        std::fs::write(adapter.join("server.py"), b"customer\n").unwrap();
        let original_types = std::fs::read(adapter.join("lampyr_types.py")).unwrap();
        let target = version(
            root.path(),
            "2.0.0",
            &[
                ("server.py", Some(b"template\n")),
                ("lampyr_types.py", Some(b"# template fix\n")),
            ],
        );
        assert!(run(root.path(), &target, true).is_err()); // conflicts written
        assert_eq!(
            std::fs::read(adapter.join("lampyr_types.py")).unwrap(),
            b"# template fix\n"
        );
        // `git restore .lampyr-adapter` brings back tracked files only.
        std::fs::write(adapter.join("server.py"), b"customer\n").unwrap();
        std::fs::write(adapter.join("lampyr_types.py"), &original_types).unwrap();
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains(".lampyr-adapter/lampyr_types.py no longer matches")
        );
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn legacy_metadata_edits_survive_same_version_upgrades() {
        let root = fixture();
        edit_metadata(root.path(), |metadata| {
            metadata.pristine = None;
            metadata.entrypoint = "/custom-entry".into();
        });
        let installed = snapshot(root.path()).metadata().unwrap().template_version;
        let target = version(root.path(), &installed, &[]);
        let mut target_metadata = target.metadata().unwrap();
        target_metadata.entrypoint = rendered_entrypoint();
        let mut target = target;
        target
            .files
            .insert("adapter.json".into(), encode(&target_metadata).unwrap());
        run(root.path(), &target, false).unwrap();
        assert_eq!(
            snapshot(root.path()).metadata().unwrap().entrypoint,
            "/custom-entry"
        );
    }

    #[test]
    fn legacy_metadata_edits_across_versions_fail_without_writing() {
        let root = fixture();
        edit_metadata(root.path(), |metadata| {
            metadata.pristine = None;
            metadata.entrypoint = "/custom-entry".into();
        });
        let target = with_entrypoint(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            &rendered_entrypoint(),
        );
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("no recorded pristine values")
        );
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn legacy_metadata_with_different_files_at_the_same_version_is_not_guessed() {
        let root = fixture();
        edit_metadata(root.path(), |metadata| {
            metadata.pristine = None;
            metadata.snapshot_warmup = None;
        });
        let installed = snapshot(root.path()).metadata().unwrap().template_version;
        let mut target = version(
            root.path(),
            &installed,
            &[("server.py", Some(b"installed\n"))],
        );
        let mut target_metadata = target.metadata().unwrap();
        target_metadata.snapshot_warmup = Some("import warmup".into());
        target
            .files
            .insert("adapter.json".into(), encode(&target_metadata).unwrap());
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("no recorded pristine values")
        );
        let error = run(root.path(), &target, false).unwrap_err().to_string();
        assert!(!error.contains("Some("), "{error}");
        assert_eq!(snapshot(root.path()), before);
    }

    fn rendered_entrypoint() -> String {
        let root = tempfile::tempdir().unwrap();
        crate::project::initialize(root.path(), Some("example"), "python").unwrap();
        snapshot(root.path()).metadata().unwrap().entrypoint
    }

    #[test]
    fn failed_cleanup_claim_leaves_only_the_named_staging_copy() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture();
        let adapter = root.path().join(DIRECTORY);
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
        // A read-only adapter root is kept on the staged copy, so moving the
        // displaced copy into the cleanup quarantine fails with EACCES.
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = run(root.path(), &target, false);
        let entries: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        for entry in &entries {
            let path = root.path().join(entry);
            if entry.starts_with(".lampyr-") {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
            }
        }
        let _ = std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755));
        result.unwrap();
        assert!(
            !entries
                .iter()
                .any(|entry| entry.starts_with(".lampyr-cleanup-")),
            "{entries:?}"
        );
    }

    #[test]
    fn edited_metadata_during_a_pending_upgrade_blocks_only_the_upgrade() {
        let root = fixture();
        std::fs::write(root.path().join(DIRECTORY).join("server.py"), b"customer\n").unwrap();
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"template\n"))]);
        assert!(run(root.path(), &target, true).is_err()); // conflicts written
        std::fs::write(root.path().join(DIRECTORY).join("server.py"), b"resolved\n").unwrap();
        edit_metadata(root.path(), |metadata| {
            metadata.entrypoint = "/custom-entry".into()
        });
        // Lifecycle commands load metadata without needing the pending upgrade.
        assert_eq!(
            metadata(root.path(), "python").unwrap().entrypoint,
            "/custom-entry"
        );
        let error = run(root.path(), &target, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("cancel the upgrade"),
            "{error:#}"
        );
    }

    #[test]
    fn customers_keep_conflicting_metadata_by_acknowledging_the_new_template() {
        for field in ["entrypoint", "snapshotWarmup"] {
            let root = fixture();
            edit_metadata(root.path(), |metadata| match field {
                "entrypoint" => metadata.entrypoint = "/custom-entry".into(),
                _ => metadata.snapshot_warmup = Some("custom warmup".into()),
            });
            let mut target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
            let mut target_metadata = target.metadata().unwrap();
            target_metadata.entrypoint = "/new-entry".into();
            target_metadata.snapshot_warmup = Some("new warmup".into());
            target
                .files
                .insert("adapter.json".into(), encode(&target_metadata).unwrap());
            let error = run(root.path(), &target, false).unwrap_err().to_string();
            assert!(error.contains(&format!("set pristine.{field}")), "{error}");
            assert!(!error.contains("Some("), "{error}");
            // Following the message: acknowledge the new template value as the base.
            edit_metadata(root.path(), |metadata| {
                let mut pristine = metadata.pristine_values();
                match field {
                    "entrypoint" => pristine.entrypoint = "/new-entry".into(),
                    _ => pristine.snapshot_warmup = Some("new warmup".into()),
                }
                metadata.pristine = Some(pristine);
            });
            run(root.path(), &target, false).unwrap();
            let after = snapshot(root.path()).metadata().unwrap();
            match field {
                "entrypoint" => assert_eq!(after.entrypoint, "/custom-entry"),
                _ => assert_eq!(after.snapshot_warmup.as_deref(), Some("custom warmup")),
            }
        }
    }

    #[test]
    fn staged_files_changed_before_install_are_refused() {
        let root = fixture();
        let target = version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]);
        let before = snapshot(root.path());
        AFTER_STAGED_WRITES.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|staging: &Path| {
                std::fs::write(staging.join("server.py"), b"tampered\n").unwrap();
            }));
        });
        let result = run(root.path(), &target, false);
        AFTER_STAGED_WRITES.with(|hook| *hook.borrow_mut() = None);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("does not match the merge result")
        );
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn tracked_files_replaced_by_directories_are_refused() {
        let root = fixture();
        let adapter = root.path().join(DIRECTORY);
        std::fs::remove_file(adapter.join("server.py")).unwrap();
        std::fs::create_dir(adapter.join("server.py")).unwrap();
        let target = version(
            root.path(),
            "2.0.0",
            &[("lampyr_types.py", Some(b"# new types\n"))],
        );
        let before = snapshot(root.path());
        let error = run(root.path(), &target, false).unwrap_err();
        assert!(format!("{error:#}").contains("would not load"), "{error:#}");
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn older_templates_never_downgrade_a_newer_adapter() {
        let root = fixture();
        let target = version(root.path(), "0.0.1", &[("server.py", Some(b"older\n"))]);
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("newer than this ly")
        );
        assert_eq!(snapshot(root.path()), before);
        assert_eq!(newer_than("0.10.0", "0.9.0"), Some(true));
        assert_eq!(newer_than("0.1.0", "0.1.0"), Some(false));
        assert_eq!(newer_than("custom", "0.1.0"), None);
        assert_eq!(newer_than("1.0.0", "1.0"), Some(false));
        assert_eq!(newer_than("0.2.0", "0.2.0-rc.1"), Some(true));
        assert_eq!(newer_than("0.2.0-rc.2", "0.2.0-rc.1"), Some(true));
        assert_eq!(newer_than("0.2.0-rc.10", "0.2.0-rc.9"), Some(true));
        assert_eq!(newer_than("0.2.0-rc.1", "0.2.0"), Some(false));
    }

    #[test]
    fn upgrade_rejects_targets_that_project_load_would_reject() {
        let root = fixture();
        let target = with_snapshot_warmup(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            "   ",
        );
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("snapshotWarmup must not be empty")
        );
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn clean_upgrade_preserves_pinned_snapshot_warmup_code() {
        let root = fixture();
        let target = with_snapshot_warmup(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            "require('fastify');\nrequire('@fastify/websocket');\nrequire('ws');\n",
        );

        run(root.path(), &target, false).unwrap();

        assert_eq!(
            snapshot(root.path())
                .metadata()
                .unwrap()
                .snapshot_warmup
                .as_deref(),
            Some("require('fastify');\nrequire('@fastify/websocket');\nrequire('ws');\n")
        );
    }

    #[test]
    fn conflicts_are_no_write_and_pending_target_survives_installed_template_changes() {
        let root = fixture();
        let target = with_snapshot_warmup(
            version(root.path(), "2.0.0", &[("server.py", Some(b"installed\n"))]),
            "require('fastify');\nrequire('@fastify/websocket');\n",
        );
        std::fs::write(root.path().join(DIRECTORY).join("server.py"), b"customer\n").unwrap();
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("nothing written")
        );
        assert_eq!(snapshot(root.path()), before);
        assert!(
            run(root.path(), &target, true)
                .unwrap_err()
                .to_string()
                .contains("conflicts written")
        );
        let written = snapshot(root.path());
        assert_eq!(
            written.bytes("adapter.json").unwrap(),
            before.bytes("adapter.json").unwrap()
        );
        assert_eq!(
            written.bytes("baseline/server.py").unwrap(),
            before.bytes("baseline/server.py").unwrap()
        );
        assert!(has_markers(written.bytes("server.py").unwrap()));
        let newer = version(root.path(), "3.0.0", &[("server.py", Some(b"newer\n"))]);
        assert!(
            run(root.path(), &newer, false)
                .unwrap_err()
                .to_string()
                .contains("unresolved conflict")
        );
        assert_eq!(snapshot(root.path()), written);
        std::fs::write(
            root.path().join(DIRECTORY).join("server.py"),
            b"customer resolved\n",
        )
        .unwrap();
        run(root.path(), &newer, false).unwrap();
        let after = snapshot(root.path());
        assert_eq!(after.bytes("server.py").unwrap(), b"customer resolved\n");
        assert_eq!(after.metadata().unwrap().template_version, "2.0.0");
        assert_eq!(
            after.metadata().unwrap().snapshot_warmup.as_deref(),
            Some("require('fastify');\nrequire('@fastify/websocket');\n")
        );
        assert_eq!(after.bytes("baseline/server.py").unwrap(), b"installed\n");
        assert!(!after.files.contains_key("pending.json"));
        assert!(!after.directories.contains_key("pending-baseline"));
        // A subsequent upgrade sees the newer target, but cannot erase the resolution.
        assert!(run(root.path(), &newer, false).is_err());
        assert_eq!(snapshot(root.path()), after);
    }

    #[test]
    fn older_adapter_metadata_defaults_warmup_to_none_and_omits_it_when_encoded() {
        let root = fixture();
        let before = snapshot(root.path());
        let metadata = before.metadata().unwrap();
        assert_eq!(metadata.snapshot_warmup, None);
        assert!(
            !serde_json::from_slice::<serde_json::Value>(before.bytes("adapter.json").unwrap())
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("snapshotWarmup")
        );
        assert_eq!(
            serde_json::from_slice::<AdapterMetadata>(&encode(&metadata).unwrap().bytes)
                .unwrap()
                .snapshot_warmup,
            None
        );
    }

    #[test]
    fn restrictive_pristine_modes_survive_clean_and_pending_upgrades() {
        for conflict in [false, true] {
            let root = fixture();
            let adapter = root.path().join(DIRECTORY);
            std::fs::set_permissions(adapter.join("adapter.json"), Permissions::from_mode(0o600))
                .unwrap();
            for path in ["server.py", "baseline/server.py"] {
                std::fs::set_permissions(adapter.join(path), Permissions::from_mode(0o600))
                    .unwrap();
            }
            let target = version(
                root.path(),
                "2.0.0",
                &[
                    ("server.py", Some(b"installed\n")),
                    ("new.py", Some(b"new\n")),
                ],
            );
            if conflict {
                std::fs::write(adapter.join("server.py"), b"customer\n").unwrap();
                assert!(run(root.path(), &target, true).is_err());
                assert_eq!(
                    snapshot(root.path()).files["pending-baseline/server.py"].mode,
                    0o600
                );
                assert_eq!(snapshot(root.path()).files["pending.json"].mode, 0o600);
                assert_eq!(snapshot(root.path()).files["adapter.json"].mode, 0o600);
                std::fs::write(adapter.join("server.py"), b"installed\n").unwrap();
            }
            run(root.path(), &target, false).unwrap();
            let after = snapshot(root.path());
            assert_eq!(after.files["adapter.json"].mode, 0o600);
            assert!(!after.files.contains_key("pending.json"));
            assert_eq!(after.files["server.py"].mode, 0o600);
            assert_eq!(after.files["baseline/server.py"].mode, 0o600);
            assert_eq!(after.files["new.py"].mode, target.files["new.py"].mode);
            assert_eq!(
                after.files["baseline/new.py"].mode,
                target.files["baseline/new.py"].mode
            );
            let deletion = version(root.path(), "3.0.0", &[("server.py", None)]);
            run(root.path(), &deletion, false).unwrap();
            assert!(!snapshot(root.path()).files.contains_key("server.py"));
        }
    }

    #[test]
    fn permission_only_edits_survive_clean_upgrades_and_conflict_with_template_deletion() {
        let root = fixture();
        let adapter = root.path().join(DIRECTORY);
        std::fs::set_permissions(adapter.join("server.py"), Permissions::from_mode(0o755)).unwrap();
        let unchanged = version(root.path(), "2.0.0", &[]);
        run(root.path(), &unchanged, false).unwrap();
        assert_eq!(snapshot(root.path()).files["server.py"].mode, 0o755);

        let target = version(root.path(), "3.0.0", &[("server.py", None)]);
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("nothing written")
        );
        assert_eq!(snapshot(root.path()), before);
        assert!(
            run(root.path(), &target, true)
                .unwrap_err()
                .to_string()
                .contains("conflicts written")
        );
        let pending = snapshot(root.path());
        assert!(has_markers(pending.bytes("server.py").unwrap()));
        assert_eq!(pending.files["server.py"].mode, 0o755);
        assert_eq!(
            pending.bytes("adapter.json").unwrap(),
            before.bytes("adapter.json").unwrap()
        );
        std::fs::remove_file(adapter.join("server.py")).unwrap();
        run(root.path(), &target, false).unwrap();
        let after = snapshot(root.path());
        assert!(!after.files.contains_key("server.py"));
        assert!(!after.files.contains_key("baseline/server.py"));
        assert!(!after.files.contains_key("pending.json"));
    }

    #[test]
    fn additions_deletions_and_independent_additions_require_explicit_choices() {
        let root = fixture();
        let adapter = root.path().join(DIRECTORY);
        let target = version(
            root.path(),
            "2.0.0",
            &[
                ("server.py", None),
                ("new.py", Some(b"installed addition\n")),
                ("Dockerfile.rootfs", Some(b"changed recipe\n")),
            ],
        );
        std::fs::write(adapter.join("server.py"), b"customer modification\n").unwrap();
        std::fs::write(adapter.join("new.py"), b"customer addition\n").unwrap();
        std::fs::remove_file(adapter.join("Dockerfile.rootfs")).unwrap();
        let before = snapshot(root.path());
        assert!(run(root.path(), &target, false).is_err());
        assert_eq!(snapshot(root.path()), before);
        assert!(run(root.path(), &target, true).is_err());
        let pending = snapshot(root.path());
        for path in ["server.py", "new.py", "Dockerfile.rootfs"] {
            assert!(has_markers(pending.bytes(path).unwrap()), "{path}");
        }
        std::fs::remove_file(adapter.join("server.py")).unwrap();
        std::fs::remove_file(adapter.join("Dockerfile.rootfs")).unwrap();
        std::fs::write(adapter.join("new.py"), b"customer addition\n").unwrap();
        run(root.path(), &target, false).unwrap();
        let after = snapshot(root.path());
        assert!(!after.files.contains_key("server.py"));
        assert!(!after.files.contains_key("baseline/server.py"));
        assert!(!after.files.contains_key("Dockerfile.rootfs"));
        assert_eq!(
            after.bytes("baseline/Dockerfile.rootfs").unwrap(),
            b"changed recipe\n"
        );
        assert_eq!(after.bytes("new.py").unwrap(), b"customer addition\n");
    }

    #[test]
    fn unchanged_target_preserves_customer_deletion_and_binary_edits() {
        let root = fixture();
        std::fs::remove_file(root.path().join(DIRECTORY).join("server.py")).unwrap();
        std::fs::write(
            root.path().join(DIRECTORY).join("lampyr_types.py"),
            b"\0binary customer",
        )
        .unwrap();
        let target = version(root.path(), "2.0.0", &[]);
        run(root.path(), &target, false).unwrap();
        let after = snapshot(root.path());
        assert!(!after.files.contains_key("server.py"));
        assert_eq!(
            after.bytes("lampyr_types.py").unwrap(),
            b"\0binary customer"
        );
    }

    #[test]
    fn corrupt_state_and_path_collisions_never_install_partial_updates() {
        let root = fixture();
        let target = version(root.path(), "2.0.0", &[("nested/file", Some(b"new\n"))]);
        std::fs::write(
            root.path().join(DIRECTORY).join("nested"),
            "customer collision",
        )
        .unwrap();
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        assert_eq!(snapshot(root.path()), before);
        std::fs::write(
            root.path().join(DIRECTORY).join("baseline/server.py"),
            "tampered",
        )
        .unwrap();
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("baseline was modified")
        );
        assert_eq!(snapshot(root.path()), before);
    }

    #[test]
    fn concurrent_edits_and_exchange_failures_leave_metadata_unadvanced() {
        let root = fixture();
        let (parent, adapter) = open_adapter(root.path()).unwrap();
        let before = Snapshot::read(&adapter).unwrap();
        let target = version(root.path(), "2.0.0", &[]);
        std::fs::write(
            root.path().join(DIRECTORY).join("server.py"),
            "concurrent edit",
        )
        .unwrap();
        let edited = snapshot(root.path());
        assert!(
            install(
                root.path(),
                &parent,
                &adapter,
                &before,
                &target.files,
                &read_manifest(&parent).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("concurrently")
        );
        assert_eq!(snapshot(root.path()), edited);
        assert!(exchange(&parent, "not-a-staging-directory").is_err());
        assert_eq!(snapshot(root.path()), edited);
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lampyr-upgrade-")
        }));
    }

    #[test]
    fn staging_cleanup_retains_replaced_directory_instead_of_deleting_it() {
        let root = tempfile::tempdir().unwrap();
        let parent = File::open(root.path()).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix(".lampyr-upgrade-")
            .tempdir_in(root.path())
            .unwrap();
        let mut staging = StagingDir::new(temporary, &parent).unwrap();
        let path = staging.path().to_owned();
        let moved = root.path().join(".moved-staging");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("customer-data"), "must remain").unwrap();

        let error = staging.cleanup().unwrap_err().to_string();
        assert!(error.contains("ownership changed"));
        assert_eq!(
            std::fs::read(path.join("customer-data")).unwrap(),
            b"must remain"
        );
        assert!(moved.is_dir());
    }

    #[test]
    fn post_exchange_sync_failures_roll_back_or_retain_recovery_files() {
        for scenario in 0..5 {
            let root = fixture();
            let (parent, _) = open_adapter(root.path()).unwrap();
            let before = snapshot(root.path());
            let target = version(root.path(), "2.0.0", &[]);
            let temporary = tempfile::Builder::new()
                .prefix(".lampyr-upgrade-")
                .tempdir_in(root.path())
                .unwrap();
            let mut staging = StagingDir::new(temporary, &parent).unwrap();
            for (path, source) in &target.files {
                let path = staging.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, &source.bytes).unwrap();
            }
            let recovery = staging.path().to_owned();
            let staged = File::open(&recovery).unwrap();
            let installed = Snapshot::read(&staged).unwrap();
            staging.expect(installed.clone());
            exchange(&parent, recovery.file_name().unwrap().to_str().unwrap()).unwrap();
            staging.expect(before.clone());
            if scenario == 3 {
                std::fs::write(
                    root.path().join(DIRECTORY).join("server.py"),
                    "concurrent edit",
                )
                .unwrap();
            }
            let calls = std::cell::Cell::new(0);
            let error = finish_install(
                &parent,
                staging,
                &installed,
                |_| {
                    let call = calls.get();
                    calls.set(call + 1);
                    if call == 0 || scenario == 2 {
                        if scenario == 4 && call == 0 {
                            std::fs::rename(
                                root.path().join(DIRECTORY),
                                root.path().join(".lampyr-replaced-adapter"),
                            )?;
                            std::fs::create_dir(root.path().join(DIRECTORY))?;
                            std::fs::write(
                                root.path().join(DIRECTORY).join("replacement"),
                                "concurrent replacement",
                            )?;
                        }
                        Err(std::io::Error::from_raw_os_error(libc::EIO))
                    } else {
                        Ok(())
                    }
                },
                |parent, name| {
                    ensure!(scenario != 1, "injected rollback failure");
                    exchange(parent, name)
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("sync service directory after adapter install failed"));
            if scenario == 4 {
                assert!(error.contains("cannot safely roll back"));
                assert!(recovery.exists());
                assert_eq!(
                    std::fs::read(root.path().join(DIRECTORY).join("replacement")).unwrap(),
                    b"concurrent replacement"
                );
                assert_eq!(
                    Snapshot::read(&File::open(&recovery).unwrap()).unwrap(),
                    before
                );
                continue;
            }
            if scenario == 0 || scenario == 2 {
                assert_eq!(snapshot(root.path()), before);
                assert!(error.contains("installation rolled back"));
            } else {
                assert_eq!(
                    Snapshot::read(&File::open(&recovery).unwrap()).unwrap(),
                    before
                );
                assert!(error.contains("previous adapter retained"));
            }
            if scenario == 0 {
                assert!(!recovery.exists());
            } else {
                assert!(recovery.exists());
                assert!(error.contains(recovery.file_name().unwrap().to_str().unwrap()));
            }
        }
    }

    #[test]
    fn mid_staging_failure_never_advances_pristine_state_or_leaves_partial_files() {
        let root = fixture();
        let oversized = vec![b'x'; MAX_FILE as usize + 1];
        let target = version(root.path(), "2.0.0", &[("zz-last", Some(&oversized))]);
        let before = snapshot(root.path());
        assert!(
            run(root.path(), &target, false)
                .unwrap_err()
                .to_string()
                .contains("file too large")
        );
        assert_eq!(snapshot(root.path()), before);
        assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lampyr-upgrade-")
        }));
    }

    #[test]
    fn replaced_adapter_directory_and_root_permission_changes_are_rejected() {
        let root = fixture();
        let (parent, adapter) = open_adapter(root.path()).unwrap();
        let before = Snapshot::read(&adapter).unwrap();
        let target = version(root.path(), "2.0.0", &[]);
        std::fs::set_permissions(root.path().join(DIRECTORY), Permissions::from_mode(0o700))
            .unwrap();
        let changed = snapshot(root.path());
        assert!(
            install(
                root.path(),
                &parent,
                &adapter,
                &before,
                &target.files,
                &read_manifest(&parent).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("concurrently")
        );
        assert_eq!(snapshot(root.path()), changed);
        std::fs::rename(root.path().join(DIRECTORY), root.path().join("previous")).unwrap();
        std::fs::create_dir(root.path().join(DIRECTORY)).unwrap();
        let changed = snapshot(root.path());
        assert!(
            install(
                root.path(),
                &parent,
                &adapter,
                &before,
                &target.files,
                &read_manifest(&parent).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("directory changed concurrently")
        );
        assert_eq!(snapshot(root.path()), changed);
    }

    #[test]
    fn baseline_paths_and_pending_state_are_strict_and_bounded() {
        let root = fixture();
        let mut metadata = snapshot(root.path()).metadata().unwrap();
        for path in [
            "../escape",
            "/absolute",
            "a//b",
            "a/./b",
            "a\\b",
            "baseline/x",
            "pending.json/x",
            "pending-baseline/x",
        ] {
            metadata.files.insert(path.into(), digest(b"x"));
            assert!(validate_metadata(&metadata, "python").is_err(), "{path}");
            metadata.files.remove(path);
        }
        metadata
            .files
            .insert("server.py/child".into(), digest(b"x"));
        assert!(validate_metadata(&metadata, "python").is_err());
        assert!(decode::<Pending>(&vec![b' '; MAX_METADATA + 1], "pending.json").is_err());
        let duplicate = br#"{"schemaVersion":"lampyr.dev/runtime-adapter/v1alpha1","runtime":"python","templateVersion":"0.1.0","entrypoint":"/python","files":{"server.py":"first","server.py":"second"}}"#;
        assert!(
            format!(
                "{:#}",
                decode::<AdapterMetadata>(duplicate, "adapter.json").unwrap_err()
            )
            .contains("duplicate adapter path")
        );
        std::fs::write(root.path().join(DIRECTORY).join("pending.json"), "{}").unwrap();
        assert!(metadata_for_test(root.path()).is_err());
    }

    #[test]
    fn marker_detection_ignores_plain_equal_sign_lines() {
        assert!(!has_markers(
            b"value = \"\"\"\n=======\n\"\"\"\n\n# Markdown\n=======\n"
        ));
        assert!(has_markers(
            b"<<<<<<< customer\r\ncustomer\r\n||||||| pristine\r\nbase\r\n=======\r\ninstalled\r\n>>>>>>> installed\r\n"
        ));
        assert!(has_markers(
            b"<<<<<<< customer\ncustomer\n||||||| pristine\nbase\n=======\ninstalled\n>>>>>>> installed\n"
        ));
    }

    fn metadata_for_test(root: &Path) -> Result<AdapterMetadata> {
        metadata(root, "python")
    }

    #[test]
    fn symlinks_hardlinks_and_special_files_are_rejected_without_following_them() {
        let root = fixture();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("outside");
        std::fs::write(&external, b"outside").unwrap();
        let adapter = root.path().join(DIRECTORY);
        std::os::unix::fs::symlink(&external, adapter.join("link")).unwrap();
        assert!(
            open_adapter(root.path())
                .and_then(|(_, a)| Snapshot::read(&a))
                .is_err()
        );
        std::fs::remove_file(adapter.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), adapter.join("directory-link")).unwrap();
        assert!(
            open_adapter(root.path())
                .and_then(|(_, a)| Snapshot::read(&a))
                .is_err()
        );
        std::fs::remove_file(adapter.join("directory-link")).unwrap();
        std::fs::hard_link(&external, adapter.join("hardlink")).unwrap();
        assert!(
            open_adapter(root.path())
                .and_then(|(_, a)| Snapshot::read(&a))
                .is_err()
        );
        std::fs::remove_file(adapter.join("hardlink")).unwrap();
        let fifo = CString::new(adapter.join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            open_adapter(root.path())
                .and_then(|(_, a)| Snapshot::read(&a))
                .is_err()
        );
        assert_eq!(std::fs::read(&external).unwrap(), b"outside");
    }
}
