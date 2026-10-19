#!/usr/bin/env python3
"""Real FFmpeg -> Tesseract -> document smoke test; run in the installed guest."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile

installed = Path('/home/user/.local/share/cocoonbox/video-extract/extract.py')
spec = importlib.util.spec_from_file_location('extract', installed)
extract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extract)
with tempfile.TemporaryDirectory(prefix='cocoonbox-ocr-smoke-') as directory:
    folder = Path(directory)
    video = folder / 'fixture.mp4'
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi',
                    '-i', 'color=c=black:s=1280x720:r=1:d=4', '-vf',
                    'drawtext=text=COCOONBOX OCR TEST 2026:fontcolor=white:fontsize=52:x=100:y=550',
                    '-c:v', 'libx264', '-pix_fmt', 'yuv420p', str(video)], check=True, timeout=30)
    state = {'status': 'running', 'bilibili_link': 'https://www.bilibili.com/video/BV1NzHfz7EA8/',
             'interval_seconds': 2, 'region': 'subtitles'}
    extract.atomic_json(folder / 'status.json', state)
    segments = extract.ocr(video, folder, 4, 2, 'subtitles')
    text = '\n'.join(row['text'] for row in segments)
    assert 'COCOONBOX' in text and '2026' in text, text
    assert len(segments) == 1, segments
    extract.write_document(folder, state, {'title': 'OCR smoke', 'uploader': 'fixture', 'part': 1,
                                          'duration': 4}, segments, 'ocr (smoke)')
    assert all((folder / name).stat().st_size for name in
               ['document.md', 'document.html', 'document.txt', 'segments.json'])
    print(json.dumps({'ok': True, 'recognized': text, 'segments': segments}, ensure_ascii=False))
