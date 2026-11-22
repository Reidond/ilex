#!/usr/bin/python3
"""Guest-only homed qualification driver. Secrets arrive on stdin, never argv.

Not installed in the image. PTYs exercise the packaged login/sudo/passwd PAM
consumers. Only redacted terminal output may leave this process.
"""
import errno
import hashlib
import json
import os
import pty
import re
import select
import signal
import subprocess as sp
import sys
import time
import traceback

request = json.load(sys.stdin)
secrets = request.get("secrets", [])
os.environ.update(LC_ALL="C", SYSTEMD_COLORS="0", SYSTEMD_URLIFY="0", TERM="dumb")
for key in ("PASSWORD", "NEWPASSWORD"):
    os.environ.pop(key, None)


def run(*args, **kwargs):
    return sp.run(args, check=True, text=True, capture_output=True, **kwargs).stdout.strip()


def terminal(argv, dialogue, timeout=180, allow_failure=False, optional=False, current_password=None):
    pid, fd = pty.fork()
    if pid == 0:
        os.execvpe(argv[0], argv, os.environ)
    transcript = b""
    cursor = 0
    deadline = time.monotonic() + timeout
    status = None
    ended = False
    try:
        for pattern, response in dialogue:
            while True:
                match = re.search(pattern.encode(), transcript[cursor:], re.I)
                if match:
                    cursor += match.end()
                    if response is not None:
                        os.write(fd, response.encode() + b"\n")
                    break
                if current_password is not None:
                    current = re.search(rb"(?:^|[\r\n])(?:\(?(?:current|old)[^\r\n]*?)?password[^\r\n]*:", transcript[cursor:], re.I)
                    if current:
                        cursor += current.end()
                        os.write(fd, current_password.encode() + b"\n")
                        current_password = None
                        continue
                if time.monotonic() > deadline:
                    raise RuntimeError("terminal prompt timed out: " + pattern)
                ready, _, _ = select.select([fd], [], [], 0.2)
                if ready:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            if optional:
                                ended = True
                                break
                            raise RuntimeError("terminal ended before prompt: " + pattern)
                        raise
                    if not chunk:
                        if optional:
                            ended = True
                            break
                        raise RuntimeError("terminal EOF before prompt: " + pattern)
                    transcript += chunk
            if ended:
                break
        while time.monotonic() < deadline:
            waited, child_status = os.waitpid(pid, os.WNOHANG)
            if waited:
                status = child_status
                code = os.waitstatus_to_exitcode(status)
                if not allow_failure and code != 0:
                    raise RuntimeError("terminal command failed")
                output = transcript.decode(errors="replace")
                return (code, output) if allow_failure else output
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    transcript += os.read(fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
        raise RuntimeError("terminal command timed out")
    except Exception as error:
        raise RuntimeError(str(error) + "\n" + transcript.decode(errors="replace")) from None
    finally:
        os.close(fd)
        if status is None:
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except ProcessLookupError:
                pass


def create(name, password, admin=False):
    argv = ["/usr/bin/ilexctl", "home", "create", name, "--disk-size=2", "--real-name=Ilex Homed Test"]
    if admin:
        argv.append("--admin")
    terminal(argv, [(r"password[^\r\n]*:", password), (r"(?:password|again)[^\r\n]*:", password)])


def activate(name, password):
    terminal(["/usr/bin/homectl", "activate", name], [(r"password[^\r\n]*:", password)])


def tty(name, password, command, extra=(), current_password=None):
    # Leave the caller's SSH session/cgroup. Otherwise pam_systemd may reuse
    # root's session and never start the target user's manager/dependencies.
    text = terminal(login_argv(name), [
        (r"password[^\r\n]*:", password),
        (r"[$#] ", command + "; printf 'ILEX-TTY-DONE:%s\\n' $?; exit"),
        *extra,
        (r"ILEX-TTY-DONE:0\r?\n", None),
    ], current_password=current_password)
    return text


def login_argv(name):
    return ["/usr/bin/systemd-run", "--quiet", "--wait", "--collect", "--pty",
            "--service-type=exec", "--setenv=LC_ALL=C", "--setenv=TERM=dumb",
            # Use Fedora's normal init -> getty -> login SELinux transitions.
            # init_t intentionally cannot execute login_exec_t directly.
            "/usr/sbin/agetty", "--skip-login", "--login-options=" + name,
            "--noreset", "--noclear", "--noissue", "-", "dumb"]


def sizes(name, flush=True):
    home = "/home/" + name
    if flush:
        run("sync", "-f", home)
    source = run("findmnt", "-nro", "SOURCE", "--target", home).split("[")[0]
    assert source.startswith("/dev/mapper/"), source
    assert run("findmnt", "-nro", "FSTYPE", "--target", home) == "btrfs"
    if flush:
        fs = int(re.search(r"total_bytes\s+(\d+)", run("btrfs", "inspect-internal", "dump-super", source)).group(1))
    else:
        fs = int(re.search(r"Device size:\s+(\d+)", run("btrfs", "filesystem", "usage", "-b", home)).group(1))
    mapper = int(run("blockdev", "--getsize64", source))
    backing = os.stat("/var/home/" + name + ".home").st_size
    assert 0 < fs <= mapper < backing, (fs, mapper, backing)
    return [fs, mapper, backing]


def checksum(name):
    path = "/home/" + name + "/e2e-data"
    if not os.path.exists(path):
        with open(path, "wb") as stream:
            for _ in range(32):
                stream.write(os.urandom(1024 * 1024))
        os.chown(path, int(run("id", "-u", name)), int(run("id", "-g", name)))
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    action = request["action"]
    name = request.get("name", "homedtest")
    if action == "installer-profile-login":
        tty(name, secrets[0], r"""test "$(findmnt -nro FSTYPE --target "$HOME")" = btrfs &&
systemctl --user is-active dbus.socket &&
test "$(systemctl --user show ilex-profile.service -p Result --value)" = success &&
timeout 60 bash -c 'until test -s ~/.local/state/ilex/profile.json; do sleep 1; done' &&
python3 -c 'import json,pathlib; assert json.loads((pathlib.Path.home()/".local/state/ilex/profile.json").read_text())["profile"] == "andriishafar"' &&
test -s ~/.config/ilex/welcome.txt""".replace("\n", " "))
        return "authenticated PAM login with encrypted btrfs and installer profile applied"
    if action == "profiles":
        tty(name, secrets[0], "python3 -I -S /run/ilex-profiles-user.py")
        return "password-authenticated profile application and safety regressions passed"
    if action == "profile-relogin":
        tty(name, secrets[0], 'test -r ~/.local/state/ilex/profile.json && test -r ~/.config/ilex/welcome.txt && systemctl --user is-enabled ilex-profile.service && ! systemctl --user restart ilex-profile.service && test "$(systemctl --user show ilex-profile.service -p ExecMainStatus --value)" = 1')
        return "login succeeds with a newer profile state on an older image"
    if action == "maintenance":
        run("useradd", "--create-home", "--groups", "wheel", "--shell", "/bin/bash", "ilexmaint")
        run("chpasswd", input="ilexmaint:" + secrets[0] + "\n")
        assert "ilexmaint:" in open("/etc/passwd").read()
        # Split the prompt in the echoed shell command: only sudo's actual
        # prompt may trigger sending the credential into the terminal.
        tty("ilexmaint", secrets[0], "sudo -k; sudo -p ILEX-'SUDO:' id -u", [(r"ILEX-SUDO:", secrets[0])])
        return "local maintenance login and password-authenticated sudo passed"
    if action == "maintenance-recovery":
        tty("ilexmaint", secrets[0], "sudo -k; sudo -p ILEX-'SUDO:' id -u", [(r"ILEX-SUDO:", secrets[0])])
        return "maintenance recovery login and sudo still work with the daily home inactive"
    if action == "create":
        create(name, secrets[0], request.get("admin", False))
        assert not any(line.startswith(name + ":") for line in open("/etc/passwd"))
        assert run("getent", "passwd", name).startswith(name + ":")
        return "created encrypted home; NSS resolves identity without a passwd entry"
    if action == "profile-empty-apps":
        tty(name, secrets[0], "ilexctl profile apply andriishafar --install-apps && "
            "flatpak remotes --user --columns=name,url | grep 'https://dl.flathub.org/repo/' && "
            "test -z \"$(flatpak list --user --app --columns=application)\"")
        return "an empty app profile still creates the user Flathub remote without downloading apps"
    if action == "collision":
        before = run("getent", "-s", "files", "passwd", "ilexmaint")
        try:
            create("ilexmaint", secrets[0])
        except RuntimeError as error:
            assert re.search(r"already|exist|used|conflict", str(error), re.I), str(error)
        else:
            raise RuntimeError("local account collision was accepted")
        assert run("getent", "-s", "files", "passwd", "ilexmaint") == before
        assert not os.path.exists("/var/home/ilexmaint.home")
        return "homed refused the local account collision without changing the maintenance identity"
    if action == "activate":
        activate(name, secrets[0])
        return {"sizes": sizes(name), "sha256": checksum(name)}
    if action == "tty":
        # ls -Z can show '?' for the idmapped mount root; stat reads its
        # effective context correctly (including the initial defcontext).
        text = tty(name, secrets[0], "test \"$SYSTEMD_HOME\" = 1 && test \"$SYSTEMD_HOME_SUSPEND\" = 0 && test -r ~/e2e-data && stat -c '%C %n' ~ ~/e2e-data && loginctl show-session self -p Type -p Name && sudo -k && sudo -p ILEX-'SUDO:' id -u", [(r"ILEX-SUDO:", secrets[0])])
        assert re.search(r"\r?\n0\r?\n", text), text
        assert "Type=tty" in text, text
        assert "Name=" + name in text, text
        assert ":user_home_dir_t:" in text and ":user_home_t:" in text, text
        return "TTY PAM activates home, suspend=off, sudo authenticates the homed password"
    if action == "password":
        # Handle both a current-password challenge and reuse of the login
        # credential. The host then checks old-password rejection and a fresh
        # login with the new password.
        tty(name, secrets[0], "passwd", [
            (r"new[^\r\n]*password[^\r\n]*:", secrets[1]),
            (r"(?:retype|repeat)[^\r\n]*password[^\r\n]*:", secrets[1]),
        ], current_password=secrets[0])
        return "password changed through the installed passwd PAM stack"
    if action == "wrong-password":
        # login retries on rejection: wait for the real rejection, then close
        # this terminal rather than providing more attempts/locking the user.
        pid, fd = pty.fork()
        if pid == 0:
            argv = login_argv(name)
            os.execv(argv[0], argv)
        data = b""
        sent = False
        try:
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if select.select([fd], [], [], 0.2)[0]:
                    data += os.read(fd, 65536)
                if not sent and re.search(b"password[^\r\n]*:", data, re.I):
                    os.write(fd, secrets[0].encode() + b"\n")
                    sent = True
                if re.search(rb"Login incorrect|Password incorrect or not sufficient for authentication", data):
                    return "wrong password rejected by login PAM"
            raise RuntimeError("login did not reject the wrong password\n" + data.decode(errors="replace"))
        finally:
            os.close(fd)
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
    if action == "resize":
        before = sizes(name)
        digest = checksum(name)
        measured = [before]
        for size in ("3G", "2G"):
            terminal(["/usr/bin/homectl", "resize", name, size], [(r"password[^\r\n]*:", secrets[0])], optional=True)
            measured.append(sizes(name))
            assert checksum(name) == digest, ("checksum changed after resize", size, measured)
        assert all(measured[1][i] > measured[0][i] for i in range(3)), measured
        assert all(measured[2][i] < measured[1][i] for i in range(3)), measured
        # Homed may clamp a request below btrfs's safe minimum rather than
        # reject it. Check the actual dimensions and data in either case.
        code, output = terminal(["homectl", "resize", name, "16M"], [(r"password[^\r\n]*:", secrets[0])], allow_failure=True, optional=True)
        minimum = sizes(name)
        if code == 0:
            assert minimum[0] > 32 * 1024 * 1024, ("unsafe shrink", minimum)
        else:
            assert re.search(r"small|minimum|space|invalid argument", output, re.I), output
            assert minimum == measured[2], ("rejected shrink changed size", minimum, measured)
        assert checksum(name) == digest, "undersize request changed data"
        terminal(["homectl", "resize", name, "2G"], [(r"password[^\r\n]*:", secrets[0])], optional=True)
        assert sizes(name) == measured[2], ("size not restored", sizes(name), measured)
        return {"sizes_fs_mapper_backing": measured, "sha256": digest, "undersize_sizes": minimum, "undersize_data_preserved": True}
    if action == "resize-to":
        assert request["size"] in ("2G", "3G")
        terminal(["homectl", "resize", name, request["size"]], [(r"password[^\r\n]*:", secrets[0])], optional=True)
        return {"sizes": sizes(name), "sha256": checksum(name)}
    if action == "crash-start":
        import ilex_homed_crash
        return ilex_homed_crash.start(name, secrets[0], request["direction"], terminal, sizes)
    if action == "crash-wait":
        import ilex_homed_crash
        return ilex_homed_crash.wait()
    if action == "multi-home":
        create("homedother", secrets[0])
        activate("homedother", secrets[0])
        groups = run("id", "-nG", "homedother").split()
        assert "ilex-homed" in groups and "wheel" not in groups, groups
        digests = {user: checksum(user) for user in (name, "homedother")}
        assert sizes(name)[0] > 0 and sizes("homedother")[0] > 0
        return {"concurrent_home_sha256": digests}
    if action == "enospc":
        # Fill a disposable outer filesystem mounted at /var/home by the
        # host tier. This never fills the guest root or a host filesystem.
        before = sizes(name)
        digest = checksum(name)
        other_digest = checksum("homedother")
        with open("/var/home/e2e-fill", "wb", buffering=0) as stream:
            try:
                while True:
                    stream.write(os.urandom(4 * 1024 * 1024))
            except OSError as error:
                assert error.errno == errno.ENOSPC
        try:
            refused = sp.run(["ilexctl", "home", "create", "homedfull", "--disk-size=2"], stdin=sp.DEVNULL, capture_output=True)
            assert refused.returncode != 0
            assert b"system reserve" in refused.stderr
            assert sp.run(["getent", "passwd", "homedfull"], capture_output=True).returncode != 0
            assert not os.path.exists("/var/home/homedfull.home")
            code, output = terminal(["homectl", "resize", name, "3G"], [(r"password[^\r\n]*:", secrets[0])], allow_failure=True, optional=True)
            limited = sizes(name)
            # Like undersize requests, homed can safely clamp growth to the
            # backing filesystem's available capacity and return success.
            assert limited[2] < 3 * 1024 * 1024 * 1024, ("growth reached requested size on full storage", before, limited, output)
            if code != 0:
                assert re.search(r"space|allocat|disk full", output, re.I), output
            assert checksum(name) == digest
            assert checksum("homedother") == other_digest
            assert run("getent", "passwd", "ilexmaint").startswith("ilexmaint:")
            tty("ilexmaint", secrets[1], "sudo -k; sudo -p ILEX-'SUDO:' id -u", [(r"ILEX-SUDO:", secrets[1])])
        finally:
            os.unlink("/var/home/e2e-fill")
            os.sync()
        terminal(["homectl", "resize", name, "3G"], [(r"password[^\r\n]*:", secrets[0])], optional=True)
        assert sizes(name)[0] > limited[0]
        assert checksum(name) == digest
        assert checksum("homedother") == other_digest
        return {"enospc_data_preserved": True, "limited_sizes": limited, "recovered_sizes": sizes(name), "sha256": digest}
    if action == "secret-audit":
        journal = run("journalctl", "--no-pager", "-o", "cat")
        assert not any(secret in journal for secret in secrets), "credential appeared in journal"
        for directory, _, files in os.walk("/var/log"):
            for filename in files:
                path = os.path.join(directory, filename)
                if os.path.isfile(path):
                    with open(path, "rb") as stream:
                        data = stream.read()
                    assert not any(secret.encode() in data for secret in secrets), "credential appeared in log: " + path
        return "no plaintext test credential in journal or /var/log"
    raise RuntimeError("unknown action")


try:
    print(json.dumps(main()))
except Exception as error:
    frame = traceback.extract_tb(error.__traceback__)[-1]
    message = f"{type(error).__name__} at {frame.name}:{frame.lineno}: {error}"
    for secret in secrets:
        message = message.replace(secret, "[redacted]")
    print(message, file=sys.stderr)
    sys.exit(1)
