#!/usr/bin/env python3
import base64
from collections import deque
from concurrent.futures import ThreadPoolExecutor
import fcntl
import hashlib
import html
import http.client
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(os.environ.get('CB_VIDEO_ROOT', '/workspace/video-extractions'))
HERE = Path(__file__).resolve().parent
MAX_DURATION = 4 * 3600
MAX_DOWNLOAD = 2 * 1024**3
DEFAULT_OCR_BASE_URL = 'http://192.168.10.109:12340/v1'
DEFAULT_OCR_MODEL = 'PaddleOCR-VL-1.6-MLX-8bit'
OCR_PROTOCOL_VERSION = 6
OCR_MAX_TOKENS = 4096
OCR_REPETITION_PENALTY = 1.3
OCR_MAX_CONCURRENCY = 4
OCR_TIMEOUT = 120
OCR_RESPONSE_LIMIT = 2 * 1024 * 1024


class OCRTokenLimitError(RuntimeError):
    pass


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


def clean_ocr_text(text):
    """Drop standalone player controls while preserving words inside dialogue."""
    return '\n'.join(line.strip() for line in text.splitlines()
                     if line.strip() and re.sub(r'\s+', '', line).casefold() not in OCR_UI_LABELS)


def merge_segments(segments, start, end, text):
    text = clean_ocr_text(text)
    if not text:
        return
    if (segments and start <= segments[-1]['end'] + 0.05
            and text == segments[-1]['text']):
        segments[-1]['end'] = end
        return
    segments.append({'start': start, 'end': end, 'text': text})


def ocr_settings():
    base_url = os.environ.get('CB_OCR_BASE_URL', DEFAULT_OCR_BASE_URL).strip().rstrip('/')
    model = os.environ.get('CB_OCR_MODEL', DEFAULT_OCR_MODEL).strip()
    url = urllib.parse.urlsplit(base_url)
    if (url.scheme not in ('http', 'https') or not url.hostname or url.username is not None
            or url.password is not None or url.query or url.fragment
            or any(char.isspace() for char in base_url)):
        raise ValueError('CB_OCR_BASE_URL must be an HTTP(S) base URL without credentials, query, or fragment')
    try:
        url.port
    except ValueError as error:
        raise ValueError('CB_OCR_BASE_URL has an invalid port') from error
    if not model:
        raise ValueError('CB_OCR_MODEL must not be empty')
    return {'base_url': base_url, 'model': model, 'protocol_version': OCR_PROTOCOL_VERSION}


def ocr_error_detail(text):
    return re.sub(r'data:image/[^\s"\\]+', '[image omitted]', text)[:500]


def recognize(frame, settings):
    endpoint = settings['base_url'] + '/chat/completions'
    model = settings['model']
    context = f'Remote OCR at {endpoint} using model {model}'
    api_key = os.environ.get('CB_OCR_API_KEY')
    if api_key is None:
        try:
            api_key = (HERE / 'ocr-api-key').read_text().strip()
        except FileNotFoundError:
            api_key = ''
    api_key = api_key.strip()
    if any(char in api_key for char in '\r\n'):
        raise ValueError('OCR API key must not contain line breaks')
    headers = {'Content-Type': 'application/json'}
    if api_key:
        headers['Authorization'] = 'Bearer ' + api_key

    def safe_detail(text):
        if api_key:
            text = text.replace(api_key, '[key omitted]')
        return ocr_error_detail(text)

    image = base64.b64encode(frame.read_bytes()).decode('ascii')
    payload = {'model': model, 'temperature': 0, 'stream': False, 'max_tokens': OCR_MAX_TOKENS,
               'repetition_penalty': OCR_REPETITION_PENALTY,
               'messages': [{'role': 'user', 'content': [
                   {'type': 'image_url', 'image_url': {'url': 'data:image/jpeg;base64,' + image}},
                   {'type': 'text', 'text': 'OCR:'}]}]}
    request = urllib.request.Request(endpoint, data=json.dumps(payload).encode('utf-8'),
                                     headers=headers, method='POST')
    try:
        with urllib.request.urlopen(request, timeout=OCR_TIMEOUT) as response:
            raw = response.read(OCR_RESPONSE_LIMIT + 1)
    except urllib.error.HTTPError as error:
        with error:
            try:
                detail = safe_detail(error.read(4096).decode('utf-8', errors='replace'))
            except (OSError, http.client.HTTPException):
                detail = 'Could not read the server error body'
        raise RuntimeError(f'{context} failed with HTTP {error.code}: {detail}. '
                           'Check the server supports image input and the configured model.') from error
    except (OSError, urllib.error.URLError, http.client.HTTPException) as error:
        raise RuntimeError(f'{context} could not complete the request: {safe_detail(str(error))}. '
                           'Check the server, proxy, and exact host/port egress policy.') from error
    if len(raw) > OCR_RESPONSE_LIMIT:
        raise RuntimeError(f'{context} returned a response larger than {OCR_RESPONSE_LIMIT} bytes')
    try:
        result = json.loads(raw)
    except (ValueError, UnicodeError) as error:
        raise RuntimeError(f'{context} returned invalid JSON') from error
    if not isinstance(result, dict):
        raise RuntimeError(f'{context} returned an invalid response object')
    if result.get('error'):
        detail = safe_detail(json.dumps(result['error'], ensure_ascii=False))
        raise RuntimeError(f'{context} returned an API error: {detail}. Check the server image support and model.')
    choices = result.get('choices')
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
        raise RuntimeError(f'{context} returned invalid or missing choices')
    choice = choices[0]
    message = choice.get('message')
    if not isinstance(message, dict):
        raise RuntimeError(f'{context} returned an invalid or missing message')
    if message.get('refusal') or choice.get('finish_reason') == 'content_filter':
        raise RuntimeError(f'{context} refused image recognition. Check the server and model.')
    if choice.get('finish_reason') == 'length':
        content = message.get('content')
        preview = repr(safe_detail(content))[:500] if isinstance(content, str) else '[unavailable]'
        raise OCRTokenLimitError(f'{context} truncated the recognized text with client max_tokens={OCR_MAX_TOKENS}. '
                           f'Check the server token limit before retrying. Output preview: {preview}')
    if choice.get('finish_reason') != 'stop':
        raise RuntimeError(f'{context} returned an incomplete or unsupported finish_reason')
    text = message.get('content')
    if (text is None and isinstance(result.get('usage'), dict)
            and result['usage'].get('completion_tokens') == 0
            and not message.get('tool_calls') and not message.get('function_call')):
        return ''
    if not isinstance(text, str):
        raise RuntimeError(f'{context} returned missing or non-string message content')
    return text


def tile_bounds(width, height):
    return [(0, 0, width, (3 * height + 4) // 5),
            (0, height // 2, width, height)]


def recognize_frame(images, bounds, settings):
    reading = {'tiles': [], 'text': '', 'complete': False}
    for image, bound in zip(images, bounds):
        tile = {'bounds': bound}
        reading['tiles'].append(tile)
        try:
            try:
                tile['raw_text'] = recognize(image, settings)
            except OCRTokenLimitError as error:
                if bound is None:
                    raise
                width, height = bound[2] - bound[0], bound[3] - bound[1]
                children = tile_bounds(width, height)
                paths = [image.with_name(f'{image.stem}-recovery-{index}.jpg') for index in range(2)]
                filters = ('split=2[a][b];'
                           f'[a]crop={width}:{children[0][3]}:0:0:exact=1[t];'
                           f'[b]crop={width}:{height-children[1][1]}:0:{children[1][1]}:exact=1[b]')
                command = ['ffmpeg', '-hide_banner', '-loglevel', 'error', '-i', str(image),
                           '-filter_complex', filters]
                for label, path in zip(('[t]', '[b]'), paths):
                    command += ['-map', label, '-frames:v', '1', '-q:v', '3', '-threads', '1', str(path)]
                result = subprocess.run(command, capture_output=True, timeout=30)
                if result.returncode or not all(path.is_file() for path in paths):
                    raise RuntimeError('FFmpeg could not split a truncated OCR tile') from error
                tile['recovery'] = {'reason': ocr_error_detail(str(error)), 'tiles': []}
                for path, child in zip(paths, children):
                    observation = {'bounds': [bound[0] + child[0], bound[1] + child[1],
                                               bound[0] + child[2], bound[1] + child[3]]}
                    tile['recovery']['tiles'].append(observation)
                    observation['raw_text'] = recognize(path, settings)
                tile['raw_text'] = '\n'.join(child['raw_text'] for child in tile['recovery']['tiles'])
        except Exception as error:
            tile['error'] = ocr_error_detail(str(error))
            reading['error'] = tile['error']
            return reading
        reading['text'] = '\n'.join(tile['raw_text'] for tile in reading['tiles'])
    reading['complete'] = True
    return reading


def ocr(video, folder, duration, interval, region, settings):
    segments = []
    processed = 0
    total = max(1, math.floor(duration / interval + 1e-9))
    bounds = [None]
    if region == 'full':
        probe = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v:0',
                                '-show_entries', 'stream=width,height,duration', '-of', 'json', str(video)],
                               capture_output=True, timeout=30)
        if probe.returncode:
            raise RuntimeError('FFprobe could not determine native frame dimensions')
        stream = json.loads(probe.stdout)['streams'][0]
        width, height = stream['width'], stream['height']
        if width <= 0 or height <= 0:
            raise RuntimeError('Invalid native frame dimensions')
        # Container duration can include an audio tail after the video track ends.
        # Sample only the visible stream, while keeping metadata duration in the document.
        try:
            video_duration = float(stream.get('duration'))
        except (TypeError, ValueError):
            video_duration = duration
        if video_duration <= 0:
            raise RuntimeError('FFprobe returned an invalid video stream duration')
        if video_duration < duration:
            duration = video_duration
            total = max(1, math.floor(duration / interval + 1e-9))
            update(folder, video_duration=duration, frames_estimated=total)
        bounds = tile_bounds(width, height)
    ledger = folder / 'frames.raw.jsonl'
    prefix = []
    if ledger.exists():
        for line in ledger.read_text().splitlines():
            reading = json.loads(line)
            if not reading.get('complete'):
                break
            start = processed * interval
            if (reading.get('protocol_version') != OCR_PROTOCOL_VERSION
                    or reading.get('start') != start
                    or reading.get('end') != min(start + interval, duration)
                    or [tile.get('bounds') for tile in reading.get('tiles', [])]
                    != [list(bound) if bound is not None else None for bound in bounds]
                    or not isinstance(reading.get('text'), str)):
                raise RuntimeError('Saved OCR evidence does not match this job; cannot resume safely')
            merge_segments(segments, start, reading['end'], reading['text'])
            prefix.append(line)
            processed += 1
        os.link(ledger, folder / f'frames.raw.previous-{time.time_ns()}.jsonl')
    prepared_ledger = ledger.with_suffix('.jsonl.resume.tmp')
    prepared_ledger.write_text(''.join(line + '\n' for line in prefix))
    prepared_ledger.replace(ledger)
    with tempfile.TemporaryDirectory(prefix='frames-', dir=folder) as scratch, ledger.open('a') as evidence:
        if prefix:
            atomic_json(folder / 'segments.partial.json', segments)
            update(folder, phase='ocr', frames_processed=processed, frames_estimated=total,
                   progress=min(0.99, processed / total))
        frames = Path(scratch)
        for chunk_start in range(int(processed * interval), math.ceil(duration), 60):
            chunk_length = min(60, duration - chunk_start)
            command = ['ffmpeg', '-hide_banner', '-loglevel', 'error', '-ss',
                       str(chunk_start), '-t', str(chunk_length), '-noautorotate', '-i', str(video)]
            if region == 'full':
                top, bottom = bounds
                filters = (f'fps=1/{interval}:start_time=0:eof_action=pass,'
                           f'trim=duration={chunk_length},split=2[top][bottom];'
                           f'[top]crop={width}:{top[3]}:0:0:exact=1[t];'
                           f'[bottom]crop={width}:{height-bottom[1]}:0:{bottom[1]}:exact=1[b]')
                command += ['-filter_complex', filters]
                for label, name in (('[t]', 'top'), ('[b]', 'bottom')):
                    command += ['-map', label, '-q:v', '3', '-threads', '1',
                                str(frames / (name + '-%06d.jpg'))]
            else:
                command += ['-vf', f'fps=1/{interval},crop=iw:ih*0.38:0:ih*0.62,scale=1600:-2',
                            '-q:v', '3', '-threads', '1', str(frames / '%06d.jpg')]
            result = subprocess.run(command, capture_output=True, timeout=180)
            if result.returncode:
                raise RuntimeError('FFmpeg frame extraction failed: ' + result.stderr.decode(errors='replace')[-500:])
            if region == 'full':
                tops = sorted(frames.glob('top-*.jpg'))
                bottoms = sorted(frames.glob('bottom-*.jpg'))
                if not tops or [p.name[4:] for p in tops] != [p.name[7:] for p in bottoms]:
                    raise RuntimeError('FFmpeg produced missing or mismatched frame tile pairs')
                # fps emits one image per complete interval; a fractional tail adds no sample.
                expected_count = max(1, math.floor(chunk_length / interval + 1e-9))
                expected = [f'{index:06d}.jpg' for index in range(1, expected_count + 1)]
                if [p.name[4:] for p in tops] != expected:
                    raise RuntimeError('FFmpeg sampled frame count or order does not cover the requested chunk')
                images = list(zip(tops, bottoms))
            else:
                images = [(frame,) for frame in sorted(frames.glob('*.jpg'))]
                if not images:
                    raise RuntimeError('FFmpeg produced no sampled frames')
            failure = None
            with ThreadPoolExecutor(max_workers=OCR_MAX_CONCURRENCY) as executor:
                remaining = iter(enumerate(images[OCR_MAX_CONCURRENCY:], OCR_MAX_CONCURRENCY))
                pending = deque((index, executor.submit(recognize_frame, pair, bounds, settings))
                                for index, pair in enumerate(images[:OCR_MAX_CONCURRENCY]))
                try:
                    while pending:
                        index, future = pending.popleft()
                        reading = future.result()
                        start = chunk_start + index * interval
                        reading.update(start=start, end=min(start + interval, duration),
                                       protocol_version=OCR_PROTOCOL_VERSION)
                        evidence.write(json.dumps(reading, ensure_ascii=False) + '\n')
                        evidence.flush()
                        if not reading['complete'] and failure is None:
                            failure = reading['error']
                        if failure is None:
                            merge_segments(segments, start, reading['end'], reading['text'])
                            processed += 1
                            next_frame = next(remaining, None)
                            if next_frame is not None:
                                next_index, pair = next_frame
                                pending.append((next_index, executor.submit(recognize_frame, pair, bounds, settings)))
                finally:
                    atomic_json(folder / 'segments.partial.json', segments)
                    update(folder, phase='ocr', frames_processed=processed, frames_estimated=total,
                           progress=min(0.99, processed / total))
            if failure is not None:
                raise RuntimeError(failure)
            for frame in frames.glob('*.jpg'):
                frame.unlink()
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
            settings = state.get('ocr_settings')
            if (not isinstance(settings, dict)
                    or not settings.get('base_url') or not settings.get('model')
                    or settings.get('protocol_version') != OCR_PROTOCOL_VERSION):
                raise RuntimeError('This job has no compatible remote OCR settings. Start a new job with the updated extractor.')
            update(folder, phase='download')
            video = download(state['bilibili_link'], folder)
            update(folder, phase='ocr', progress=0)
            segments = ocr(video, folder, info['duration'], state['interval_seconds'], state['region'], settings)
            method = 'ocr (remote ' + settings['model'] + ')'
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
    settings = ocr_settings() if mode != 'captions' else None
    identity = [url, mode, region, interval]
    key = (json.dumps(identity + [settings], sort_keys=True, separators=(',', ':'))
           if settings is not None else json.dumps(identity))
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
                 'mode': mode, 'region': region, 'interval_seconds': interval, 'created_at': time.time(),
                 'ocr_settings': settings}
        atomic_json(folder / 'status.json', state)
        with (folder / 'worker.log').open('a') as log:
            process = subprocess.Popen([sys.executable,
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
