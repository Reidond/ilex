"""Deterministic scheduling gates, inside the real authenticated homed home.

Production has no scheduling switches/hooks. Each competing edit/restart is
performed by another process with the same UID, using real filesystem/dconf
operations. The gates wrap, rather than replace, the shipped implementations.
"""
import os
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
            assert error.recovery is None
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
        recovery.unlink()
    else:
        raise AssertionError("rollback discarded an intervening save")
    finally:
        verified.read = original_read
        profile.exchange = original_exchange
    target.unlink()
    print("real homed file exchange preserves in-place/atomic saves and rollback interleavings: passed")

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
