"""Real stdio MCP/capture and stale-coordinate checks against an owned VM.

With --resize, wait for Cua to resize the exact QEMU HWND after ready.json.
Only a stale-coordinate rejection is attempted; no host input is fabricated.
"""
import argparse
import base64
import io
import json
from pathlib import Path
import queue
import subprocess
import threading
import time
from PIL import Image

p = argparse.ArgumentParser()
p.add_argument('--info', type=Path, required=True)
p.add_argument('--vmctl', type=Path, required=True)
p.add_argument('--artifacts', type=Path, required=True)
p.add_argument('--resize', action='store_true')
a = p.parse_args()
info = json.loads(a.info.read_text())
assert info['schema'] == 1 and info['display'] == 'sdl-gl'
a.artifacts.mkdir(parents=True, exist_ok=True)
messages = queue.Queue()
with (a.artifacts / 'mcp.stderr.log').open('w', encoding='utf-8') as log:
    server = subprocess.Popen([str(a.vmctl), 'mcp', '--vm', info['purpose']],
                              cwd=Path(__file__).resolve().parents[2], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=log, text=True, encoding='utf-8')
    def read():
        for line in server.stdout:
            messages.put(json.loads(line))
    threading.Thread(target=read, daemon=True).start()
    sequence = 0
    def request(method, params=None):
        global sequence
        sequence += 1
        server.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': sequence, 'method': method, 'params': params or {}}) + '\n')
        server.stdin.flush()
        reply = messages.get(timeout=15)
        assert reply['id'] == sequence and 'error' not in reply
        return reply['result']
    def tool(name, arguments=None):
        return request('tools/call', {'name': name, 'arguments': arguments or {}})
    def screenshot(name):
        result = tool('screenshot', {'full': True})
        assert not result['isError'], result
        images = [v for v in result['content'] if v['type'] == 'image']
        assert len(images) == 1 and images[0]['mimeType'] == 'image/png'
        data = base64.b64decode(images[0]['data'], validate=True)
        size = Image.open(io.BytesIO(data)).size
        (a.artifacts / name).write_bytes(data)
        return size
    try:
        initialized = request('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'ilex-real-window-check', 'version': '1'}})
        assert initialized['serverInfo']['name'] == 'ilex-vmctl'
        server.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        server.stdin.flush()
        tools = request('tools/list')['tools']
        assert {'capabilities', 'screenshot', 'click', 'vm_list'} <= {t['name'] for t in tools}
        listed = tool('vm_list')
        assert not listed['isError'] and any(info['name'] in c.get('text', '') for c in listed['content'])
        caps = tool('capabilities')
        assert not caps['isError']
        assert json.loads(caps['content'][0]['text'])['provider'] == 'windows-qemu-whpx'
        before = screenshot('mcp-before.png')
        result = {'stdio_initialize': True, 'tools_list': True, 'owned_vm_list': True, 'capabilities': True, 'before': before}
        if a.resize:
            (a.artifacts / 'ready.json').write_text(json.dumps({'pid': info['pid'], 'hwnd': info['hwnd'], 'size': before}), encoding='utf-8')
            until = time.monotonic() + 60
            # Cua creates the physical resize. Wait only for its explicit marker.
            while not (a.artifacts / 'resized').exists() and time.monotonic() < until:
                time.sleep(.1)
            assert (a.artifacts / 'resized').exists(), 'Cua resize marker did not arrive'
            rejected = tool('click', {'x': 0, 'y': 0})
            assert rejected['isError'] and any('screenshot' in c.get('text', '') for c in rejected['content']), rejected
            after = screenshot('mcp-after.png')
            assert after != before
            result.update({'stale_coordinates_rejected': True, 'fresh_screenshot_recovers': True, 'after': after})
        (a.artifacts / 'mcp-check.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
        print(json.dumps(result))
    finally:
        server.stdin.close()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
