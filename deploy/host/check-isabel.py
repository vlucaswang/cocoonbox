"""Deployment smoke test; reads the scoped credential without printing it."""
import json
from pathlib import Path
import urllib.error
import urllib.request

root = Path('/etc/cocoonbox')
box = json.loads((root/'isabel.json').read_text())['box_id']
token = (root/'isabel-api-token').read_text().strip()

def rpc(target, method, params=None, session=None, notification=False):
    headers = {'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json',
               'Accept': 'application/json, text/event-stream'}
    if session:
        headers['Mcp-Session-Id'] = session
    body = {'jsonrpc': '2.0', 'method': method, 'params': params or {}}
    if not notification:
        body['id'] = 1
    req = urllib.request.Request('http://127.0.0.1:18202/cocoonbox.mcp/' + target,
        data=json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=180) as response:
        raw = response.read().decode()
        if raw.startswith(('event:', 'data:')):
            raw = next(line[5:].strip() for line in raw.splitlines() if line.startswith('data:'))
        return (json.loads(raw) if raw else {}), response.headers.get('Mcp-Session-Id')

def success(result):
    assert 'error' not in result, result.get('error')
    assert not result.get('result', {}).get('isError'), 'Tool failed'
    return result['result']

success(rpc('control', 'tools/call', {'name': 'box_ensure', 'arguments': {'box': box, 'wait': True}})[0])
print('PASS: isabel ensured running', flush=True)
initialized, session = rpc(box, 'initialize', {'protocolVersion': '2025-06-18',
    'capabilities': {}, 'clientInfo': {'name': 'isabel-isolation-check', 'version': '1'}})
success(initialized)
rpc(box, 'notifications/initialized', session=session, notification=True)
tools = success(rpc(box, 'tools/list', session=session)[0])['tools']
assert any(t['name'] == 'screenshot' for t in tools)
shot = success(rpc(box, 'tools/call', {'name': 'screenshot', 'arguments': {}}, session)[0])
assert any(c['type'] == 'image' and len(c.get('data', '')) > 100 for c in shot['content'])
print('PASS: dedicated desktop tools and real screenshot', flush=True)
try:
    denied = rpc('cb-b023b84c576f', 'initialize', {'protocolVersion': '2025-06-18',
        'capabilities': {}, 'clientInfo': {'name': 'isabel-isolation-check', 'version': '1'}})[0]
    assert 'error' in denied, 'SECURITY: main box was accessible'
except urllib.error.HTTPError as err:
    assert err.code in (401, 403, 404), err.code
print('PASS: isabel credential cannot access main box', flush=True)
denied = rpc('control', 'tools/call', {'name': 'box_status', 'arguments': {'box': 'cb-b023b84c576f'}})[0]
assert 'error' in denied or denied.get('result', {}).get('isError'), 'SECURITY: main lifecycle accessible'
print('PASS: isabel credential cannot manage main lifecycle', flush=True)
print(json.dumps({'box_id': box, 'tool_count': len(tools)}))
