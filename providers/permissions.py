"""Decide whether a user other than a file's owner can write to it.

Distributions that give every user a private group (Ubuntu and Debian, with
USERGROUPS_ENAB) default to umask 002, so every directory a user creates is
group-writable by a group that should contain only that user. Group write
access is accepted only in that case:

- the file's group is the user's primary group;
- that gid has exactly one group entry, named after the user, listing no
  other members;
- no other account has that gid as its primary group;
- the file is on a local filesystem (ext4, XFS, Btrfs, tmpfs, overlayfs) and
  has no POSIX access ACL. With an ACL the group bits are only the ACL mask;
  network filesystems (NFSv4, CIFS, GPFS) can't report one.

The exemption trusts the account and group databases, so it applies only when
nsswitch.conf reads both from local sources (files, systemd). Directory
sources such as LDAP, sssd, NIS or winbind may not enumerate every account or
member, and glibc skips a source that fails partway through a walk (Python's
getgrall and getpwall can't report that), so with them group write is
refused. This mirrors cli/src/permissions.rs.
"""

import ctypes
import ctypes.util
import errno
import functools
import os
import struct
import sys

ACCESS_ACL = "system.posix_acl_access"

# statfs f_type of filesystems that report POSIX ACLs through ACCESS_ACL.
LOCAL_FILESYSTEMS = frozenset({
    0xEF53,  # ext2, ext3, ext4
    0x58465342,  # XFS
    0x9123683E,  # Btrfs
    0x01021994,  # tmpfs
    0x794C7630,  # overlayfs
})


def private_group(egid, account, groups, sharing):
    """Return egid when that group is private to the user, else raise ValueError.

    account is the user's passwd entry, or None when it couldn't be looked
    up; groups are every group entry with gid egid; sharing are the names of
    other accounts whose primary gid is egid.
    """
    if account is None:
        raise ValueError("your passwd entry couldn't be looked up")
    if account.pw_gid != egid:
        raise ValueError(f"your passwd entry names gid {account.pw_gid}, not your effective gid {egid}")
    if not groups:
        raise ValueError(f"your primary group (gid {egid}) couldn't be looked up")
    if len(groups) > 1:
        raise ValueError(f"gid {egid} appears in {len(groups)} group entries")
    group = groups[0]
    if group.gr_name != account.pw_name:
        raise ValueError(f"your primary group {group.gr_name!r} isn't named after you ({account.pw_name!r})")
    for member in group.gr_mem:
        if member != account.pw_name:
            raise ValueError(f"your primary group {group.gr_name!r} also lists {member!r}")
    if sharing:
        raise ValueError(f"account {sharing[0]!r} also has gid {egid} as its primary group")
    return egid


SAFE_NSS_SOURCES = frozenset({"files", "systemd"})


def sharing_accounts(euid, egid, own, accounts):
    """Return the names of other accounts whose primary gid is egid.

    Accounts are told apart by uid, so a second entry with the user's name and
    another uid raises ValueError.
    """
    sharing = []
    for account in accounts:
        if account.pw_uid == euid:
            continue
        if account.pw_name == own:
            raise ValueError(f"another account named {own!r} has uid {account.pw_uid}")
        if account.pw_gid == egid:
            sharing.append(account.pw_name)
    return sharing


def nss_sources_are_local(conf):
    """Raise ValueError unless nsswitch.conf reads passwd and group only from
    local sources. conf is None when the file doesn't exist, which glibc
    treats as files."""
    if conf is None:
        return
    for database in ("passwd", "group"):
        for line in conf.splitlines():
            line = line.split("#", 1)[0].strip()
            name, colon, sources = line.partition(":")
            if not colon or name.strip() != database:
                continue
            depth, words = 0, []
            for character in sources:
                if character == "[":
                    depth += 1
                elif character == "]" and depth > 0:
                    depth -= 1
                elif depth == 0:
                    words.append(character)
            for source in "".join(words).split():
                if source not in SAFE_NSS_SOURCES:
                    raise ValueError(
                        f"nsswitch.conf reads {database} from {source!r}, which may not list every account")
            break


def read_nsswitch(path="/etc/nsswitch.conf"):
    """Return nsswitch.conf, or None only when it doesn't exist. Any other
    read error raises ValueError, so an unreadable file isn't taken for
    glibc's default."""
    try:
        with open(path, encoding="utf-8", errors="replace") as conf:
            return conf.read()
    except FileNotFoundError:
        return None
    except OSError as error:
        raise ValueError(f"nsswitch.conf couldn't be read: {error}") from error


def group_write(gid, private, unverifiable):
    """Return None when group write on a file with group gid is safe, else why not.

    private is the user's private gid, or the ValueError explaining why the
    user has none. unverifiable is true when the file may have an ACL.
    """
    if unverifiable:
        return ("it has a POSIX ACL, or is on a filesystem where ACLs can't be checked, "
                "so its group bits don't show who can write")
    if isinstance(private, ValueError):
        return str(private)
    if gid != private:
        return f"its group (gid {gid}) isn't your private group (gid {private})"
    return None


@functools.lru_cache(maxsize=None)
def user_private_group():
    import grp
    import pwd

    egid, euid = os.getegid(), os.geteuid()
    try:
        nss_sources_are_local(read_nsswitch())
        try:
            account = pwd.getpwuid(euid)
        except KeyError:
            account = None
        groups = [group for group in grp.getgrall() if group.gr_gid == egid]
        sharing = [] if account is None else sharing_accounts(euid, egid, account.pw_name, pwd.getpwall())
        return private_group(egid, account, groups, sharing)
    except ValueError as error:
        return error


@functools.lru_cache(maxsize=None)
def _libc():
    # find_library runs ldconfig, so look libc up once.
    return ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)


def filesystem_type(path):
    """Return path's statfs f_type, or None when it can't be read."""
    if not sys.platform.startswith("linux"):
        return None
    # struct statfs starts with f_type, a long on Linux; the rest of the
    # buffer is room for the other fields. The magic numbers are 32-bit.
    buffer = ctypes.create_string_buffer(256)
    if _libc().statfs(os.fsencode(path), buffer) != 0:
        return None
    return struct.unpack_from("L", buffer)[0] & 0xFFFFFFFF


def may_have_acl(path):
    """Report whether path may have a POSIX access ACL: it has one, its
    filesystem can't report one, or the check failed."""
    if not hasattr(os, "listxattr") or filesystem_type(path) not in LOCAL_FILESYSTEMS:
        return True
    try:
        return ACCESS_ACL in os.listxattr(path, follow_symlinks=False)
    except OSError as error:
        return error.errno not in (errno.ENOTSUP, errno.EOPNOTSUPP, errno.ENODATA)


def others_can_write(path, metadata):
    """Return why a user other than the owner can write to path, or None."""
    if metadata.st_mode & 0o002:
        return "every user can write to it"
    if not metadata.st_mode & 0o020:
        return None
    if not sys.platform.startswith("linux"):
        return "its group can write to it"
    # The filesystem check is cheap and decides most network homes, so it
    # runs before the account and group lookups.
    if may_have_acl(path):
        reason = group_write(metadata.st_gid, metadata.st_gid, True)
    else:
        reason = group_write(metadata.st_gid, user_private_group(), False)
    return None if reason is None else f"its group can write to it and {reason}"
