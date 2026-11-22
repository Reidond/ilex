"""Give the disposable installer one additional, signed source."""

import json
from pathlib import Path
import re
import sys


def override_source(seed, config):
    policy_path = Path('/etc/containers/policy.json')
    policy = json.loads(policy_path.read_text())
    assert policy['default'] == [{'type': 'reject'}]
    policy['transports'].setdefault('docker', {})[config['repository']] = [{
        'type': 'sigstoreSigned', 'keyPath': str(seed / 'test.pub'),
        'signedIdentity': {'type': 'matchRepository'},
    }]
    policy_path.write_text(json.dumps(policy))
    Path('/etc/containers/registries.conf.d/99-encrypted-test.conf').write_text(
        '[[registry]]\nlocation = ' + json.dumps(config['repository']) + '\ninsecure = true\n')
    Path('/etc/containers/registries.d/encrypted-test.yaml').write_text(
        'docker:\n  ' + json.dumps(config['repository']) + ':\n    use-sigstore-attachments: true\n')
    bootc = Path('/tmp/ilex-bootc.ks')
    text, count = re.subn(r'--source-imgref\s+registry:\S+',
                         '--source-imgref registry:' + config['source'], bootc.read_text())
    assert count == 1
    bootc.write_text(text)


if __name__ == '__main__':
    seed = Path(sys.argv[1])
    override_source(seed, json.loads((seed / 'config.json').read_text()))
