// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use flate2::{Compression, write::GzEncoder};
use lampyr_cli::platform;
use sha2::{Digest, Sha256};

#[test]
fn doctor_reports_build_commit_and_dirty_state() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ly"))
        .arg("doctor")
        .current_dir(root.path())
        .env("PATH", "")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let build = stdout
        .lines()
        .find(|line| line.contains("[ok] build"))
        .unwrap_or_else(|| panic!("doctor output has no build identity: {stdout}"));
    assert!(build.contains("commit "), "{build}");
    assert!(
        build.ends_with(", clean") || build.ends_with(", dirty") || build.ends_with(", unknown"),
        "{build}"
    );
}

#[test]
fn build_metadata_does_not_watch_missing_tracked_directories() {
    use std::process::Command;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let root = root.as_path();
    let output_dir = root.join("cli/target/debug/build/metadata/out");
    std::fs::create_dir_all(&output_dir).unwrap();
    std::fs::create_dir_all(root.join("docs/cli")).unwrap();
    std::fs::write(root.join("docs/cli/example.md"), "example").unwrap();
    std::fs::write(root.join("README.md"), "example").unwrap();
    std::fs::write(root.join("cli/example.rs"), "example").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "docs/cli/example.md", "README.md", "cli/example.rs"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    std::fs::remove_file(root.join("docs/cli/example.md")).unwrap();
    std::fs::remove_dir(root.join("docs/cli")).unwrap();
    std::fs::remove_file(root.join("README.md")).unwrap();
    std::fs::remove_file(root.join("cli/example.rs")).unwrap();
    let executable = root.join(format!("build-metadata{}", std::env::consts::EXE_SUFFIX));
    let compiled = Command::new("rustc")
        .args(["--edition=2024", "--crate-name=build_metadata"])
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{compiled:?}");
    let output = Command::new(executable)
        .env("CARGO_MANIFEST_DIR", root.join("cli"))
        .env("OUT_DIR", &output_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let watches: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("cargo:rerun-if-changed="))
        .map(std::path::PathBuf::from)
        .collect();
    assert!(!watches.is_empty(), "{stdout}");
    for path in &watches {
        assert!(path.exists(), "missing Cargo input: {}", path.display());
        assert!(
            !output_dir.starts_with(path),
            "Cargo input contains build outputs: {}",
            path.display()
        );
    }
    assert!(watches.contains(&root.join("docs")), "{stdout}");
}

fn archive(link: bool, duplicate: bool) -> Vec<u8> {
    let compressed = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(compressed);
    let mut directory = tar::Header::new_gnu();
    directory.set_entry_type(tar::EntryType::Directory);
    directory.set_mode(0o777);
    directory.set_size(0);
    directory.set_cksum();
    archive
        .append_data(&mut directory, "build", &b""[..])
        .unwrap();
    for _ in 0..if duplicate { 2 } else { 1 } {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o777);
        if link {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name("/etc/passwd").unwrap();
        } else {
            header.set_size(7);
        }

        header.set_cksum();
        let bytes = if link { &b""[..] } else { &b"payload"[..] };
        archive
            .append_data(&mut header, "build/runtime", bytes)
            .unwrap();
    }
    let mut config = tar::Header::new_gnu();
    config.set_mode(0o666);
    config.set_size(6);
    config.set_cksum();
    archive
        .append_data(&mut config, "build/config", &b"config"[..])
        .unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

#[test]
fn setup_under_group_umask_creates_a_loadable_provider_artifact() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::process::Command;

    use lampyr_cli::artifact::{Artifact, FileDigest, Manifest, Operation};

    if std::env::var_os("LAMPYR_TEST_GROUP_UMASK_CHILD").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "setup_under_group_umask_creates_a_loadable_provider_artifact",
            ])
            .env("LAMPYR_TEST_GROUP_UMASK_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let data = tempfile::tempdir().unwrap();
    let lampyr_home = data.path().parent().unwrap().join(format!(
        "lampyr-home-{}",
        data.path().file_name().unwrap().to_string_lossy()
    ));
    assert!(!lampyr_home.exists());
    #[cfg(unix)]
    unsafe {
        libc::umask(0o002);
    }

    let digest = |path: &str, content: &[u8]| FileDigest {
        path: path.to_owned(),
        sha256: Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    let rootfs = b"test rootfs";
    let program = b"# external provider\n";
    let manifest = serde_json::to_vec(&Manifest {
        protocol_version: 1,
        id: "test".to_owned(),
        version: "1".to_owned(),
        runtime_version: lampyr_cli::assets::VERSION.to_owned(),
        memory_mib: 256,
        rootfs: digest("rootfs.cpio", rootfs),
        kernel: None,
        cold_compatibility: None,
        program: digest("provider.py", program),
        snapshot_index: None,
        snapshot_repository: None,
        environment: BTreeMap::new(),
        network_hosts: Vec::new(),
        source_kinds: BTreeSet::new(),
        operations: BTreeMap::from([(Operation::Describe, BTreeSet::new())]),
    })
    .unwrap();
    let compressed = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(compressed);
    for (path, bytes) in [
        (".lampyr-platform", &b"test"[..]),
        ("k8s/fiberd-tenant.yaml", &b"manifest"[..]),
        ("providers/test/artifact/manifest.json", manifest.as_slice()),
        ("providers/test/artifact/rootfs.cpio", rootfs),
        ("providers/test/artifact/provider.py", program),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o664);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, path, bytes).unwrap();
    }
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    let checksum = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/platform.tar.gz", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (index, stream) in listener.incoming().take(2).enumerate() {
            let mut reader = BufReader::new(stream.unwrap());
            reader
                .get_mut()
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let body = if index == 0 {
                assert!(request.starts_with("GET /platform.tar.gz.sha256 "));
                checksum.as_bytes()
            } else {
                assert!(request.starts_with("GET /platform.tar.gz "));
                &bytes
            };
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            reader.get_mut().write_all(body).unwrap();
        }
    });
    let invoke = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ly"))
            .args(arguments)
            .env("LAMPYR_HOME", &lampyr_home)
            .env_remove("LAMPYR_PLATFORM_SHA256_URL")
            .output()
            .unwrap()
    };
    let installed = invoke(&["setup", "--version", "local-proof", "--url", &url]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let progress = String::from_utf8_lossy(&installed.stderr);
    assert!(progress.contains("  [ok] Preparing the Lampyr platform payload"));
    assert_eq!(
        progress
            .matches("Preparing the Lampyr platform payload")
            .count(),
        1
    );
    server.join().unwrap();
    assert!(
        invoke(&["setup", "--version", "local-proof", "--check"])
            .status
            .success()
    );
    assert!(
        invoke(&["setup", "--version", "local-proof"])
            .status
            .success()
    );
    let rejected = invoke(&["setup", "--version", "../escape", "--remove"]);
    assert!(rejected.status.code().is_some_and(|code| code != 0));
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("  [fail] Preparing the Lampyr platform payload")
    );
    assert!(
        lampyr_home
            .join("platforms/local-proof/k8s/fiberd-tenant.yaml")
            .is_file()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            lampyr_home.clone(),
            lampyr_home.join("platforms"),
            lampyr_home.join("platforms/local-proof"),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o022, 0, "{} has mode {mode:o}", path.display());
        }
    }
    Artifact::load(
        lampyr_home.join("platforms/local-proof/providers/test/artifact"),
        "test",
    )
    .unwrap();
    assert!(
        invoke(&["setup", "--version", "local-proof", "--remove"])
            .status
            .success()
    );
    assert!(!lampyr_home.join("platforms/local-proof").exists());
    // A platform from before fiberd-k8s-controller carries
    // k8s/grant-controller.yaml instead, which this ly cannot run, but must
    // still remove.
    let legacy = lampyr_home.join("platforms/legacy-proof");
    std::fs::create_dir_all(legacy.join("k8s")).unwrap();
    std::fs::write(legacy.join(".lampyr-platform"), "").unwrap();
    std::fs::write(legacy.join("k8s/grant-controller.yaml"), "manifest").unwrap();
    assert!(
        !invoke(&["setup", "--version", "legacy-proof", "--check"])
            .status
            .success()
    );
    let removed = invoke(&["setup", "--version", "legacy-proof", "--remove"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!legacy.exists());
    std::fs::remove_dir_all(&lampyr_home).unwrap();
}

#[test]
fn platform_extraction_rejects_links_and_duplicate_files_and_preserves_executable_permissions() {
    for (link, duplicate) in [(true, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        assert!(platform::extract(&archive(link, duplicate)[..], directory.path()).is_err());
    }
    let directory = tempfile::tempdir().unwrap();
    platform::extract(&archive(false, false)[..], directory.path()).unwrap();
    let file = directory.path().join("build/runtime");
    assert_eq!(std::fs::read(&file).unwrap(), b"payload");
    let config = directory.path().join("build/config");
    assert_eq!(std::fs::read(&config).unwrap(), b"config");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(directory.path().join("build"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[test]
fn platform_extraction_removes_group_write_permissions_from_provider_files() {
    if std::env::var_os("LAMPYR_TEST_GROUP_UMASK_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform_extraction_removes_group_write_permissions_from_provider_files",
            ])
            .env("LAMPYR_TEST_GROUP_UMASK_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::umask(0o002);
    }
    let directory = tempfile::tempdir().unwrap();
    let compressed = GzEncoder::new(Vec::new(), Compression::default());
    let mut archive = tar::Builder::new(compressed);
    let mut parent = tar::Header::new_gnu();
    parent.set_entry_type(tar::EntryType::Directory);
    parent.set_size(0);
    parent.set_mode(0o775);
    parent.set_cksum();
    archive
        .append_data(&mut parent, "providers", &b""[..])
        .unwrap();
    let mut file = tar::Header::new_gnu();
    file.set_size(7);
    file.set_mode(0o775);
    file.set_cksum();
    archive
        .append_data(&mut file, "providers/runtime", &b"payload"[..])
        .unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();

    platform::extract(&bytes[..], directory.path()).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            directory.path().join("providers"),
            directory.path().join("providers/runtime"),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o022,
                0
            );
        }
    }
}
