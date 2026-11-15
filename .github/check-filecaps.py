"""Run inside an image: compare RPM-recorded file capabilities with getcap.

podman run --rm -i --network=none --entrypoint python3 IMAGE -I -S - < this-file
No RPM scriptlets or writes; missing files/capabilities and command errors fail.
"""

import subprocess
import sys


def output(*args):
    return subprocess.check_output(args, text=True)


records = output("rpm", "-qa", "--qf", "[%{FILENAMES}\t%{FILECAPS}\n]")
checked, failed = 0, 0
for record in records.splitlines():
    path, expected = record.split("\t", 1)
    if expected in ("", "(none)"):
        continue
    checked += 1
    actual = output("getcap", "-n", path).strip()
    prefix = path + " "
    actual = actual[len(prefix):] if actual.startswith(prefix) else actual
    if actual != expected:
        print(f"FAIL {path}: rpm={expected!r}, getcap={actual!r}", file=sys.stderr)
        failed += 1
    else:
        print(f"OK {path}: {actual}")
if not checked:
    sys.exit("no RPM-recorded file capabilities found; refusing a vacuous check")
print(f"file capabilities: {checked} checked, {failed} mismatched")
sys.exit(bool(failed))
