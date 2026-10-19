#!/usr/bin/env python3
"""Host-side acceptance: retrieve every MCP document page and verify guest artifacts."""
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shlex
import sys

spec = importlib.util.spec_from_file_location('box_rpc', Path(__file__).with_name('box-rpc.py'))
box_rpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(box_rpc)

job_id = sys.argv[1]
if not re.fullmatch(r'[0-9a-f]{16}', job_id):
    raise ValueError('Expected a job ID')
destination = Path(sys.argv[2])


def extraction(params):
    response = box_rpc.call('bilibili_run', {'code': '', 'screenshot': False, 'params': params})
    report = json.loads(next(c['text'] for c in response['content'] if c['type'] == 'text'))
    if report.get('error') or report.get('exit_code') != 0:
        raise RuntimeError(report)
    return report['result']


box_rpc.connect()
state = extraction({'action': 'status', 'job_id': job_id})
if state['status'] != 'completed':
    print(json.dumps(state, ensure_ascii=False))
    sys.exit(2 if state['status'] in ('queued', 'running') else 1)
cached = extraction({'bilibili_link': state['bilibili_link'] +
                     ('&' if '?' in state['bilibili_link'] else '?') + 'utm_source=chatgpt.com',
                     'mode': state['mode'], 'region': state['region'],
                     'interval_seconds': state['interval_seconds']})
assert cached['job_id'] == job_id and cached['status'] == 'completed', 'Completed job was not reused'

pieces, offset, pages = [], 0, 0
total_characters = None
while True:
    result = extraction({'action': 'document', 'job_id': job_id, 'offset': offset, 'limit': 16000})
    assert result['status'] == 'completed', result
    if total_characters is None:
        total_characters = result['total_characters']
    assert result['total_characters'] == total_characters, 'Document changed during retrieval'
    pieces.append(result['document'])
    pages += 1
    next_offset = result['next_offset']
    if next_offset is None:
        break
    assert next_offset > offset, 'Pagination must advance'
    offset = next_offset
document = ''.join(pieces)
assert len(document) == total_characters, 'MCP document is truncated'
if state.get('method', '').startswith('ocr'):
    assert not re.search(r'^\s*(?:1\s*[xX]|回顾|隐藏|自动中|自动)\s*$', document, re.M)
document_hash = hashlib.sha256(document.encode()).hexdigest()

# Run checks next to the files, returning hashes and statistics rather than large text.
check = r'''
import base64,hashlib,html,json,re
from pathlib import Path
folder=Path('/workspace/video-extractions')/JOB_ID
state=json.loads((folder/'status.json').read_text())
rows=json.loads((folder/'segments.json').read_text())
md=(folder/'document.md').read_text()
markup=(folder/'document.html').read_text()
assert state['status']=='completed'
assert state['progress']==1
is_ocr=state.get('method','').startswith('ocr')
if is_ocr:
 assert abs(state['frames_processed']-state['frames_estimated'])<=1,'Unexpected frame coverage'
assert rows and len(rows)==state['segment_count']
assert md==(folder/'document.txt').read_text()
assert html.unescape(re.search(r'<pre>(.*?)</pre>',markup,re.S)[1])==md
if is_ocr:
 assert not re.search(r'^\s*(?:1\s*[xX]|回顾|隐藏|自动中|自动)\s*$',md,re.M)
last_start=last_end=0
for row in rows:
 assert 0<=row['start']<row['end']<=state['duration']+0.01,row
 assert row['start']>=last_start,row
 if is_ocr: assert row['start']>=last_end,row
 assert row['text'].strip(),row
 last_start,last_end=row['start'],row['end']
# The outro can contain only illustrations; frame coverage, not a forced text
# result near the end, establishes that the whole video was processed.
chinese_characters=sum(len(re.findall(r'[\u3400-\u9fff]',r['text'])) for r in rows)
assert len(re.findall(r'^## ',md,re.M))==len(rows)
summary={'ok':True,'job_id':JOB_ID,'duration_seconds':state['duration'],
 'frames_processed':state.get('frames_processed'),'frames_estimated':state.get('frames_estimated'),
 'chinese_characters':chinese_characters,
 'segments':len(rows),'first_text_seconds':rows[0]['start'],'last_text_seconds':rows[-1]['end'],
 'tail_without_recognized_text_seconds':state['duration']-rows[-1]['end'],
 'md_sha256':hashlib.sha256(md.encode()).hexdigest(),
 'html_sha256':hashlib.sha256(markup.encode()).hexdigest(),
 'html_bytes':len(markup.encode()),'filter_version':state.get('ocr_filter_version'),
 'samples':[rows[i] for i in (0,len(rows)//2,len(rows)-1)]}
print(json.dumps(summary,ensure_ascii=False))
'''.replace('JOB_ID', repr(job_id))
response = box_rpc.call('shell', {'command': 'python3 -c ' + shlex.quote(check), 'timeout_s': 30})
output = next(c['text'] for c in response['content'] if c['type'] == 'text')
summary = json.loads(output.splitlines()[0])
assert summary['md_sha256'] == document_hash, 'MCP content does not match the saved document'
summary.update(mcp_pages=pages, document_characters=total_characters, cached_start_verified=True)

# Fetch the exact printable artifact in bounded chunks through authenticated MCP.
html_parts = []
for offset in range(0, summary['html_bytes'], 12000):
    read = ("import base64; from pathlib import Path; p=Path('/workspace/video-extractions')/"
            + repr(job_id) + "/'document.html'; print(base64.b64encode(p.read_bytes()["
            + str(offset) + ':' + str(offset + 12000) + "]).decode())")
    response = box_rpc.call('shell', {'command': 'python3 -c ' + shlex.quote(read), 'timeout_s': 10})
    text = next(c['text'] for c in response['content'] if c['type'] == 'text')
    html_parts.append(base64.b64decode(text.splitlines()[0], validate=True))
markup = b''.join(html_parts)
assert len(markup) == summary['html_bytes']
assert hashlib.sha256(markup).hexdigest() == summary['html_sha256']
destination.mkdir(parents=True, exist_ok=True)
(destination / 'document.md').write_text(document)
(destination / 'document.html').write_bytes(markup)
(destination / 'validation.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2))
print(json.dumps(summary, ensure_ascii=False))
