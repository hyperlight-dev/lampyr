import errno
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import permissions  # noqa: E402


def account(name, gid, uid=None):
    return types.SimpleNamespace(pw_name=name, pw_gid=gid, pw_uid=os.geteuid() if uid is None else uid)


def group(name, members=(), gid=1000):
    return types.SimpleNamespace(gr_name=name, gr_mem=list(members), gr_gid=gid)


class PrivateGroupTests(unittest.TestCase):
    def test_primary_group_is_private_only_when_it_is_the_users_alone(self):
        dan = account("dan", 1000)
        cases = [
            ("private", dan, [group("dan")], [], True),
            ("lists only the user", dan, [group("dan", ["dan"])], [], True),
            ("other name", dan, [group("staff")], [], False),
            ("extra member", dan, [group("dan", ["dan", "eve"])], [], False),
            ("no passwd entry", None, [group("dan")], [], False),
            ("no group entry", dan, [], [], False),
            ("passwd names another gid", account("dan", 1001), [group("dan")], [], False),
            ("a second entry for the gid", dan, [group("dan"), group("shared", ["eve"])], [], False),
            ("another account's primary gid", dan, [group("dan")], ["eve"], False),
        ]
        for name, user, groups, sharing, private in cases:
            with self.subTest(name):
                if private:
                    self.assertEqual(permissions.private_group(1000, user, groups, sharing), 1000)
                else:
                    with self.assertRaises(ValueError):
                        permissions.private_group(1000, user, groups, sharing)

    def test_group_write_needs_the_private_group_and_no_acl(self):
        self.assertIsNone(permissions.group_write(1000, 1000, False))
        self.assertIn("ACL", permissions.group_write(1000, 1000, True))
        self.assertIn("gid 1001", permissions.group_write(1001, 1000, False))
        shared = ValueError("your primary group 'staff' isn't named after you")
        self.assertIn("staff", permissions.group_write(1000, shared, False))

    def test_databases_are_scanned_for_duplicates(self):
        """A duplicate group line or a second account on the gid defeats the
        exemption, even though getgrgid would return only the first entry."""
        permissions.user_private_group.cache_clear()
        self.addCleanup(permissions.user_private_group.cache_clear)
        egid = os.getegid()
        dan = account("dan", egid)
        databases = [
            ([group("dan", gid=egid)], [dan], None),
            ([group("dan", gid=egid), group("shared", ["eve"], gid=egid)], [dan], "2 group entries"),
            ([group("dan", gid=egid)], [dan, account("eve", egid, uid=os.geteuid() + 1)], "'eve'"),
            # dan:x:1001:1000, the user's name with another uid.
            ([group("dan", gid=egid)], [dan, account("dan", egid, uid=os.geteuid() + 1)], "another account"),
        ]
        for groups, accounts, problem in databases:
            with self.subTest(problem=problem):
                permissions.user_private_group.cache_clear()
                with mock.patch.object(permissions, "read_nsswitch", return_value=None), \
                     mock.patch("pwd.getpwuid", return_value=dan), \
                     mock.patch("pwd.getpwall", return_value=accounts), \
                     mock.patch("grp.getgrall", return_value=groups):
                    result = permissions.user_private_group()
                if problem is None:
                    self.assertEqual(result, egid)
                else:
                    self.assertIsInstance(result, ValueError)
                    self.assertIn(problem, str(result))

    def test_other_accounts_are_told_apart_by_uid(self):
        self.assertEqual(permissions.sharing_accounts(1000, 1000, "dan", [account("dan", 1000, 1000)]), [])
        self.assertEqual(permissions.sharing_accounts(1000, 1000, "dan", [account("eve", 1000, 1001)]), ["eve"])
        with self.assertRaisesRegex(ValueError, "uid 1001"):
            permissions.sharing_accounts(1000, 1000, "dan", [account("dan", 1000, 1001)])
        self.assertEqual(permissions.sharing_accounts(1000, 1000, "dan", [account("eve", 1001, 1001)]), [])

    def test_only_local_nss_sources_are_trusted(self):
        permissions.nss_sources_are_local(None)
        permissions.nss_sources_are_local("passwd: files systemd\ngroup: files systemd\n")
        permissions.nss_sources_are_local(
            "# comment\npasswd:  files [NOTFOUND=return] systemd # trailing\ngroup: files\n")
        permissions.nss_sources_are_local("hosts: dns\n")
        for conf in ("passwd: files sss\ngroup: files\n", "passwd: files\ngroup: files ldap\n",
                     "passwd: compat\ngroup: compat\n", "group: files winbind\n",
                     "passwd: files ] ldap\ngroup: files\n"):
            with self.subTest(conf=conf), self.assertRaises(ValueError):
                permissions.nss_sources_are_local(conf)

    def test_only_a_missing_nsswitch_conf_counts_as_files(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertIsNone(permissions.read_nsswitch(os.path.join(directory, "missing")))
            with self.assertRaisesRegex(ValueError, "couldn't be read"):
                permissions.read_nsswitch(directory)
        permissions.user_private_group.cache_clear()
        self.addCleanup(permissions.user_private_group.cache_clear)
        with mock.patch("builtins.open", side_effect=PermissionError(13, "denied")):
            result = permissions.user_private_group()
        self.assertIsInstance(result, ValueError)
        self.assertIn("couldn't be read", str(result))

    def test_a_directory_source_grants_no_exemption(self):
        permissions.user_private_group.cache_clear()
        self.addCleanup(permissions.user_private_group.cache_clear)
        with mock.patch.object(permissions, "read_nsswitch", return_value="passwd: files sss\ngroup: files sss\n"):
            result = permissions.user_private_group()
        self.assertIsInstance(result, ValueError)
        self.assertIn("sss", str(result))

    def test_lookup_failures_grant_no_exemption(self):
        permissions.user_private_group.cache_clear()
        self.addCleanup(permissions.user_private_group.cache_clear)
        with mock.patch.object(permissions, "read_nsswitch", return_value=None), \
             mock.patch("pwd.getpwuid", side_effect=KeyError), \
             mock.patch("pwd.getpwall", return_value=[]), \
             mock.patch("grp.getgrall", return_value=[group("dan", gid=os.getegid())]):
            self.assertIsInstance(permissions.user_private_group(), ValueError)

    def test_mode_and_acl_decide_with_the_real_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            path.chmod(0o707)
            self.assertIn("every user", permissions.others_can_write(path, path.lstat()))
            path.chmod(0o700)
            self.assertIsNone(permissions.others_can_write(path, path.lstat()))
            path.chmod(0o770)
            metadata = path.lstat()
            with mock.patch.object(permissions, "user_private_group", return_value=metadata.st_gid), \
                 mock.patch.object(permissions, "may_have_acl", return_value=False):
                self.assertIsNone(permissions.others_can_write(path, metadata))
            with mock.patch.object(permissions, "user_private_group", return_value=metadata.st_gid), \
                 mock.patch.object(permissions, "may_have_acl", return_value=True):
                self.assertIn("ACL", permissions.others_can_write(path, metadata))

    def test_real_private_group_and_acl(self):
        """Runs the real group lookup and ACL check, unmocked."""
        if not sys.platform.startswith("linux"):
            return  # Other systems grant no exemption; may_have_acl says so.
        import grp
        import pwd

        permissions.user_private_group.cache_clear()
        self.addCleanup(permissions.user_private_group.cache_clear)
        # permissions' own rule on the real databases, so a regression in the
        # rule shows up here too.
        egid, euid = os.getegid(), os.geteuid()
        user = pwd.getpwuid(euid)
        try:
            permissions.nss_sources_are_local(permissions.read_nsswitch())
            groups = [entry for entry in grp.getgrall() if entry.gr_gid == egid]
            sharing = permissions.sharing_accounts(euid, egid, user.pw_name, pwd.getpwall())
            permissions.private_group(egid, user, groups, sharing)
            private = True
        except ValueError:
            private = False
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            path.chmod(0o770)
            local = permissions.filesystem_type(path) in permissions.LOCAL_FILESYSTEMS
            reason = permissions.others_can_write(path, path.lstat())
            if private and local:
                self.assertIsNone(reason)
            else:
                self.assertIsNotNone(reason)
            setfacl = shutil.which("setfacl")
            if setfacl is None:
                return
            result = subprocess.run([setfacl, "-m", "u:nobody:rwx", str(path)], capture_output=True)
            if result.returncode != 0:
                return  # The filesystem doesn't support ACLs.
            self.assertIn("ACL", permissions.others_can_write(path, path.lstat()))

    def test_acl_lookup_fails_closed(self):
        with mock.patch.object(permissions, "filesystem_type", return_value=0xEF53):
            with mock.patch.object(os, "listxattr", side_effect=OSError(errno.EACCES, "denied"), create=True):
                self.assertTrue(permissions.may_have_acl("/anywhere"))
            with mock.patch.object(os, "listxattr", side_effect=OSError(errno.ENOTSUP, "no"), create=True):
                self.assertFalse(permissions.may_have_acl("/anywhere"))
            with mock.patch.object(os, "listxattr", return_value=[permissions.ACCESS_ACL], create=True):
                self.assertTrue(permissions.may_have_acl("/anywhere"))

    def test_filesystem_is_checked_before_the_databases(self):
        if not sys.platform.startswith("linux"):
            return
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            path.chmod(0o770)
            with mock.patch.object(permissions, "may_have_acl", return_value=True), \
                 mock.patch.object(permissions, "user_private_group") as lookup:
                self.assertIn("ACL", permissions.others_can_write(path, path.lstat()))
            lookup.assert_not_called()

    def test_network_filesystems_grant_no_exemption(self):
        # NFS reports EOPNOTSUPP for the ACL attribute, which looks like "no ACL".
        with mock.patch.object(permissions, "filesystem_type", return_value=0x6969), \
             mock.patch.object(os, "listxattr", side_effect=OSError(errno.EOPNOTSUPP, "no"), create=True):
            self.assertTrue(permissions.may_have_acl("/mnt/nfs"))
        with mock.patch.object(permissions, "filesystem_type", return_value=None):
            self.assertTrue(permissions.may_have_acl("/anywhere"))

    def test_local_filesystem_type_is_read(self):
        if not sys.platform.startswith("linux"):
            return
        with tempfile.TemporaryDirectory() as directory:
            self.assertIsInstance(permissions.filesystem_type(directory), int)


if __name__ == "__main__":
    unittest.main()
