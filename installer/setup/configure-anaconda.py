"""Add ilex's account and storage checks at Anaconda's validation boundaries."""

from pathlib import Path
import sysconfig

PACKAGE = Path(sysconfig.get_path("platlib", vars={"platbase": "/usr"})) / "pyanaconda"


def patch(module, anchor, replacement):
    path = PACKAGE / module
    text = path.read_text()
    if text.count(anchor) != 1:
        raise SystemExit(f"Review the ilex validation hook for {module} after the Anaconda update")
    updated = text.replace(anchor, replacement)
    compile(updated, str(path), "exec")
    path.write_text(updated)


# Keep the standard WebUI and account creation. This callback also covers
# kickstart accounts, including a conflicting username in a pre-answer.
patch("modules/users/users.py", "        self._users = users\n",
      "        from pyanaconda.ilex_setup import validate_users\n"
      "        validate_users(users)\n"
      "        self._users = users\n")
patch("modules/users/users_interface.py",
      "        self.implementation.set_users(UserData.from_structure_list(users))\n",
      "        from pyanaconda.ilex_setup import validate_maintenance\n"
      "        accounts = UserData.from_structure_list(users)\n"
      "        validate_maintenance(accounts)\n"
      "        self.implementation.set_users(accounts)\n")
patch("modules/users/users.py", "        return [\n            self.configure_groups_with_task(),\n",
      "        from pyanaconda.ilex_setup import validate_maintenance\n"
      "        validate_maintenance(self.users)\n"
      "        return [\n            self.configure_groups_with_task(),\n")
patch("modules/users/installation.py", "        self._create_users()\n",
      "        self._create_users()\n"
      "        from pyanaconda.ilex_setup import validate_installed_maintenance\n"
      "        validate_installed_maintenance(self._sysroot, self._user_data_list)\n")
patch("modules/storage/checker/utils.py", "    check_empty = bool(root and",
      "    from pyanaconda.ilex_setup import validate_storage\n"
      "    validate_storage(storage, root, report_error)\n\n"
      "    check_empty = bool(root and")
target = PACKAGE / "ilex_setup.py"
target.write_bytes(Path("/usr/libexec/ilex-installer/anaconda-policy.py").read_bytes())
