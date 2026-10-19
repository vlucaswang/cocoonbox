#!/usr/bin/env python3
"""CPU-only Bilibili captions/OCR. JSON jobs and Markdown documents, no LLM calls."""
import concurrent.futures
import difflib
import fcntl
import hashlib
import html
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

ROOT = Path(os.environ.get('CB_VIDEO_ROOT', '/workspace/video-extractions'))
HERE = Path(__file__).resolve().parent
MAX_DURATION = 4 * 3600
MAX_DOWNLOAD = 2 * 1024**3
OCR_LOCAL = threading.local()
OCR_UI_LABELS = frozenset(('1x', '回顾', '隐藏', '自动中', '自动'))
OCR_FILTER_VERSION = 1


def canonical_url(value):
    if not isinstance(value, str):
        raise ValueError('bilibili_link must be a Bilibili video URL')
    url = urllib.parse.urlsplit(value)
    match = re.fullmatch(r'/video/(BV[A-Za-z0-9]{10})/?', url.path)
    if (url.scheme != 'https' or url.hostname not in ('www.bilibili.com', 'bilibili.com')
            or url.port not in (None, 443) or url.username or url.password or not match):
        raise ValueError('Use https://www.bilibili.com/video/BV…/ (expand short links first)')
    query = urllib.parse.parse_qs(url.query)
    part = int(query.get('p', ['1'])[0])
    if part < 1:
        raise ValueError('p must be a positive video part number')
    return f'https://www.bilibili.com/video/{match[1]}/' + (f'?p={part}' if part > 1 else '')


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2))
    temporary.replace(path)


def discover(value):
    """List all P parts of this BV; never expand recommendations or collections."""
    url = canonical_url(value).split('?')[0]
    bvid = url.rstrip('/').rsplit('/', 1)[1]
    folder = ROOT / 'discovery'
    folder.mkdir(parents=True, exist_ok=True)
    cache = folder / (bvid + '.json')
    # Serialize only this short metadata lookup, not extraction jobs.
    with (folder / (bvid + '.lock')).open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if cache.exists() and time.time() - cache.stat().st_mtime < 600:
            return json.loads(cache.read_text())
        command = [sys.executable, str(HERE / 'yt-dlp'), '--ignore-config',
                   '--yes-playlist', '--flat-playlist', '--simulate',
                   '--ignore-no-formats-error', '--dump-single-json',
                   '--socket-timeout', '15', '--retries', '1']
        if (HERE / 'cookies.txt').exists():
            command += ['--cookies', str(HERE / 'cookies.txt')]
        result = subprocess.run(command + [url], capture_output=True, text=True, timeout=85)
        if result.returncode:
            raise RuntimeError('Part detection failed; do not assume this video has only one part. '
                               'The source may be unavailable or require login.')
        info = json.loads(result.stdout)
        entries = info.get('entries') if info.get('_type') == 'playlist' else [info]
        if not isinstance(entries, list) or not entries:
            raise ValueError('No video parts found')
        parts = []
        for index, entry in enumerate(entries, 1):
            if not isinstance(entry, dict):
                raise ValueError('Incomplete part list')
            part_url = canonical_url(entry.get('url') or entry.get('webpage_url') or url)
            if part_url.split('?')[0] != url:
                raise ValueError('Detected a different BV; collections are not video P parts')
            number = int(urllib.parse.parse_qs(urllib.parse.urlsplit(part_url).query).get('p', ['1'])[0])
            if number != index:
                raise ValueError('Part list is incomplete or out of order')
            parts.append({'part': number, 'title': entry.get('title') or f'P{number}',
                          'bilibili_link': part_url})
        data = {'bvid': bvid, 'title': info.get('title') or bvid,
                'part_count': len(parts), 'parts': parts,
                'next_action': 'Start each parts[].bilibili_link in order. Complete and read all document pages for each job before the next part.'}
        atomic_json(cache, data)
        return data


def update(folder, **values):
    state = json.loads((folder / 'status.json').read_text())
    state.update(values, updated_at=time.time())
    atomic_json(folder / 'status.json', state)
    return state


def metadata(url, folder=None, want_captions=True):
    # Let the maintained extractor initialize Bilibili's normal anonymous session.
    # A bare API request can receive HTTP 412 even for a publicly playable video.
    cache = folder / 'source.info.json' if folder else None
    command = [sys.executable, str(HERE / 'yt-dlp'), '--ignore-config', '--no-playlist',
               '--simulate', '--dump-single-json', '--socket-timeout', '30', '--retries', '1']
    if want_captions:
        command += ['--write-subs', '--sub-langs', 'all,-danmaku']
    command.append(url)
    if (HERE / 'cookies.txt').exists():
        command[2:2] = ['--cookies', str(HERE / 'cookies.txt')]
    if cache and cache.exists() and time.time() - cache.stat().st_mtime < 1800:
        info = json.loads(cache.read_text())
    else:
        result = subprocess.run(command, capture_output=True, text=True, timeout=120)
        first_log = result.stderr
        if result.returncode and want_captions:
            # Captions can require a sign-in even when anonymous video playback works.
            command.remove('--write-subs')
            result = subprocess.run(command, capture_output=True, text=True, timeout=120)
            first_log += '\nCaption lookup failed; retrying metadata without captions.\n' + result.stderr
        if folder:
            (folder / 'metadata.log').write_text(first_log)
        if result.returncode:
            raise RuntimeError('Bilibili metadata unavailable through yt-dlp. The source may require '
                               'login or be unavailable; see metadata.log in the job directory.')
        info = json.loads(result.stdout)
        if cache:
            atomic_json(cache, info)
    part = int(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get('p', ['1'])[0])
    duration = float(info.get('duration') or 0)
    if not 0 < duration <= MAX_DURATION:
        raise ValueError('This workflow supports videos up to four hours per part')
    return {'bvid': re.search(r'/video/(BV[A-Za-z0-9]{10})', url)[1],
            'title': info['title'], 'part': part, 'duration': duration,
            'uploader': info.get('uploader', ''), '_subtitles': info.get('subtitles', {})}


def captions(info, url):
    tracks = info.get('_subtitles', {})
    for language in sorted(tracks, key=lambda key: (not key.startswith('zh'), key)):
        if language == 'danmaku':
            continue
        for track in tracks[language]:
            if track.get('ext') == 'srt' and track.get('data'):
                segments = parse_srt(track['data'])
                if segments:
                    return segments, language
    return [], ''


def parse_srt(text):
    segments = []
    pattern = r'(\d+):(\d+):(\d+)[,.](\d+)\s+-->\s+(\d+):(\d+):(\d+)[,.](\d+)'
    for block in re.split(r'\n\s*\n', text.replace('\r', '').strip()):
        match = re.search(pattern, block)
        if not match:
            continue
        h, m, s, ms, eh, em, es, ems = map(int, match.groups())
        content = html.unescape(re.sub(r'<[^>]*>', '', block[match.end():].strip()))
        if content:
            segments.append({'start': h * 3600 + m * 60 + s + ms / 1000,
                             'end': eh * 3600 + em * 60 + es + ems / 1000, 'text': content})
    return segments


def download(url, folder):
    program = HERE / 'yt-dlp'
    if not program.exists():
        raise RuntimeError('yt-dlp is missing; run install.sh')
    output = folder / 'video.%(ext)s'
    command = [sys.executable, str(program), '--ignore-config', '--no-playlist', '--no-progress',
               '--socket-timeout', '30', '--retries', '2', '--fragment-retries', '2',
               '--http-chunk-size', '10M',
               '--max-filesize', str(MAX_DOWNLOAD), '--format',
               'bestvideo[width<=1280][height<=1280][vcodec^=avc1]/'
               'bestvideo[width<=1280][height<=1280]/best[width<=1280][height<=1280]/bestvideo/best',
               '--output', str(output), '--print', 'after_move:filepath']
    if (folder / 'source.info.json').exists():
        command += ['--load-info-json', str(folder / 'source.info.json')]
    else:
        command.append(url)
    # Optional cookies are provided by the operator as a local file, never returned to ChatGPT.
    cookie_file = HERE / 'cookies.txt'
    if cookie_file.exists():
        command[2:2] = ['--cookies', str(cookie_file)]
    with (folder / 'download.log').open('w') as log:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=log, text=True)
        deadline = time.monotonic() + 1800
        try:
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise RuntimeError('Video download exceeded 30 minutes')
                size = sum(p.stat().st_size for p in folder.glob('video.*') if p.is_file())
                if size > MAX_DOWNLOAD:
                    raise RuntimeError('Video download exceeded 2 GiB')
                update(folder, downloaded_bytes=size)
                time.sleep(1)
            stdout = process.communicate()[0]
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    if process.returncode:
        raise RuntimeError('Bilibili download failed. Login, regional availability, or site changes may '
                           'be involved; see download.log in the job directory. No access restrictions were bypassed.')
    paths = [Path(line) for line in stdout.splitlines() if Path(line).is_file()]
    if not paths:
        raise RuntimeError('Downloader returned no video file (possibly over the size limit)')
    return paths[-1]


def normalized(text):
    return re.sub(r'\W+', '', text).casefold()


def clean_ocr_text(text):
    """Drop standalone player controls while preserving words inside dialogue."""
    return '\n'.join(line.strip() for line in text.splitlines()
                     if line.strip() and re.sub(r'\s+', '', line).casefold() not in OCR_UI_LABELS)


def merge_segments(segments, start, end, text):
    text = clean_ocr_text(text)
    key = normalized(text)
    if not key:
        return
    if segments and start <= segments[-1]['end'] + 0.05:
        previous = normalized(segments[-1]['text'])
        if key == previous or difflib.SequenceMatcher(None, previous, key).ratio() >= 0.94:
            segments[-1]['end'] = end
            return
    segments.append({'start': start, 'end': end, 'text': text})


def recognize(frame):
    if importlib.util.find_spec('rapidocr_onnxruntime'):
        from rapidocr_onnxruntime import RapidOCR
        if not hasattr(OCR_LOCAL, 'engine'):
            OCR_LOCAL.engine = RapidOCR(intra_op_num_threads=1, inter_op_num_threads=1,
                                       text_score=0.75)
        rows, _ = OCR_LOCAL.engine(str(frame))
        return '\n'.join(row[1] for row in rows or [] if row[2] >= 0.75)
    env = dict(os.environ, OMP_THREAD_LIMIT='1')
    result = subprocess.run(['tesseract', str(frame), 'stdout', '-l', 'chi_sim+eng', '--psm', '6'],
                            capture_output=True, text=True, timeout=30, env=env)
    if result.returncode:
        raise RuntimeError('Tesseract failed: ' + result.stderr[-500:])
    return result.stdout


def ocr(video, folder, duration, interval, region):
    segments = []
    processed = 0
    total = math.ceil(duration / interval)
    # Small chunks bound temporary storage; two single-thread OCR workers leave the desktop usable.
    with tempfile.TemporaryDirectory(prefix='frames-', dir=folder) as scratch, \
            concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        frames = Path(scratch)
        for chunk_start in range(0, math.ceil(duration), 60):
            chunk_length = min(60, duration - chunk_start)
            crop = 'crop=iw:ih*0.38:0:ih*0.62,' if region == 'subtitles' else ''
            vf = f'fps=1/{interval},' + crop + "scale=1600:-2"
            result = subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-ss',
                                     str(chunk_start), '-i', str(video), '-t', str(chunk_length),
                                     '-vf', vf, '-q:v', '3', '-threads', '1', str(frames / '%06d.jpg')],
                                    capture_output=True, timeout=180)
            if result.returncode:
                raise RuntimeError('FFmpeg frame extraction failed: ' + result.stderr.decode(errors='replace')[-500:])
            images = sorted(frames.glob('*.jpg'))
            for index, text in enumerate(pool.map(recognize, images)):
                start = chunk_start + index * interval
                merge_segments(segments, start, min(start + interval, duration), text)
                processed += 1
            for frame in images:
                frame.unlink()
            update(folder, phase='ocr', frames_processed=processed, frames_estimated=total,
                   progress=min(0.99, (chunk_start + chunk_length) / duration))
            # Keep an honest partial result if processing is interrupted.
            atomic_json(folder / 'segments.partial.json', segments)
    return segments


def stamp(seconds):
    seconds = int(seconds)
    return f'{seconds // 3600:02}:{seconds // 60 % 60:02}:{seconds % 60:02}'


def write_document(folder, state, info, segments, method):
    if method.startswith('ocr'):
        cleaned = []
        for segment in segments:
            merge_segments(cleaned, segment['start'], segment['end'], segment['text'])
        segments = cleaned
    lines = [f'# {info["title"]}', '', f'Source: {state["bilibili_link"]}',
             f'Uploader: {info["uploader"]}', f'Video part: {info["part"]}',
             f'Duration: {stamp(info["duration"])}', f'Extraction: {method}', '']
    if method.startswith('ocr'):
        lines += [f'OCR samples one frame every {state["interval_seconds"]} seconds '
                  f'({state["region"]} region). Recognition may contain errors and miss text between samples. '
                  'This is visible text, not an audio transcript.', '']
    else:
        lines += ['These are the published caption track, not all text visible on screen.', '']
    for segment in segments:
        lines += [f'## {stamp(segment["start"])} – {stamp(segment["end"])}', '', segment['text'], '']
    text = '\n'.join(lines)
    (folder / 'document.md').write_text(text)
    (folder / 'document.txt').write_text(text)
    # A standalone printable document preserves Chinese text without office dependencies.
    (folder / 'document.html').write_text('<!doctype html><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width"><title>' + html.escape(info['title'])
        + '</title><style>body{max-width:52em;margin:3em auto;padding:0 1em;font:18px/1.7 sans-serif;}'
        'pre{white-space:pre-wrap;overflow-wrap:anywhere;font:inherit}</style><pre>'
        + html.escape(text) + '</pre>')
    atomic_json(folder / 'segments.json', segments)
    return segments


def worker(folder):
    state = update(folder, status='running', phase='metadata', pid=os.getpid())
    try:
        info = metadata(state['bilibili_link'], folder, want_captions=state['mode'] != 'ocr')
        update(folder, **{key: value for key, value in info.items() if not key.startswith('_')})
        segments = []
        language = ''
        if state['mode'] in ('auto', 'captions'):
            update(folder, phase='captions')
            try:
                segments, language = captions(info, state['bilibili_link'])
            except Exception as error:
                update(folder, caption_warning=str(error))
        if not segments:
            if state['mode'] == 'captions':
                raise RuntimeError('No accessible caption track. Use mode="ocr" for visible text.')
            update(folder, phase='download')
            video = download(state['bilibili_link'], folder)
            update(folder, phase='ocr', progress=0)
            segments = ocr(video, folder, info['duration'], state['interval_seconds'], state['region'])
            method = ('ocr (RapidOCR ONNX CPU, Chinese+English)' if importlib.util.find_spec('rapidocr_onnxruntime')
                      else 'ocr (Tesseract chi_sim+eng)')
        else:
            method = 'captions (' + language + ')'
        if not segments:
            raise RuntimeError('No text was recognized. No completed document was produced.')
        segments = write_document(folder, state, info, segments, method)
        update(folder, status='completed', phase='done', progress=1, method=method,
               ocr_filter_version=OCR_FILTER_VERSION,
               segment_count=len(segments), document_path=str(folder / 'document.md'),
               html_path=str(folder / 'document.html'), segments_path=str(folder / 'segments.json'))
    except Exception as error:
        update(folder, status='failed', phase='error', error=str(error))
    finally:
        # Media is temporary; documents, status, and diagnostics survive box snapshots.
        for media in folder.glob('video.*'):
            if media.is_file():
                media.unlink()


def alive(state):
    try:
        cmdline = Path(f'/proc/{int(state.get("pid", 0))}/cmdline').read_bytes()
        return b'--worker' in cmdline and state['job_id'].encode() in cmdline
    except (OSError, ValueError):
        return False


def status(job_id):
    if not isinstance(job_id, str) or not re.fullmatch(r'[0-9a-f]{16}', job_id):
        raise ValueError('job_id must be the ID returned by start')
    folder = ROOT / job_id
    state = json.loads((folder / 'status.json').read_text())
    # Upgrade documents from a worker that was already running when the filter changed.
    if (state['status'] == 'completed' and state.get('method', '').startswith('ocr')
            and state.get('ocr_filter_version') != OCR_FILTER_VERSION):
        segments = json.loads((folder / 'segments.json').read_text())
        segments = write_document(folder, state, state, segments, state['method'])
        state = update(folder, segment_count=len(segments), ocr_filter_version=OCR_FILTER_VERSION)
    starting = state['status'] == 'queued' and time.time() - state['created_at'] < 10
    if state['status'] in ('queued', 'running') and not starting and not alive(state):
        state = update(folder, status='failed', error='Worker stopped; retry with retry=true. Partial OCR, if any, remains in segments.partial.json.')
    return state


def main(params):
    action = params.get('action', 'start')
    if action == 'discover':
        return discover(params.get('bilibili_link'))
    if action in ('status', 'document'):
        state = status(params.get('job_id'))
        if action == 'document' and state['status'] == 'completed':
            content = Path(state['document_path']).read_text()
            offset = int(params.get('offset', 0))
            if offset < 0 or offset > len(content):
                raise ValueError('offset is outside the document')
            limit = min(24000, max(1, int(params.get('limit', 16000))))
            state['document'] = content[offset:offset + limit]
            state['next_offset'] = offset + limit if offset + limit < len(content) else None
            state['total_characters'] = len(content)
        return state
    if action != 'start':
        raise ValueError('action must be discover, start, status, or document')
    url = canonical_url(params.get('bilibili_link'))
    mode = params.get('mode', 'auto')
    region = params.get('region', 'full')
    interval = float(params.get('interval_seconds', 2))
    if mode not in ('auto', 'captions', 'ocr') or region not in ('full', 'subtitles'):
        raise ValueError('mode: auto|captions|ocr; region: full|subtitles')
    if interval not in (1, 2, 3, 5, 10):
        raise ValueError('interval_seconds must be 1, 2, 3, 5, or 10')
    # Settings form the cache key; tracking links do not start duplicate jobs.
    key = json.dumps([url, mode, region, interval])
    job_id = hashlib.sha256(key.encode()).hexdigest()[:16]
    ROOT.mkdir(parents=True, exist_ok=True)
    with (ROOT / '.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        folder = ROOT / job_id
        if (folder / 'status.json').exists():
            existing = status(job_id)
            if existing['status'] != 'failed' or not params.get('retry', False):
                return existing
        for path in ROOT.glob('*/status.json'):
            other = status(path.parent.name)
            if other['status'] in ('queued', 'running'):
                raise RuntimeError(f'An extraction is already running: {other["job_id"]}. Poll it before starting another.')
        folder.mkdir(exist_ok=True)
        state = {'job_id': job_id, 'status': 'queued', 'phase': 'starting', 'bilibili_link': url,
                 'mode': mode, 'region': region, 'interval_seconds': interval, 'created_at': time.time()}
        atomic_json(folder / 'status.json', state)
        with (folder / 'worker.log').open('a') as log:
            python = HERE / 'venv/bin/python'
            process = subprocess.Popen([str(python) if python.exists() else sys.executable,
                                        str(Path(__file__).resolve()), '--worker', str(folder)],
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        # Do not race a worker update by rewriting its state after spawning it.
        state['pid'] = process.pid
        state['next_action'] = 'Poll action=status with this job_id; on completion use action=document.'
        return state


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--worker':
        worker(Path(sys.argv[2]))
    else:
        try:
            params = json.loads(os.environ.get('CB_PARAMS', '{}') if 'CB_PARAMS' in os.environ else sys.argv[1])
            result = main(params)
            if 'CB_RESULT' in os.environ:
                Path(os.environ['CB_RESULT']).write_text(json.dumps(result, ensure_ascii=False))
            else:
                print(json.dumps(result, ensure_ascii=False))
        except Exception as error:
            print(str(error), file=sys.stderr)
            sys.exit(1)
