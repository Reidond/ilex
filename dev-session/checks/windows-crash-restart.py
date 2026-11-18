"""Read-only evidence for reclamation after an owned launcher crash.

Save vm.json before windows-lifecycle.py forced-exit, restart the same ephemeral
purpose, then compare the crashed run with the new live, SSH-pinned guest.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import paramiko

p = argparse.ArgumentParser()
p.add_argument('--before', type=Path, required=True)
p.add_argument('--after', type=Path, required=True)
p.add_argument('--crash-result', type=Path, required=True)
p.add_argument('--owner', required=True)
p.add_argument('--out', type=Path, required=True)
a = p.parse_args()
old = json.loads(a.before.read_text())
new = json.loads(a.after.read_text())
crash = json.loads(a.crash_result.read_text())
assert old['owner'] == new['owner'] == crash['owner'] == a.owner
assert old['purpose'] == new['purpose'] and old['run'] == crash['run'] != new['run']
assert crash['mode'] == 'forced-exit' and crash['launcher_exit_code'] == 37
assert crash['qemu_exited'] and crash['launcher_exited']
assert crash['ephemeral_disk_remaining'] and crash['ephemeral_vars_remaining']
old_dir, new_dir = Path(old['ssh_key']).parent, Path(new['ssh_key']).parent
assert old_dir.name == old['run'] and new_dir.name == new['run']
assert old_dir.parent == new_dir.parent == a.after.parent
names = ['id_ed25519', 'id_ed25519.pub', 'host_ed25519', 'host_ed25519.pub',
         'overlay.qcow2', 'overlay.vars.fd']
assert all(not (old_dir / name).exists() for name in names)
assert all((new_dir / name).exists() for name in names)
public_keys = {name: hashlib.sha256((new_dir / name).read_bytes()).hexdigest()
               for name in names if name.endswith('.pub')}
assert all(value != crash['public_key_sha256'][name] for name, value in public_keys.items())
for key in ['qmp_endpoint', 'input_endpoint']:
    assert old[key].startswith('unix:') and new[key].startswith('unix:')
    assert not Path(old[key][5:]).exists() and Path(new[key][5:]).exists()
c = paramiko.SSHClient()
for kind, key in paramiko.HostKeys(new['known_hosts']).lookup(new['host_key_alias']).items():
    c.get_host_keys().add(f"[127.0.0.1]:{new['ssh_port']}", kind, key)
c.set_missing_host_key_policy(paramiko.RejectPolicy())
try:
    c.connect('127.0.0.1', port=new['ssh_port'], username='root', key_filename=new['ssh_key'],
              look_for_keys=False, allow_agent=False, timeout=5)
    stdin, stdout, stderr = c.exec_command('cat /proc/sys/kernel/random/boot_id', timeout=5)
    stdin.close()
    boot_id = stdout.read().decode().strip()
    assert stdout.channel.recv_exit_status() == 0 and len(boot_id) == 36, stderr.read()
    result = {'schema': 1, 'utc': datetime.now(timezone.utc).isoformat(), 'owner': a.owner,
              'purpose': new['purpose'], 'old_run': old['run'], 'new_run': new['run'],
              'old_keys_sockets_ephemeral_disk_and_vars_removed': True,
              'new_keys_sockets_ephemeral_disk_and_vars_present': True,
              'new_public_key_sha256': public_keys, 'both_public_keys_rotated': True,
              'new_pinned_ssh_connected': True, 'new_guest_boot_id': boot_id,
              'slot_reused_by_same_purpose': True}
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
finally:
    c.close()
