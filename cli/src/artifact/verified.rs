// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Memory images this process has already hashed, so repeated provider loads in one `ly` command
//! hash each multi-gigabyte image once. `Snapshot::checked_load` still hashes the handle it maps.

use std::collections::HashSet;
use std::sync::Mutex;

/// Identifies one version of a file. A write changes its ctime, which unprivileged users cannot set
/// back, so a modified image is hashed again. On kernels with coarse timestamps two writes in one
/// clock tick can share a ctime; `Snapshot::checked_load` still rejects such an image before boot.
#[derive(Hash, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    sha256: String,
}

static VERIFIED: Mutex<Option<HashSet<Identity>>> = Mutex::new(None);

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata, sha256: &str) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;
    Some(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        sha256: sha256.to_owned(),
    })
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata, _: &str) -> Option<Identity> {
    None
}

pub(crate) fn contains(metadata: &std::fs::Metadata, sha256: &str) -> bool {
    let Some(identity) = identity(metadata, sha256) else {
        return false;
    };
    VERIFIED
        .lock()
        .is_ok_and(|verified| verified.as_ref().is_some_and(|set| set.contains(&identity)))
}

pub(crate) fn insert(metadata: &std::fs::Metadata, sha256: &str) {
    if let (Some(identity), Ok(mut verified)) = (identity(metadata, sha256), VERIFIED.lock()) {
        verified.get_or_insert_with(HashSet::new).insert(identity);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write;

    #[test]
    fn a_rewritten_image_is_no_longer_verified() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("memory");
        std::fs::write(&path, b"verified memory").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(!super::contains(&metadata, "digest"));
        super::insert(&metadata, "digest");
        assert!(super::contains(
            &std::fs::metadata(&path).unwrap(),
            "digest"
        ));
        assert!(!super::contains(&metadata, "other-digest"));
        // Same size, same inode: only the timestamps tell this write apart.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"tampered memory").unwrap();
        drop(file);
        assert!(!super::contains(
            &std::fs::metadata(&path).unwrap(),
            "digest"
        ));
    }
}
