"""Add ilex's account and storage checks at Anaconda's validation boundaries."""

from pathlib import Path
import sysconfig


def insert(module, anchor, lines):
    path = Path(sysconfig.get_path("purelib")) / "pyanaconda" / module
    text = path.read_text()
    if text.count(anchor) != 1:
        raise SystemExit(f"Review the ilex validation hook for {module} after the Anaconda update")
    path.write_text(text.replace(anchor, lines + anchor))


# Keep the standard WebUI and account creation. This callback also covers
# kickstart accounts, including a conflicting username in a pre-answer.
insert("modules/users/users.py", "        self._users = users\n", """+        from pyanaconda.ilex_setup import validate_users
        validate_users(users)
""")
insert("modules/storage/checker/utils.py", "    check_empty = bool(root and", """+    from pyanaconda.ilex_setup import validate_storage
    validate_storage(storage, root, report_error)

""")
target = Path(sysconfig.get_path("purelib")) / "pyanaconda/ilex_setup.py"
target.write_bytes(Path("/usr/libexec/ilex-installer/anaconda-policy.py").read_bytes())
