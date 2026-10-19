#!/usr/bin/env python3
"""Operator helper: scoped isabel MCP calls, credentials never leave the host."""
import json
from pathlib import Path
import sys
import urllib.request

root = Path('/etc/cocoonbox')
box = json.loads((root / 'isabel.json').read_text())['box_id']
headers = {'Authorization': 'Bearer ' + (root / 'isabel-api-token').read_text().strip(),
           'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream'}


def rpc(target, method, params):
    body = {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params}
    request = urllib.request.Request('http://127.0.0.1:18202/cocoonbox.mcp/' + target,
                                     data=json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(request, timeout=650) as response:
        raw = response.read().decode()
        if raw.startswith(('event:', 'data:')):
            raw = next(line[5:].strip() for line in raw.splitlines() if line.startswith('data:'))
        result = json.loads(raw)
        if response.headers.get('Mcp-Session-Id'):
            headers['Mcp-Session-Id'] = response.headers['Mcp-Session-Id']
    if 'error' in result:
        raise RuntimeError(result['error'])
    return result['result']


def connect():
    rpc('control', 'tools/call', {'name': 'box_ensure', 'arguments': {'box': box, 'wait': True}})
    rpc(box, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                          'clientInfo': {'name': 'video-extract-operator', 'version': '1'}})


def call(name, arguments):
    result = rpc(box, 'tools/call', {'name': name, 'arguments': arguments})
    if result.get('isError'):
        raise RuntimeError(result)
    return result


if __name__ == '__main__':
    connect()
    if sys.argv[1] == 'list':
        result = rpc(box, 'tools/list', {})
    else:
        args = json.loads(Path(sys.argv[2]).read_text())
        result = call(sys.argv[1], args)
    print(json.dumps(result, ensure_ascii=False))
