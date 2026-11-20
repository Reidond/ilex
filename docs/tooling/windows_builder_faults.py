"""Windows row of the lifecycle matrix, run against the managed builder.

The real submit reply is delayed in a temporary SSH relay beside a copy of
xtask.exe. The client runs in its own hidden console. Its relay refuses every
PTY request, so a regression in the test cannot start an unintended VM.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


RELAY = r'''
use std::{env, fs, io::{self, Read, Write}, path::PathBuf, process::{self, Command, Stdio},
          thread, time::{Duration, Instant}};

fn main() -> io::Result<()> {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let state = PathBuf::from(env::var_os("ILEX_MATRIX_RELAY").unwrap());
    if args.first().is_some_and(|arg| arg == "-t" || arg == "-tt") {
        fs::write(state.join("attach-attempted"), b"PTY requested")?;
        process::exit(91);
    }
    let mut data = Vec::new();
    io::stdin().read_to_end(&mut data)?;
    let submit = String::from_utf8_lossy(&data).contains("\"operation\":\"submit\"");
    let mut child = Command::new(env::var_os("ILEX_MATRIX_REAL_SSH").unwrap())
        .args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input = thread::spawn(move || stdin.write_all(&data));
    let reply = child.wait_with_output()?;
    let _ = input.join();
    if submit && reply.status.success() {
        let temporary = state.join("registered.partial");
        fs::write(&temporary, &reply.stdout)?;
        fs::rename(temporary, state.join("registered"))?;
        let until = Instant::now() + Duration::from_secs(45);
        while !state.join("release").is_file() && Instant::now() < until {
            thread::sleep(Duration::from_millis(30));
        }
    }
    io::stdout().write_all(&reply.stdout)?;
    io::stderr().write_all(&reply.stderr)?;
    process::exit(reply.status.code().unwrap_or(1));
}
'''

CONSOLE_EVENT = r'''
import ctypes
import os
import sys
import time

kernel = ctypes.WinDLL("kernel32", use_last_error=True)
client = int(sys.argv[1])
kernel.FreeConsole()
assert kernel.AttachConsole(client), ctypes.WinError(ctypes.get_last_error())
try:
    peers = (ctypes.c_uint32 * 16)()
    count = kernel.GetConsoleProcessList(peers, len(peers))
    assert count == 2 and set(peers[:count]) == {client, os.getpid()}, "console is shared"
    assert kernel.SetConsoleCtrlHandler(None, 1), ctypes.WinError(ctypes.get_last_error())
    # Group zero is all clients of this newly created console only.
    assert kernel.GenerateConsoleCtrlEvent(0, 0), ctypes.WinError(ctypes.get_last_error())
    time.sleep(.5)
finally:
    kernel.FreeConsole()
'''


def ctrl_c_during_interactive_submit():
    workspace = Path(__file__).resolve().parents[2]
    executable = workspace / "target/debug/xtask.exe"
    assert executable.is_file(), "build the current native xtask before running the Windows matrix"
    description = json.loads(subprocess.check_output([str(executable), "info", "--json"], cwd=workspace, timeout=45))
    remote = Path(description["state_dir"]) / "remote"
    # Keep the test's binary and relay out of PATH and out of the checkout.
    with tempfile.TemporaryDirectory(prefix="ilex-builder-matrix-") as root:
        root = Path(root)
        test_client = root / "xtask.exe"
        shutil.copy2(executable, test_client)
        code = root / "relay.rs"
        code.write_text(RELAY, encoding="utf-8")
        subprocess.run(["rustc", "--edition", "2024", str(code), "-o", str(root / "ssh.exe")],
                       cwd=workspace, check=True, timeout=60)
        env = os.environ.copy()
        env["ILEX_MATRIX_RELAY"] = str(root)
        env["ILEX_MATRIX_REAL_SSH"] = shutil.which("ssh.exe")
        assert env["ILEX_MATRIX_REAL_SSH"]
        console = subprocess.STARTUPINFO()
        console.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        console.wShowWindow = 0
        client = subprocess.Popen([str(test_client), "vm"], cwd=workspace, env=env,
                                  startupinfo=console, creationflags=subprocess.CREATE_NEW_CONSOLE)
        job = None
        try:
            until = time.monotonic() + 120
            registered = root / "registered"
            while not registered.is_file():
                assert client.poll() is None, f"client exited before submit reply was held ({client.returncode})"
                assert time.monotonic() < until, "submit reply did not reach the relay"
                time.sleep(.1)
            reply = json.loads(registered.read_bytes())
            assert reply["ok"]
            job = reply["value"]["job"]
            sent = subprocess.run([sys.executable, "-B", "-c", CONSOLE_EVENT, str(client.pid)],
                                  capture_output=True, timeout=10, creationflags=subprocess.CREATE_NO_WINDOW)
            assert sent.returncode == 0, sent.stderr.decode(errors="replace")
            assert client.poll() is None, "Ctrl-C killed the client before the submit reply was released"
            (root / "release").touch()
            assert client.wait(timeout=90) == 130, f"unexpected client exit ({client.returncode})"
            assert not (root / "attach-attempted").exists(), "the client attempted to open a terminal"
            receipt = json.loads((remote / "jobs" / job / "collection.json").read_bytes())
            assert receipt["job"] == job and receipt["exit_code"] == 130
            received = Path(receipt["received"])
            result = json.loads((received / "result.json").read_bytes())
            state = json.loads((received / "job/state.json").read_bytes())
            assert result["job"] == job and state == {"state": "cancelled", "exit_code": 130}
            assert {entry["path"] for entry in result["artifacts"]} == {
                "job/request.json", "job/context.json", "job/state.json"
            }
            print(f"Windows submit {job} was cancelled with no PTY request", flush=True)
        finally:
            if client.poll() is None:
                client.kill()
                client.wait(timeout=10)
            if job is not None and not (remote / "jobs" / job / "collection.json").exists():
                subprocess.run([str(executable), "remote", "cancel", "--job", job],
                               cwd=workspace, env=env, timeout=60, check=False)
