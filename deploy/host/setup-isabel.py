"""Run as root on this deployment; keep all credentials on the host."""
import http.cookiejar
import json
import os
from pathlib import Path
import secrets
import urllib.request

os.umask(0o077)
root = Path('/etc/cocoonbox')
origin = 'https://cocoonbox.tailb695d.ts.net'
identity = 'chatgpt-isabel'

def client():
    return urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))

def api(c, method, path, body=None):
    req = urllib.request.Request(origin + '/cb/api/' + path, method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={'Content-Type': 'application/json', 'Origin': origin})
    with c.open(req, timeout=180) as response:
        raw = response.read()
        return json.loads(raw) if raw else None

admin = client()
api(admin, 'POST', 'login', {'name': 'lucas', 'password': (root/'initial-admin-password').read_text().strip()})
users = api(admin, 'GET', 'users')
existing = [u for u in users if u['name'] == identity]
password_file = root/'isabel-password'
if existing and not password_file.exists():
    raise RuntimeError('An unrelated service user exists; refusing to modify it')
if not password_file.exists():
    password_file.write_text(secrets.token_urlsafe(36) + '\n')
password = password_file.read_text().strip()
user = existing[0] if existing else api(admin, 'POST', 'users', {'name': identity, 'password': password})
api(admin, 'PATCH', 'users/' + user['id'], {'admin': False, 'max_boxes': 1, 'max_boxes_cap': 1, 'max_running': 1})
c = client()
api(c, 'POST', 'login', {'name': identity, 'password': password})
me = api(c, 'GET', 'me')
boxes = me['boxes']
if len(boxes) > 1:
    raise RuntimeError('Unexpected extra boxes; refusing to choose')
box = boxes[0] if boxes else api(c, 'POST', 'boxes', {'name': 'isabel'})
if box['name'] != 'isabel':
    box = api(c, 'PATCH', 'boxes/' + box['id'], {'name': 'isabel'})
token_file = root/'isabel-api-token'
if not token_file.exists():
    token_file.write_text(api(c, 'POST', 'me/token')['token'] + '\n')
metadata = {'user_id': user['id'], 'box_id': box['id'], 'name': 'isabel'}
(root/'isabel.json').write_text(json.dumps(metadata) + '\n')
for session in (admin, c):
    api(session, 'POST', 'logout', {})
print(json.dumps(metadata))
