"""Deterministic scheduling gates, inside the real authenticated homed home.

Production has no scheduling switches/hooks. Each competing edit/restart is
performed by another process with the same UID, using real filesystem/dconf
operations. The gates wrap, rather than replace, the shipped implementations.
"""
import os
import argparse
import contextlib
import io
import json
from pathlib import Path
import subprocess

home = Path.home()
verified = profile.active_home()
relative = ".config/ilex/concurrent.txt"
target = home / relative
original_read = verified.read
original_exchange = profile.exchange
original_run = profile.run
original_worker = profile.settings_worker


def editor_save(text, atomic=False):
    subprocess.run(["/usr/bin/python3", "-I", "-S", "-c", """import os, sys, uuid
from pathlib import Path
target = Path(sys.argv[1])
if sys.argv[3] == 'atomic':
    staged = target.with_name('.editor-save-' + uuid.uuid4().hex)
    staged.write_text(sys.argv[2])
    os.replace(staged, target)
else:
    target.write_text(sys.argv[2])
""", str(target), text, "atomic" if atomic else "inplace"], check=True)


try:
    # Reviewer's gate: edit after the real final comparison returned its bytes.
    for atomic in (False, True):
        target.write_bytes(b"last managed\n")
        fired = []
        def compared(path):
            value = original_read(path)
            if path == relative and not fired:
                fired.append(True)
                editor_save("concurrent user edit\n", atomic)
            return value
        verified.read = compared
        try:
            verified.write(relative, b"new profile\n", profile.digest(b"last managed\n"))
        except profile.FileConflict as error:
            assert error.recovery.startswith(profile.BACKUPS + "/")
            assert (home / error.recovery).read_bytes() == b"new profile\n"
        else:
            raise AssertionError("concurrent editor save was overwritten")
        finally:
            verified.read = original_read
        assert fired and target.read_text() == "concurrent user edit\n"

    # A second editor save while rolling back must survive as a named version.
    target.write_bytes(b"last managed\n")
    fired = []
    verified.read = compared
    exchanges = []
    def exchanged(fd, source, destination):
        original_exchange(fd, source, destination)
        exchanges.append(True)
        if len(exchanges) == 1:
            editor_save("second concurrent edit\n", atomic=True)
    profile.exchange = exchanged
    try:
        verified.write(relative, b"new profile\n", profile.digest(b"last managed\n"))
    except profile.FileConflict as error:
        assert error.recovery is not None
        recovery = home / error.recovery
        assert target.read_text() == "concurrent user edit\n"
        assert recovery.read_text() == "second concurrent edit\n"
    else:
        raise AssertionError("rollback discarded an intervening save")
    finally:
        verified.read = original_read
        profile.exchange = original_exchange
    target.unlink()
    print("real homed file exchange preserves in-place/atomic saves and rollback interleavings: passed")

    # Include direct-helper snapshots in the persistent index before exercising
    # the complete apply path. Only the fixture writes scheduling gates.
    state_path = home / profile.STATE
    raw = state_path.read_bytes()
    state = json.loads(raw)
    verified.backups.entries.update(state.get("backups", {}))
    profile.save_state(verified, state, raw)
    relative = ".config/ilex/welcome.txt"
    target = home / relative
    (target.parent / "added.txt").write_text("new public file\n")
    read_method = profile.Home.read
    read_at_method = profile.Home.read_at
    arguments = argparse.Namespace(name="andriishafar", selected=False, shell=None, install_apps=False)
    if gnome:
        original_run("/usr/bin/gsettings", "set", "org.gnome.desktop.interface", "color-scheme", "default")

    def apply_profile():
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = profile.apply(arguments)
        return result, json.loads(output.getvalue())

    def finish_editor(editor):
        editor.stdin.write("save\n")
        editor.stdin.flush()
        assert editor.wait(timeout=20) == 0, editor.stderr.read()
        editor.stdin.close()
        editor.stdout.close()
        editor.stderr.close()

    for timing in ("after-verification", "after-apply"):
        raw = state_path.read_bytes()
        state = json.loads(raw)
        state["files"][relative] = profile.digest(b"last managed\n")
        state_path.write_text(json.dumps(state, sort_keys=True, indent=2) + "\n")
        target.write_bytes(b"last managed\n")
        editors = []
        fired = []
        def open_after_comparison(instance, path):
            value = read_method(instance, path)
            # The first read is apply's comparison; the second is write's
            # final comparison after staging. Keep the real old inode open.
            if path == relative:
                fired.append(True)
                if len(fired) == 2:
                    editor = subprocess.Popen(["/usr/bin/python3", "-I", "-S", "-c", """import os, sys
with open(sys.argv[1], 'r+b') as stream:
    print('ready', flush=True)
    assert sys.stdin.readline() == 'save\\n'
    stream.seek(0)
    stream.truncate()
    stream.write(b'late editor save\\n')
    stream.flush()
    os.fsync(stream.fileno())
""", str(target)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    assert editor.stdout.readline().strip() == "ready"
                    editors.append(editor)
            return value
        def finish_after_verification(instance, fd, leaf):
            value = read_at_method(instance, fd, leaf)
            if leaf.startswith(".ilex-profile-") and editors and timing == "after-verification":
                finish_editor(editors.pop())
            return value
        profile.Home.read = open_after_comparison
        profile.Home.read_at = finish_after_verification
        try:
            result, report = apply_profile()
            assert len(fired) == 2
            if timing == "after-apply":
                assert result == 0, report
                finish_editor(editors.pop())
            else:
                assert result == 1, report
                assert any("backup changed" in c for c in report["conflicts"]), report
        finally:
            profile.Home.read = read_method
            profile.Home.read_at = read_at_method
            for editor in editors:
                editor.kill()
                editor.wait()
        state = json.loads(state_path.read_bytes())
        recoveries = [path for path, record in state["backups"].items()
                      if record["source"] == relative and (home / path).read_bytes() == b"late editor save\n"]
        assert recoveries, state
        assert target.read_text() == "profile revision two\n"
        result, report = apply_profile()
        assert result == 1 and any(recoveries[-1] in c for c in report["conflicts"]), report
        # Keep the next iteration independent. Explicit user-run cleanup is the
        # only production operation allowed to remove these retained versions.
        cleanup = subprocess.run(["/usr/bin/ilex-agent", "profile", "prune-backups", "--execute"],
                                 capture_output=True, text=True)
        assert cleanup.returncode == 0, (cleanup.stdout, cleanup.stderr)
    print("real btrfs homed outstanding-fd saves survive in backups; end/next apply report paths: passed")

    # No automatic pruning, including oversized late edits. Size accounting
    # uses fstat while content verification remains bounded to one MiB.
    for cycle in range(3):
        raw = state_path.read_bytes()
        state = json.loads(raw)
        verified.backups = profile.Backups(verified, state["backups"])
        state["cleanup_fixture"] = cycle
        profile.save_state(verified, state, raw)
    raw = state_path.read_bytes()
    state = json.loads(raw)
    del state["cleanup_fixture"]
    verified.backups = profile.Backups(verified, state["backups"])
    large = verified.backups.reserve(relative, profile.digest(b"before late growth"))
    with verified.parent(large, create=True) as (fd, leaf):
        target_fd = os.open(leaf, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=fd)
        try:
            os.ftruncate(target_fd, 51 * 1024 * 1024)
        finally:
            os.close(target_fd)
    profile.save_state(verified, state, raw)
    notice = io.StringIO()
    with contextlib.redirect_stderr(notice):
        result, report = apply_profile()
    assert result == 1 and large in " ".join(report["conflicts"]), report
    assert "backups use 51." in notice.getvalue(), notice.getvalue()
    assert len(notice.getvalue().splitlines()) == 1
    assert (home / large).stat().st_size == 51 * 1024 * 1024

    def cleanup(*args, code=0):
        result = subprocess.run(["/usr/bin/ilex-agent", "profile", "prune-backups", *args],
                                capture_output=True, text=True)
        assert result.returncode == code, (result.stdout, result.stderr)
        return json.loads(result.stdout) if code == 0 else None

    raw = state_path.read_bytes()
    dry = cleanup("--keep", "2")
    assert dry["dry_run"] and dry["bytes"] >= 51 * 1024 * 1024
    assert sum(not group["remove"] for group in dry["applies"]) == 2
    assert state_path.read_bytes() == raw
    assert all((home / group["path"]).is_dir() for group in dry["applies"])
    assert cleanup()["dry_run"]  # No --execute always lists without removal.

    # Neither dry-run nor execute may traverse a redirected backup directory.
    backup_root = home / profile.BACKUPS
    saved_root = backup_root.with_name("profile-backups-saved")
    outside = home / "outside-profile"
    sentinel = outside / "welcome.txt"
    backup_root.rename(saved_root)
    backup_root.symlink_to(outside, target_is_directory=True)
    try:
        cleanup(code=2)
        cleanup("--execute", code=2)
        assert sentinel.read_text() == "outside sentinel\n"
    finally:
        backup_root.unlink()
        saved_root.rename(backup_root)
    executed = cleanup("--keep", "2", "--execute")
    assert not executed["dry_run"] and executed["state_backup"]
    for group in executed["applies"]:
        assert (home / group["path"]).exists() != group["remove"], group
    assert (home / executed["state_backup"]).is_dir()
    cleanup("--execute")  # Explicit fixture cleanup, including the sparse edit.
    print("profile backup notice, no automatic pruning, dry run, keep-N and no-follow cleanup: passed")

    if gnome:
        key = "/org/gnome/desktop/interface/clock-show-weekday"
        schema = "org.gnome.desktop.interface"
        setting = "clock-show-weekday"
        previous = {setting: "true"}
        desired = {"gnome": {setting: False}}
        saved = original_run("/usr/bin/dconf", "read", key)

        try:
            original_run("/usr/bin/gsettings", "set", schema, setting, "true")
            resets = []
            def reset_after_comparison(*args, **kwargs):
                value = original_run(*args, **kwargs)
                if args == ("/usr/bin/dconf", "read", key) and not resets:
                    resets.append(True)
                    original_run("/usr/bin/gsettings", "reset", schema, setting, **kwargs)
                return value
            profile.run = reset_after_comparison
            conflicts = []
            managed = profile.settings(verified, desired, previous, "gnome", conflicts)
            assert resets and any("changed before write" in c for c in conflicts), conflicts
            assert managed == previous and original_run("/usr/bin/dconf", "read", key) == ""
            profile.run = original_run

            # A user's later value wins if another client saves during our write.
            original_run("/usr/bin/gsettings", "set", schema, setting, "true")
            resets = []
            def reset_during_write(*args, **kwargs):
                value = original_run(*args, **kwargs)
                if args[:5] == ("/usr/bin/gsettings", "set", schema, setting, "false") and not resets:
                    resets.append(True)
                    original_run("/usr/bin/gsettings", "reset", schema, setting, **kwargs)
                return value
            profile.run = reset_during_write
            conflicts = []
            managed = profile.settings(verified, desired, previous, "gnome", conflicts)
            assert resets and any("changed during write" in c for c in conflicts), conflicts
            assert managed == previous and original_run("/usr/bin/dconf", "read", key) == ""
            profile.run = original_run

            # The real checks pass, then another same-UID process replaces the
            # writer. Its static unit is still valid; identity must be rechecked.
            original_run("/usr/bin/gsettings", "set", schema, setting, "true")
            restarted = []
            def restart_after_checks(data, old, conflicts, environment):
                subprocess.run(["/usr/bin/systemctl", "--user", "restart", "dconf.service"],
                               env=environment, check=True)
                restarted.append(True)
                return original_worker(data, old, conflicts, environment)
            profile.settings_worker = restart_after_checks
            conflicts = []
            managed = profile.settings(verified, desired, previous, "gnome", conflicts)
            assert restarted and "settings.gnome (writer or bus changed during apply)" in conflicts, conflicts
            assert managed == previous
            print("real homed concurrent resets and post-write writer replacement detection: passed")
        finally:
            profile.run = original_run
            profile.settings_worker = original_worker
            if saved:
                original_run("/usr/bin/gsettings", "set", schema, setting, saved)
            else:
                original_run("/usr/bin/gsettings", "reset", schema, setting)
finally:
    verified.read = original_read
    profile.exchange = original_exchange
    profile.run = original_run
    profile.settings_worker = original_worker
    os.close(verified.fd)
