"""Validate the complete cross-job root inventory before emitting shell fields.

No registry, compiler or imported project code; signing calls this without keys.
Each row is tag, own snapshot, changed, staged digest, fingerprint, tested ID.
An unchanged/unforced root has explicit '-' for all three test fields.
"""

import json
import re
import sys


def digest(value):
    return isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value)


rows = json.load(sys.stdin)
assert isinstance(rows, list) and rows, "missing root inventory"
seen = set()
output = []
for row in rows:
    assert isinstance(row, dict) and set(row) == {
        "tag", "snapshot", "changed", "digest", "fingerprint", "image_id"
    }, "invalid root fields"
    tag = row["tag"]
    assert isinstance(tag, str) and len(tag) <= 116, "invalid root tag"
    assert tag == "base" or re.fullmatch(r"generic\.[a-z][a-z0-9-]*(\.[a-z][a-z0-9-]*)*", tag), "not a root tag"
    assert tag not in seen, "duplicate root"
    seen.add(tag)
    assert row["snapshot"] == "-" or digest(row["snapshot"]), "invalid own snapshot"
    assert type(row["changed"]) is bool, "invalid changed flag"
    evidence = [row[k] for k in ("digest", "fingerprint", "image_id")]
    assert all(digest(v) for v in evidence) or (
        not row["changed"] and evidence == ["-"] * 3
    ), "missing/invalid test evidence"
    output.append("\t".join([
        tag, row["snapshot"], str(row["changed"]).lower(), *evidence
    ]))
print("\n".join(output))
