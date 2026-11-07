// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::Serialize;

use crate::runtime::PAYLOAD_LIMIT;

const CHUNK_SIZE: usize = 8 * 1024;
const STATE_LIMIT: usize = 512 * 1024;
const FILE_LIMIT: u64 = 8 * 1024 * 1024;

pub struct Directory {
    root: PathBuf,
    directory: Dir,
}

#[derive(Serialize)]
pub struct Chunk {
    pub data: String,
    pub next: u64,
    pub eof: bool,
}

#[derive(Serialize)]
pub struct Entry {
    pub path: String,
    pub kind: &'static str,
}

#[derive(Serialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub next: Option<usize>,
}

impl Directory {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize().context("resolve workspace")?;
        let directory =
            Dir::open_ambient_dir(&root, ambient_authority()).context("open scoped workspace")?;
        Ok(Self { root, directory })
    }

    pub fn private(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(root)
            .context("create private installation directory")?;
        ensure!(
            std::fs::symlink_metadata(root)?.is_dir(),
            "private installation directory must not be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .context("protect installation directory")?;
        }
        Self::open(root)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn read_text(&self, path: &str, limit: usize) -> Result<String> {
        relative_path(path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = self.directory.open_with(path, &options)?;
        ensure!(
            file.metadata()?.is_file(),
            "scoped read requires a regular file"
        );
        let mut text = String::new();
        file.take((limit + 1) as u64).read_to_string(&mut text)?;
        ensure!(
            text.len() <= limit,
            "scoped file exceeds its transport limit"
        );
        Ok(text)
    }

    pub fn write_private(&self, path: &str, content: &str) -> Result<()> {
        relative_path(path)?;
        ensure!(
            content.len() <= PAYLOAD_LIMIT / 2,
            "private scratch file exceeds 32 KiB"
        );
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            self.directory.create_dir_all(parent)?;
        }
        match self.directory.symlink_metadata(path) {
            Ok(metadata) => ensure!(
                metadata.is_file(),
                "private scratch target must be a regular file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect private scratch file"),
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = self.directory.open_with(path, &options)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn read(&self, path: &str, offset: u64) -> Result<Chunk> {
        relative_path(path)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let mut file = self
            .directory
            .open_with(path, &options)
            .with_context(|| format!("read workspace file {path}"))?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "workspace reads require a regular file");
        ensure!(metadata.len() <= FILE_LIMIT, "workspace file exceeds 8 MiB");
        ensure!(
            offset <= metadata.len(),
            "workspace read offset is beyond EOF"
        );
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(CHUNK_SIZE as u64).read_to_end(&mut bytes)?;
        let next = offset + bytes.len() as u64;
        Ok(Chunk {
            data: STANDARD.encode(&bytes),
            next,
            eof: next >= metadata.len(),
        })
    }

    pub fn list(&self, path: &str, cursor: usize) -> Result<Listing> {
        let directory = if path.is_empty() {
            self.directory.try_clone()?
        } else {
            relative_path(path)?;
            self.directory.open_dir(path)?
        };
        let mut entries = Vec::new();
        for entry in directory.entries()? {
            let entry = entry?;
            ensure!(
                entries.len() < 1024,
                "workspace directory exceeds 1024 entries"
            );
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("workspace file name is not UTF-8"))?;
            let kind = entry.file_type()?;
            entries.push(Entry {
                path: if path.is_empty() {
                    name
                } else {
                    format!("{path}/{name}")
                },
                kind: if kind.is_dir() {
                    "directory"
                } else if kind.is_file() {
                    "file"
                } else {
                    "special"
                },
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        ensure!(cursor <= entries.len(), "invalid workspace listing cursor");
        let next = (cursor + 32).min(entries.len());
        Ok(Listing {
            next: (next < entries.len()).then_some(next),
            entries: entries.into_iter().skip(cursor).take(32).collect(),
        })
    }

    pub fn scaffold(&self, files: &BTreeMap<String, String>) -> Result<()> {
        ensure!(
            !files.is_empty() && files.len() <= 64,
            "scaffold requires 1 to 64 files"
        );
        ensure!(
            serde_json::to_vec(files)?.len() <= PAYLOAD_LIMIT,
            "scaffold exceeds 64 KiB"
        );
        for path in files.keys() {
            relative_path(path)?;
            for parent in Path::new(path).ancestors().skip(1) {
                ensure!(
                    !files.contains_key(parent.to_str().context("invalid scaffold parent path")?),
                    "scaffold file is also a parent directory: {}",
                    parent.display()
                );
            }
            match self.directory.symlink_metadata(path) {
                Ok(_) => bail!("workspace file {path} already exists"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("check scaffold destination"),
            }
        }
        for (path, contents) in files {
            if let Some(parent) = Path::new(path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                self.directory.create_dir_all(parent)?;
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o644);
            }
            let mut file = self
                .directory
                .open_with(path, &options)
                .with_context(|| format!("create workspace file {path} without overwriting"))?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
        }
        Ok(())
    }

    pub fn read_state(&self, key: &str) -> Result<Option<serde_json::Value>> {
        state_key(key)?;
        let file = match self.directory.open(format!("{key}.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read installation state"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "installation state must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take((STATE_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= STATE_LIMIT,
            "installation state exceeds 512 KiB"
        );
        Ok(Some(
            serde_json::from_slice(&bytes).context("decode installation state")?,
        ))
    }

    pub fn write_state(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        state_key(key)?;
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() <= STATE_LIMIT,
            "installation state exceeds 512 KiB"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(format!("{key}.json")))
            .map_err(|error| error.error)
            .context("persist installation state")?;
        std::fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

pub fn relative_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 1024,
        "invalid workspace path"
    );
    ensure!(
        !path.contains('\\')
            && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
            && Path::new(path).components().all(|part| {
                matches!(part, Component::Normal(name) if name != ".git" && name != ".hg" && name != ".svn")
            }),
        "workspace path must stay beneath its authorized directory"
    );
    Ok(())
}

fn state_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 64
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid installation state key"
    );
    Ok(())
}
