// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Decides whether a user other than a file's owner can write to it.
//!
//! Distributions that give every user a private group (Ubuntu and Debian,
//! with `USERGROUPS_ENAB`) default to umask 002, so every directory a user
//! creates is group-writable by a group that should contain only that user.
//! Group write access is accepted only in that case:
//!
//! - the file's group is the user's primary group;
//! - that gid has exactly one group entry, named after the user, listing no
//!   other members;
//! - no other account has that gid as its primary group;
//! - the file is on a local filesystem (ext4, XFS, Btrfs, tmpfs, overlayfs)
//!   and has no POSIX access ACL. With an ACL the group bits are only the
//!   ACL mask; network filesystems (NFSv4, CIFS, GPFS) can't report one.
//!
//! The exemption trusts the account and group databases, so it applies
//! only when nsswitch.conf reads both from local sources (`files`,
//! `systemd`). Directory sources such as LDAP, sssd, NIS or winbind may not
//! enumerate every account or member, and glibc skips a source that fails
//! partway through a walk, so with them group write is refused.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::OnceLock;

/// A passwd entry, as far as the decision needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Account {
    pub(crate) name: CString,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// A group entry, as far as the decision needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub(crate) name: CString,
    pub(crate) members: Vec<CString>,
}

/// Returns the user's primary gid when that group is private to the user,
/// or why it isn't. `groups` are every group entry with gid `egid`, and
/// `sharing` the other accounts whose primary gid is `egid`.
pub(crate) fn private_group(
    egid: u32,
    account: Option<&Account>,
    groups: &[Group],
    sharing: &[CString],
) -> Result<u32, String> {
    let account = account.ok_or("your passwd entry couldn't be looked up")?;
    if account.gid != egid {
        return Err(format!(
            "your passwd entry names gid {}, not your effective gid {egid}",
            account.gid
        ));
    }
    let group = match groups {
        [] => {
            return Err(format!(
                "your primary group (gid {egid}) couldn't be looked up"
            ));
        }
        [group] => group,
        _ => {
            return Err(format!(
                "gid {egid} appears in {} group entries",
                groups.len()
            ));
        }
    };
    if group.name != account.name {
        return Err(format!(
            "your primary group {:?} isn't named after you ({:?})",
            group.name, account.name
        ));
    }
    if let Some(member) = group.members.iter().find(|member| **member != account.name) {
        return Err(format!(
            "your primary group {:?} also lists {member:?}",
            group.name
        ));
    }
    if let Some(other) = sharing.first() {
        return Err(format!(
            "account {other:?} also has gid {egid} as its primary group"
        ));
    }
    Ok(egid)
}

/// The names of other accounts that share `egid` as their primary gid, or an
/// error when another uid uses the user's own name. Accounts are told apart
/// by uid: a second `dan` entry with another uid is another account.
pub(crate) fn sharing_accounts(
    euid: u32,
    egid: u32,
    own: &CString,
    accounts: &[Account],
) -> Result<Vec<CString>, String> {
    let mut sharing = Vec::new();
    for account in accounts.iter().filter(|account| account.uid != euid) {
        if account.name == *own {
            return Err(format!(
                "another account named {own:?} has uid {}",
                account.uid
            ));
        }
        if account.gid == egid {
            sharing.push(account.name.clone());
        }
    }
    Ok(sharing)
}

/// Reads nsswitch.conf: `None` only when it doesn't exist. Any other read
/// error fails, so an unreadable file isn't taken for glibc's default.
pub(crate) fn read_nsswitch(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{} couldn't be read: {error}", path.display())),
    }
}

/// NSS sources whose account and group lists are complete and local.
const SAFE_NSS_SOURCES: [&str; 2] = ["files", "systemd"];

/// Checks that nsswitch.conf (`None` when it doesn't exist, which glibc
/// treats as `files`) reads `passwd` and `group` only from local sources.
pub(crate) fn nss_sources_are_local(conf: Option<&str>) -> Result<(), String> {
    let Some(conf) = conf else {
        return Ok(());
    };
    for database in ["passwd", "group"] {
        let line = conf.lines().find_map(|line| {
            let line = line.split('#').next().unwrap_or_default().trim();
            line.strip_prefix(database)?.trim_start().strip_prefix(':')
        });
        let Some(line) = line else {
            continue;
        };
        // Sources are the words outside [STATUS=action] brackets.
        let mut depth = 0;
        let mut sources = String::new();
        for character in line.chars() {
            match character {
                '[' => depth += 1,
                // A stray ']' is part of a word, so the line is refused
                // rather than hiding the sources after it.
                ']' if depth > 0 => depth -= 1,
                _ if depth == 0 => sources.push(character),
                _ => {}
            }
        }
        if let Some(source) = sources
            .split_whitespace()
            .find(|source| !SAFE_NSS_SOURCES.contains(source))
        {
            return Err(format!(
                "nsswitch.conf reads {database} from {source:?}, which may not list every account"
            ));
        }
    }
    Ok(())
}

/// Decides whether group write access on a file with group `gid` is safe.
/// `unverifiable` is true when the file may have an ACL: it has one, or its
/// filesystem can't say.
pub(crate) fn group_write(
    gid: u32,
    private: &Result<u32, String>,
    unverifiable: bool,
) -> Result<(), String> {
    if unverifiable {
        return Err(
            "it has a POSIX ACL, or is on a filesystem where ACLs can't be checked, \
             so its group bits don't show who can write"
                .into(),
        );
    }
    let private = private.as_ref().map_err(Clone::clone)?;
    if gid != *private {
        return Err(format!(
            "its group (gid {gid}) isn't your private group (gid {private})"
        ));
    }
    Ok(())
}

/// Returns why a user other than the owner can write to `path`, or `None`.
pub(crate) fn others_can_write(path: &Path, metadata: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    let mode = metadata.mode();
    if mode & 0o002 != 0 {
        return Some("every user can write to it".into());
    }
    if mode & 0o020 == 0 {
        return None;
    }
    if !cfg!(target_os = "linux") {
        return Some("its group can write to it".into());
    }
    // The filesystem check is cheap and decides most network homes, so it
    // runs before the account and group lookups.
    let reason = if may_have_acl(path) {
        group_write(metadata.gid(), &Ok(metadata.gid()), true)
    } else {
        group_write(metadata.gid(), user_private_group(), false)
    };
    reason
        .err()
        .map(|reason| format!("its group can write to it and {reason}"))
}

fn user_private_group() -> &'static Result<u32, String> {
    static PRIVATE: OnceLock<Result<u32, String>> = OnceLock::new();
    PRIVATE.get_or_init(|| {
        // SAFETY: getegid and geteuid have no preconditions.
        let (egid, euid) = unsafe { (libc::getegid(), libc::geteuid()) };
        nss_sources_are_local(read_nsswitch(Path::new("/etc/nsswitch.conf"))?.as_deref())?;
        let account = lookup_account(euid);
        let groups = groups_with_gid(egid)
            .map_err(|error| format!("the group database couldn't be read: {error}"))?;
        let accounts = all_accounts()
            .map_err(|error| format!("the passwd database couldn't be read: {error}"))?;
        let sharing = match &account {
            Some(account) => sharing_accounts(euid, egid, &account.name, &accounts)?,
            None => Vec::new(),
        };
        private_group(egid, account.as_ref(), &groups, &sharing)
    })
}

fn lookup_account(uid: u32) -> Option<Account> {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        // SAFETY: passwd is plain data; getpwuid_r fills it.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: the buffer outlives every pointer stored in `entry`, and its length is passed.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer.len() < MAX_ENTRY_BUFFER {
            let larger = buffer.len() * 2;
            buffer.resize(larger, 0);
            continue;
        }
        if status != 0 || result.is_null() || entry.pw_name.is_null() {
            return None;
        }
        return Some(Account {
            // SAFETY: getpwuid_r succeeded, so pw_name is a NUL-terminated string in buffer.
            name: unsafe { CStr::from_ptr(entry.pw_name) }.to_owned(),
            uid: entry.pw_uid,
            gid: entry.pw_gid,
        });
    }
}

/// One step of a `getgrent_r`/`getpwent_r` walk. Only Linux walks the
/// databases; the tests run the walk everywhere.
#[cfg(any(target_os = "linux", test))]
pub(crate) enum Next<T> {
    /// An entry, or `None` for one that doesn't match.
    Entry(Option<T>),
    /// The walk is over.
    End,
    /// The entry doesn't fit the buffer (ERANGE); retry with a larger one.
    TooSmall,
    /// The database couldn't be read.
    Failed(i32),
}

/// The largest buffer a database walk grows to before giving up.
const MAX_ENTRY_BUFFER: usize = 16 << 20;

/// Walks a database with `next`, which fills one entry into the buffer it is
/// given. Only a clean end finishes the walk: an entry that doesn't fit is
/// retried with a larger buffer, and any other error fails the walk, so a
/// truncated scan can't hide a second group or account.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn scan<T>(mut next: impl FnMut(&mut [u8]) -> Next<T>) -> Result<Vec<T>, String> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut entries = Vec::new();
    loop {
        match next(&mut buffer) {
            Next::Entry(Some(entry)) => entries.push(entry),
            Next::Entry(None) => {}
            Next::End => return Ok(entries),
            Next::TooSmall if buffer.len() < MAX_ENTRY_BUFFER => {
                let larger = buffer.len() * 2;
                buffer.resize(larger, 0);
            }
            Next::TooSmall => {
                return Err(format!(
                    "an entry is larger than {} MiB",
                    MAX_ENTRY_BUFFER >> 20
                ));
            }
            Next::Failed(code) => return Err(std::io::Error::from_raw_os_error(code).to_string()),
        }
    }
}

/// Maps a `getgrent_r`/`getpwent_r` status to a walk step.
#[cfg(target_os = "linux")]
fn step<T>(status: i32, found: bool, entry: impl FnOnce() -> Option<T>) -> Next<T> {
    match status {
        0 if found => Next::Entry(entry()),
        0 | libc::ENOENT => Next::End,
        libc::ERANGE => Next::TooSmall,
        code => Next::Failed(code),
    }
}

/// Every group entry with `gid`. A second entry for the same gid would make
/// getgrgid's answer only the first of them.
#[cfg(target_os = "linux")]
fn groups_with_gid(gid: u32) -> Result<Vec<Group>, String> {
    // SAFETY: setgrent/getgrent_r/endgrent walk the group database; the
    // walk runs once, from the OnceLock initializer.
    unsafe { libc::setgrent() };
    let groups = scan(|buffer| {
        // SAFETY: group is plain data; getgrent_r fills it.
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: the buffer outlives every pointer stored in `entry`. On
        // ERANGE glibc steps back, so the retry reads the same entry.
        let status = unsafe {
            libc::getgrent_r(
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        step(status, !result.is_null(), || {
            if entry.gr_gid != gid || entry.gr_name.is_null() {
                return None;
            }
            let mut members = Vec::new();
            let mut member = entry.gr_mem;
            // SAFETY: getgrent_r succeeded, so gr_name and the
            // NULL-terminated gr_mem array point into buffer.
            unsafe {
                while !member.is_null() && !(*member).is_null() {
                    members.push(CStr::from_ptr(*member).to_owned());
                    member = member.add(1);
                }
                Some(Group {
                    name: CStr::from_ptr(entry.gr_name).to_owned(),
                    members,
                })
            }
        })
    });
    // SAFETY: closes the walk opened above.
    unsafe { libc::endgrent() };
    groups
}

/// Every passwd entry.
#[cfg(target_os = "linux")]
fn all_accounts() -> Result<Vec<Account>, String> {
    // SAFETY: as in groups_with_gid, for the passwd database.
    unsafe { libc::setpwent() };
    let names = scan(|buffer| {
        // SAFETY: passwd is plain data; getpwent_r fills it.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: the buffer outlives every pointer stored in `entry`.
        let status = unsafe {
            libc::getpwent_r(
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        step(status, !result.is_null(), || {
            (!entry.pw_name.is_null()).then(|| Account {
                // SAFETY: getpwent_r succeeded, so pw_name points into buffer.
                name: unsafe { CStr::from_ptr(entry.pw_name) }.to_owned(),
                uid: entry.pw_uid,
                gid: entry.pw_gid,
            })
        })
    });
    // SAFETY: closes the walk opened above.
    unsafe { libc::endpwent() };
    names
}

/// Other systems grant no exemption; `others_can_write` never asks.
#[cfg(not(target_os = "linux"))]
fn groups_with_gid(_gid: u32) -> Result<Vec<Group>, String> {
    Ok(Vec::new())
}

#[cfg(not(target_os = "linux"))]
fn all_accounts() -> Result<Vec<Account>, String> {
    Ok(Vec::new())
}

/// Filesystems that report POSIX ACLs through `system.posix_acl_access`.
#[cfg(target_os = "linux")]
const LOCAL_FILESYSTEMS: [u32; 5] = [
    0xEF53,      // ext2, ext3, ext4
    0x5846_5342, // XFS
    0x9123_683E, // Btrfs
    0x0102_1994, // tmpfs
    0x794C_7630, // overlayfs
];

/// Reports whether `path` may have a POSIX access ACL: it has one, it is on
/// a filesystem that can't report one, or the check failed.
#[cfg(target_os = "linux")]
fn may_have_acl(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: statfs is plain data that statfs fills.
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: path is NUL-terminated and filesystem is a valid out pointer.
    // f_type's width varies by architecture; the magic numbers are 32-bit.
    if unsafe { libc::statfs(path.as_ptr(), &mut filesystem) } != 0
        || !LOCAL_FILESYSTEMS.contains(&(filesystem.f_type as u32))
    {
        return true;
    }
    // SAFETY: both strings are NUL-terminated; a null buffer with size 0 only
    // asks for the attribute's length.
    let length = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if length >= 0 {
        return true;
    }
    !matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENODATA | libc::ENOTSUP)
    )
}

#[cfg(not(target_os = "linux"))]
fn may_have_acl(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, gid: u32) -> Account {
        Account {
            name: CString::new(name).unwrap(),
            uid: 1000,
            gid,
        }
    }

    fn group(name: &str, members: &[&str]) -> Group {
        Group {
            name: CString::new(name).unwrap(),
            members: members
                .iter()
                .map(|member| CString::new(*member).unwrap())
                .collect(),
        }
    }

    /// A name, the passwd entry, the gid's group entries, the other accounts
    /// sharing the gid, and whether the group counts as private.
    type Case<'a> = (
        &'a str,
        Option<&'a Account>,
        Vec<Group>,
        &'a [CString],
        bool,
    );

    #[test]
    fn primary_group_is_private_only_when_it_is_the_users_alone() {
        let dan = account("dan", 1000);
        let eve = vec![CString::new("eve").unwrap()];
        let cases: [Case<'_>; 9] = [
            ("private", Some(&dan), vec![group("dan", &[])], &[], true),
            (
                "lists only the user",
                Some(&dan),
                vec![group("dan", &["dan"])],
                &[],
                true,
            ),
            (
                "other name",
                Some(&dan),
                vec![group("staff", &[])],
                &[],
                false,
            ),
            (
                "extra member",
                Some(&dan),
                vec![group("dan", &["dan", "eve"])],
                &[],
                false,
            ),
            ("no passwd entry", None, vec![group("dan", &[])], &[], false),
            ("no group entry", Some(&dan), vec![], &[], false),
            (
                "a second entry for the gid",
                Some(&dan),
                vec![group("dan", &[]), group("shared", &["eve"])],
                &[],
                false,
            ),
            (
                "another account's primary gid",
                Some(&dan),
                vec![group("dan", &[])],
                &eve,
                false,
            ),
            (
                "passwd names another gid",
                Some(&account("dan", 1001)),
                vec![group("dan", &[])],
                &[],
                false,
            ),
        ];
        for (name, account, groups, sharing, private) in cases {
            assert_eq!(
                private_group(1000, account, &groups, sharing).is_ok(),
                private,
                "{name}"
            );
        }
        let error = private_group(1000, Some(&dan), &[group("dan", &[])], &eve).unwrap_err();
        assert!(error.contains("eve"), "{error}");
    }

    #[test]
    fn other_accounts_are_told_apart_by_uid() {
        let dan = CString::new("dan").unwrap();
        let entry = |name: &str, uid, gid| Account {
            name: CString::new(name).unwrap(),
            uid,
            gid,
        };
        // The user's own entry never counts.
        assert!(
            sharing_accounts(1000, 1000, &dan, &[entry("dan", 1000, 1000)])
                .unwrap()
                .is_empty()
        );
        // Another uid on the same gid shares it.
        assert_eq!(
            sharing_accounts(1000, 1000, &dan, &[entry("eve", 1001, 1000)]).unwrap(),
            [CString::new("eve").unwrap()]
        );
        // dan:x:1001:1000, the user's name with another uid, is refused.
        assert!(
            sharing_accounts(1000, 1000, &dan, &[entry("dan", 1001, 1000)])
                .unwrap_err()
                .contains("uid 1001")
        );
        // Unrelated accounts don't matter.
        assert!(
            sharing_accounts(1000, 1000, &dan, &[entry("eve", 1001, 1001)])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_local_nss_sources_are_trusted() {
        assert!(nss_sources_are_local(None).is_ok());
        assert!(
            nss_sources_are_local(Some("passwd: files systemd\ngroup: files systemd\n")).is_ok()
        );
        assert!(
            nss_sources_are_local(Some(
                "# comment\npasswd:  files [NOTFOUND=return] systemd # trailing\ngroup: files\n"
            ))
            .is_ok()
        );
        assert!(nss_sources_are_local(Some("hosts: dns\n")).is_ok());
        for conf in [
            "passwd: files sss\ngroup: files\n",
            "passwd: files\ngroup: files ldap\n",
            "passwd: compat\ngroup: compat\n",
            "group: files winbind\n",
            "passwd: files ] ldap\ngroup: files\n",
        ] {
            assert!(nss_sources_are_local(Some(conf)).is_err(), "{conf}");
        }
    }

    #[test]
    fn only_a_missing_nsswitch_conf_counts_as_files() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(read_nsswitch(&directory.path().join("missing")), Ok(None));
        // A directory where the file should be is an error, not "missing".
        assert!(read_nsswitch(directory.path()).is_err());
        // Invalid UTF-8 is read, lossily, and still checked.
        let conf = directory.path().join("nsswitch.conf");
        std::fs::write(&conf, b"passwd: files \xff sss\n").unwrap();
        let text = read_nsswitch(&conf).unwrap();
        assert!(nss_sources_are_local(text.as_deref()).is_err());
    }

    #[test]
    fn a_database_walk_fails_closed() {
        // Entries, an entry too big for the first buffer, then the end.
        let mut steps = vec![
            Next::Entry(Some(1)),
            Next::TooSmall,
            Next::Entry(Some(2)),
            Next::Entry(None),
            Next::End,
        ]
        .into_iter();
        let mut sizes = Vec::new();
        let entries = scan(|buffer| {
            sizes.push(buffer.len());
            steps.next().unwrap()
        })
        .unwrap();
        assert_eq!(entries, [1, 2]);
        assert!(sizes[2] > sizes[1], "buffer sizes {sizes:?}");

        // An NSS error stops the walk with an error, not a short list.
        let mut steps = vec![Next::Entry(Some(1)), Next::Failed(libc::EIO)].into_iter();
        assert!(scan(|_| steps.next().unwrap()).is_err());

        // An entry that never fits is an error too.
        assert!(scan::<u32>(|_| Next::TooSmall).unwrap_err().contains("MiB"));
    }

    #[test]
    fn group_write_needs_the_private_group_and_no_acl() {
        let private = Ok(1000);
        assert!(group_write(1000, &private, false).is_ok());
        assert!(
            group_write(1000, &private, true)
                .unwrap_err()
                .contains("ACL")
        );
        assert!(
            group_write(1001, &private, false)
                .unwrap_err()
                .contains("gid 1001")
        );
        let shared = Err("your primary group \"staff\" isn't named after you".to_owned());
        assert!(
            group_write(1000, &shared, false)
                .unwrap_err()
                .contains("staff")
        );
    }

    #[test]
    fn other_write_is_never_accepted() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o702)).unwrap();
        let metadata = std::fs::metadata(path).unwrap();
        assert!(
            others_can_write(path, &metadata)
                .unwrap()
                .contains("every user")
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(others_can_write(path, &std::fs::metadata(path).unwrap()).is_none());
    }

    /// With an access ACL, the group bits are only the mask: `setfacl -m
    /// u:nobody:rwx` leaves the directory looking like mode 770.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_access_acl_is_never_exempt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        let status = std::process::Command::new("setfacl")
            .args(["-m", "u:nobody:rwx"])
            .arg(path)
            .status();
        match status {
            Ok(status) if status.success() => {}
            // Without setfacl or ACL support there is no ACL to detect.
            _ => return,
        }
        assert!(may_have_acl(path));
        let reason = others_can_write(path, &std::fs::metadata(path).unwrap()).unwrap();
        assert!(reason.contains("ACL"), "{reason}");
    }
}
