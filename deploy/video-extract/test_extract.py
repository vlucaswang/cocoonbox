import base64
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('extract', Path(__file__).with_name('extract.py'))
extract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extract)
URL = 'https://www.bilibili.com/video/BV1NzHfz7EA8/'


def sample_frames(command, count):
    if command[0] == 'ffprobe':
        return subprocess.CompletedProcess(command, 0, stdout=b'{"streams":[{"width":583,"height":1281}]}')
    for target in (Path(arg) for arg in command if '%06d' in arg):
        for index in range(1, count + 1):
            marker = 255 if target.name.startswith('bottom-') else index - 1
            Path(str(target).replace('%06d', f'{index:06}')).write_bytes(JPEG + bytes([marker]))
    return subprocess.CompletedProcess(command, 0)


class ExtractionTests(unittest.TestCase):
    def test_full_ocr_uses_video_stream_duration_when_container_has_an_audio_tail(self):
        def sample(command, **kwargs):
            if command[0] == 'ffprobe':
                return subprocess.CompletedProcess(command, 0,
                    stdout=b'{"streams":[{"width":583,"height":1281,"duration":"2.5"}]}')
            return sample_frames(command, 2)

        def recognize(frame, settings):
            return '可见文字' if frame.name.startswith('top-') else ''

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=sample), \
                patch.object(extract, 'recognize', side_effect=recognize):
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {})
            segments = extract.ocr(Path('video.mp4'), folder, 4, 1, 'full', {})
            state = json.loads((folder / 'status.json').read_text())
            rows = [json.loads(line) for line in (folder / 'frames.raw.jsonl').read_text().splitlines()]

        self.assertEqual(segments, [{'start': 0, 'end': 2, 'text': '可见文字'}])
        self.assertEqual([row['start'] for row in rows], [0, 1])
        self.assertEqual(state['video_duration'], 2.5)
        self.assertEqual(state['frames_estimated'], 2)

    def test_discovery_enumerates_all_parts_even_from_part_specific_link(self):
        info = {'_type': 'playlist', 'title': '两集', 'entries': [
            {'url': URL + '?p=1'}, {'url': URL + '?p=2', 'title': '第二集'}]}
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'run') as run:
            run.return_value.returncode = 0
            run.return_value.stdout = json.dumps(info)
            result = extract.main({'action': 'discover', 'bilibili_link': URL + '?p=2'})
            self.assertEqual(result['part_count'], 2)
            self.assertEqual(result['parts'][1]['title'], '第二集')
            self.assertEqual(result['parts'][0]['bilibili_link'], URL)
            self.assertEqual(result['parts'][1]['bilibili_link'], URL + '?p=2')
            self.assertEqual(run.call_args.args[0][-1], URL)
            self.assertEqual(extract.discover(URL), result)
            self.assertEqual(run.call_count, 1)

    def test_single_video_discovery(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'run') as run:
            run.return_value.returncode = 0
            run.return_value.stdout = json.dumps({'title': '单集', 'webpage_url': URL})
            result = extract.discover(URL)
            self.assertEqual(result['part_count'], 1)
            self.assertEqual(result['parts'][0]['title'], '单集')

    def test_discovery_never_silently_truncates_or_calls_failure_a_single_video(self):
        bad = [
            {'_type': 'playlist', 'entries': []},
            {'_type': 'playlist', 'entries': [{'url': URL}, {'url': URL + '?p=3'}]},
            {'_type': 'playlist', 'entries': [{'url': URL.replace('BV1NzHfz7EA8', 'BV1xx411c7mD')}]},
        ]
        for info in bad:
            with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                    patch.object(extract.subprocess, 'run') as run:
                run.return_value.returncode = 0
                run.return_value.stdout = json.dumps(info)
                with self.assertRaises(ValueError):
                    extract.discover(URL)
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'run') as run:
            run.return_value.returncode = 1
            with self.assertRaises(RuntimeError):
                extract.discover(URL)

    def test_player_labels_are_removed_as_whole_lines_only(self):
        text = '1X\n回顾\n隐藏\n自动中\n自动\n1 x\n请隐藏这封信\n回顾过去\n你好'
        self.assertEqual(extract.clean_ocr_text(text), '请隐藏这封信\n回顾过去\n你好')
        rows = []
        extract.merge_segments(rows, 0, 2, '1X\n回顾\n隐藏\n自动中')
        self.assertEqual(rows, [], 'Player controls must not create empty timestamp sections')

    def test_completed_ocr_from_an_older_worker_is_cleaned_on_retrieval(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)):
            folder = Path(directory) / ('c' * 16)
            folder.mkdir()
            state = {'job_id': folder.name, 'status': 'completed', 'method': 'ocr (older worker)',
                     'title': 'Example', 'uploader': 'test', 'part': 1, 'duration': 4,
                     'bilibili_link': URL, 'interval_seconds': 2, 'region': 'subtitles',
                     'document_path': str(folder / 'document.md')}
            extract.atomic_json(folder / 'status.json', state)
            extract.atomic_json(folder / 'segments.json', [
                {'start': 0, 'end': 2, 'text': '1X\n回顾\n隐藏\n自动中'},
                {'start': 2, 'end': 4, 'text': '请隐藏这封信\n回顾\n隐藏\n自动中'}])
            result = extract.main({'action': 'document', 'job_id': folder.name})
            self.assertEqual(result['segment_count'], 1)
            self.assertIn('请隐藏这封信', result['document'])
            self.assertNotIn('1X', result['document'])
            self.assertNotIn('00:00:00 – 00:00:02', result['document'])
            self.assertEqual(json.loads((folder / 'segments.json').read_text()), [
                {'start': 2, 'end': 4, 'text': '请隐藏这封信'}])

    def test_urls_preserve_parts_and_drop_tracking(self):
        self.assertEqual(extract.canonical_url(URL + '?utm_source=chatgpt.com'), URL)
        self.assertEqual(extract.canonical_url(URL + '?p=2&tracking=x'), URL + '?p=2')
        for bad in ['http://www.bilibili.com/video/BV1NzHfz7EA8/',
                    'https://www.bilibili.com.evil.test/video/BV1NzHfz7EA8/',
                    URL + ';touch /tmp/oops', URL + '?p=-1', None]:
            with self.assertRaises(ValueError):
                extract.canonical_url(bad)

    def test_consecutive_duplicates_merge_but_later_repetitions_remain(self):
        rows = []
        extract.merge_segments(rows, 0, 2, '你好，世界')
        extract.merge_segments(rows, 2, 4, '你好，世界')
        extract.merge_segments(rows, 4, 6, 'Goodbye')
        extract.merge_segments(rows, 6, 8, '你好，世界')
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]['end'], 4)
        extract.merge_segments(rows, 12, 14, '你好，世界')
        self.assertEqual(len(rows), 4, 'A blank interval must prevent merging')

    def test_typewriter_completion_preserves_later_characters(self):
        prefix = '这是一段逐字显示的台词' * 10
        rows = []
        extract.merge_segments(rows, 0, 2, prefix)
        extract.merge_segments(rows, 2, 4, prefix + '结束了')
        self.assertEqual(rows, [
            {'start': 0, 'end': 2, 'text': prefix},
            {'start': 2, 'end': 4, 'text': prefix + '结束了'}])

    def test_shared_watermark_does_not_merge_different_dialogue(self):
        watermark = '这是始终出现在画面上的水印文字' * 10
        texts = [watermark + '\n是', watermark + '\n否']
        rows = []
        for index, text in enumerate(texts):
            extract.merge_segments(rows, index * 2, index * 2 + 2, text)
        self.assertEqual([row['text'] for row in rows], texts)

    def test_punctuation_and_case_changes_remain_distinct(self):
        texts = ['你好，世界', '你好 世界', 'Hello', 'hello', '……', '？！']
        rows = []
        for index, text in enumerate(texts):
            extract.merge_segments(rows, index * 2, index * 2 + 2, text)
        self.assertEqual([row['text'] for row in rows], texts)

    def test_danmaku_is_not_a_caption_track(self):
        result = {'_subtitles': {'danmaku': [{'ext': 'srt',
                  'data': '1\n00:00:01,000 --> 00:00:03,000\nComment\n'}]}}
        self.assertEqual(extract.captions(result, URL), ([], ''))

    def test_extractor_srt_captions_preserve_chinese_and_timestamps(self):
        info = {'_subtitles': {'danmaku': [{'ext': 'srt', 'data': 'comments'}],
                'zh-CN': [{'ext': 'srt', 'data': '1\n00:01:02,500 --> 00:01:04,750\n<b>你好</b> &amp; hello\n'}]}}
        rows, language = extract.captions(info, URL)
        self.assertEqual(language, 'zh-CN')
        self.assertEqual(rows, [{'start': 62.5, 'end': 64.75, 'text': '你好 & hello'}])

    def test_document_is_paginated_without_silent_truncation(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)):
            folder = Path(directory) / ('a' * 16)
            folder.mkdir()
            doc = folder / 'document.md'
            content = '中文<example>' * 3000
            doc.write_text(content)
            extract.atomic_json(folder / 'status.json', {
                'job_id': folder.name, 'status': 'completed', 'document_path': str(doc)})
            parts, offset = [], 0
            while True:
                result = extract.main({'action': 'document', 'job_id': folder.name, 'offset': offset})
                parts.append(result['document'])
                offset = result['next_offset']
                if offset is None:
                    break
            self.assertEqual(''.join(parts), content)
            with self.assertRaises(ValueError):
                extract.main({'action': 'document', 'job_id': '../escape'})

    def test_dead_background_worker_is_reported_as_failed(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)):
            folder = Path(directory) / ('b' * 16)
            folder.mkdir()
            extract.atomic_json(folder / 'status.json', {
                'job_id': folder.name, 'status': 'running', 'pid': 0})
            state = extract.status(folder.name)
            self.assertEqual(state['status'], 'failed')
            self.assertIn('Worker stopped', state['error'])

    def test_repeated_start_returns_cached_job_without_another_worker(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'Popen') as spawn:
            spawn.return_value.pid = 123
            first = extract.main({'bilibili_link': URL})
            folder = Path(directory) / first['job_id']
            extract.update(folder, status='completed')
            second = extract.main({'bilibili_link': URL + '?utm_source=chatgpt.com'})
            self.assertEqual(first['job_id'], second['job_id'])
            self.assertEqual(spawn.call_count, 1)

    def test_parts_get_separate_jobs_and_documents_without_parallel_workers(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'Popen') as spawn:
            spawn.return_value.pid = 123
            first = extract.main({'bilibili_link': URL})
            with self.assertRaisesRegex(RuntimeError, 'already running'):
                extract.main({'bilibili_link': URL + '?p=2'})
            extract.update(Path(directory) / first['job_id'], status='completed')
            second = extract.main({'bilibili_link': URL + '?p=2'})
            self.assertNotEqual(first['job_id'], second['job_id'])
            self.assertEqual(second['bilibili_link'], URL + '?p=2')
            self.assertEqual(spawn.call_count, 2)

    def test_worker_failure_is_not_a_completed_document(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {'bilibili_link': URL, 'mode': 'auto'})
            with patch.object(extract, 'metadata', side_effect=RuntimeError('Unavailable')):
                extract.worker(folder)
            state = json.loads((folder / 'status.json').read_text())
            self.assertEqual(state['status'], 'failed')
            self.assertEqual(state['error'], 'Unavailable')
            self.assertFalse((folder / 'document.md').exists())

    def test_caption_success_creates_escaped_printable_document_without_downloading(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {'bilibili_link': URL, 'mode': 'auto'})
            info = {'title': '<Title>', 'uploader': 'test', 'part': 1, 'duration': 10}
            with patch.object(extract, 'metadata', return_value=info), \
                    patch.object(extract, 'captions', return_value=([
                        {'start': 0, 'end': 5, 'text': '<script>你好</script>'}], 'zh')), \
                    patch.object(extract, 'download') as download:
                extract.worker(folder)
            self.assertEqual(json.loads((folder / 'status.json').read_text())['status'], 'completed')
            download.assert_not_called()
            self.assertIn('&lt;script&gt;', (folder / 'document.html').read_text())
            self.assertIn('你好', (folder / 'document.md').read_text())

JPEG = base64.b64decode(
    '/9j/4AAQSkZJRgABAgAAAQABAAD//gAPTGF2YzYzLjEuMTAxAP/bAEMACAQEBAQEBQUFBQUFBgYGBgYG'
    'BgYGBgYGBgYGBgcHBwgICAcHBwYGBwcICAgICQkJCAgICAkJCgoKDAwLCw4ODhERFP/EAEsAAQEAAAAA'
    'AAAAAAAAAAAAAAAHAQEAAAAAAAAAAAAAAAAAAAAAEAEAAAAAAAAAAAAAAAAAAAAAEQEAAAAAAAAAAAAAAAAA'
    'AAAA/8AAEQgAAgACAwEiAAIRAAMRAP/aAAwDAQACEQMRAD8Av4AP/9k=')


def completion(text, finish='stop', **message):
    return {'choices': [{'finish_reason': finish, 'message': {'content': text, **message}}]}


@contextmanager
def ocr_server(responses):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            raw = self.rfile.read(int(self.headers['Content-Length']))
            requests.append({'path': self.path, 'headers': self.headers, 'body': json.loads(raw)})
            status, body = responses(requests[-1]['body']) if callable(responses) else responses.pop(0)
            if not isinstance(body, bytes):
                body = json.dumps(body, ensure_ascii=False).encode('utf-8')
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    with patch('socket.getfqdn', return_value='localhost'):
        server = HTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.01}, daemon=True)
    thread.start()
    settings = {'base_url': f'http://127.0.0.1:{server.server_port}/v1',
                'model': 'fixture-paddle', 'protocol_version': extract.OCR_PROTOCOL_VERSION}
    try:
        with patch.dict(os.environ, {'no_proxy': '127.0.0.1', 'NO_PROXY': '127.0.0.1'}):
            yield settings, requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class RemoteOCRTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        here = patch.object(extract, 'HERE', Path(directory.name))
        here.start()
        self.addCleanup(here.stop)
        environment = patch.dict(os.environ, {'CB_OCR_API_KEY': ''})
        environment.start()
        self.addCleanup(environment.stop)

    def test_real_http_payload_carries_jpeg_and_returns_chinese_or_blank(self):
        with tempfile.TemporaryDirectory() as directory, \
                ocr_server([(200, completion('你好\n世界')), (200, completion(''))]) as (settings, requests):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            with patch.object(extract.urllib.request, 'urlopen', wraps=extract.urllib.request.urlopen) as send:
                self.assertEqual(extract.recognize(frame, settings), '你好\n世界')
                self.assertEqual(extract.recognize(frame, settings), '')
            self.assertEqual([call.kwargs['timeout'] for call in send.call_args_list], [120, 120])
            self.assertEqual(len(requests), 2)
            for request in requests:
                self.assertEqual(request['path'], '/v1/chat/completions')
                self.assertEqual(request['headers']['Content-Type'], 'application/json')
                self.assertNotIn('Authorization', request['headers'])
                body = request['body']
                self.assertEqual(body['model'], 'fixture-paddle')
                self.assertEqual(body['temperature'], 0)
                self.assertIs(body['stream'], False)
                self.assertEqual(body['max_tokens'], 4096)
                self.assertEqual(body['repetition_penalty'], 1.3)
                self.assertEqual(body['messages'][0]['role'], 'user')
                image, instruction = body['messages'][0]['content']
                self.assertEqual(instruction, {'type': 'text', 'text': 'OCR:'})
                self.assertEqual(image['type'], 'image_url')
                prefix, encoded = image['image_url']['url'].split(',', 1)
                self.assertEqual(prefix, 'data:image/jpeg;base64')
                self.assertEqual(base64.b64decode(encoded, validate=True), JPEG)

    def test_omlx_zero_token_stop_may_omit_content_but_other_missing_content_fails(self):
        blank = {'choices': [{'message': {'role': 'assistant'}, 'finish_reason': 'stop'}],
                 'usage': {'completion_tokens': 0}}
        nonempty = {'choices': [{'message': {'role': 'assistant'}, 'finish_reason': 'stop'}],
                    'usage': {'completion_tokens': 2}}
        with tempfile.TemporaryDirectory() as directory, \
                ocr_server([(200, blank), (200, nonempty)]) as (settings, _):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            self.assertEqual(extract.recognize(frame, settings), '')
            with self.assertRaisesRegex(RuntimeError, 'missing or non-string'):
                extract.recognize(frame, settings)

    def test_bearer_auth_uses_environment_before_file_and_stays_out_of_settings(self):
        key = 'synthetic-ocr-secret'
        (extract.HERE / 'ocr-api-key').write_text('synthetic-file-secret\n')
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {'CB_OCR_API_KEY': key}), \
                ocr_server([(200, completion('你好'))]) as (settings, requests):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            self.assertEqual(extract.recognize(frame, settings), '你好')
            self.assertEqual(requests[0]['headers']['Authorization'], 'Bearer ' + key)
            self.assertNotIn(key, json.dumps(extract.ocr_settings()))
            self.assertNotIn('api_key', extract.ocr_settings())

    def test_bearer_auth_reads_file_only_when_environment_is_unset(self):
        key = 'synthetic-file-secret'
        (extract.HERE / 'ocr-api-key').write_text(key + '\n')
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {}, clear=True), \
                ocr_server([(200, completion('你好'))]) as (settings, requests):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            self.assertEqual(extract.recognize(frame, settings), '你好')
            self.assertEqual(requests[0]['headers']['Authorization'], 'Bearer ' + key)
            self.assertNotIn(key, json.dumps(extract.ocr_settings()))

    def test_empty_environment_disables_file_auth(self):
        (extract.HERE / 'ocr-api-key').write_text('synthetic-file-secret')
        with tempfile.TemporaryDirectory() as directory, \
                ocr_server([(200, completion(''))]) as (settings, requests):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            self.assertEqual(extract.recognize(frame, settings), '')
            self.assertNotIn('Authorization', requests[0]['headers'])

    def test_errors_redact_bearer_key(self):
        key = 'synthetic-ocr-secret'
        responses = [(401, {'error': {'message': key}}),
                     (200, {'error': {'message': key}}),
                     (200, completion(key, 'length'))]
        with tempfile.TemporaryDirectory() as directory, \
                patch.dict(os.environ, {'CB_OCR_API_KEY': key}), \
                ocr_server(responses) as (settings, _):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            for _ in responses:
                with self.assertRaises(RuntimeError) as caught:
                    extract.recognize(frame, settings)
                self.assertNotIn(key, str(caught.exception))
                self.assertIn('[key omitted]', str(caught.exception))

    def test_length_failure_names_client_limit_and_bounds_sanitized_preview(self):
        partial = 'partial\n\x1b data:image/jpeg;base64,privatepayload ' + 'x' * 2000
        with tempfile.TemporaryDirectory() as directory, \
                ocr_server([(200, completion(partial, 'length'))]) as (settings, _):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            with self.assertRaises(RuntimeError) as caught:
                extract.recognize(frame, settings)
            detail = str(caught.exception)
            self.assertIn('client max_tokens=4096', detail)
            self.assertIn('partial\\n\\x1b', detail)
            self.assertIn('[image omitted]', detail)
            self.assertNotIn('privatepayload', detail)
            self.assertNotIn('\x1b', detail)
            self.assertLessEqual(len(detail.split('Output preview: ', 1)[1]), 500)

    def test_remote_errors_never_become_blank_frames(self):
        cases = [
            (400, {'error': {'message': 'model does not support images'}}, 'HTTP 400'),
            (503, b'server unavailable' * 1000, 'HTTP 503'),
            (200, {'error': {'message': 'image input unavailable'}}, 'image input unavailable'),
            (200, b'not JSON', 'invalid JSON'),
            (200, b'\xff', 'invalid JSON'),
            (200, [], 'response object'),
            (200, {}, 'choices'),
            (200, {'choices': []}, 'choices'),
            (200, {'choices': [None]}, 'choices'),
            (200, {'choices': [{'message': None}]}, 'message'),
            (200, completion(None), 'non-string'),
            (200, completion([{'type': 'text', 'text': 'not a string'}]), 'non-string'),
            (200, completion('', refusal='Cannot process image'), 'refused'),
            (200, completion('partial', 'length'), 'truncated'),
            (200, completion('', 'content_filter'), 'refused'),
            (200, completion('partial', None), 'finish_reason'),
            (200, completion('partial', 'tool_calls'), 'finish_reason'),
            (200, b'x' * (extract.OCR_RESPONSE_LIMIT + 1), 'larger than'),
        ]
        with tempfile.TemporaryDirectory() as directory, \
                ocr_server([(status, body) for status, body, _ in cases]) as (settings, requests):
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            for _, _, expected in cases:
                with self.subTest(expected=expected), self.assertRaisesRegex(RuntimeError, expected) as caught:
                    extract.recognize(frame, settings)
                self.assertIn(settings['base_url'], str(caught.exception))
                self.assertIn(settings['model'], str(caught.exception))
                self.assertLess(len(str(caught.exception)), 1000)
            self.assertEqual(len(requests), len(cases))

    def test_connection_failure_names_endpoint_model_and_policy(self):
        with tempfile.TemporaryDirectory() as directory:
            frame = Path(directory) / 'frame.jpg'
            frame.write_bytes(JPEG)
            with ocr_server([]) as (settings, _):
                pass
            with patch.dict(os.environ, {'no_proxy': '127.0.0.1', 'NO_PROXY': '127.0.0.1'}), \
                    self.assertRaisesRegex(RuntimeError, 'proxy.*egress') as caught:
                extract.recognize(frame, settings)
            self.assertIn(settings['base_url'], str(caught.exception))
            self.assertIn(settings['model'], str(caught.exception))

    def test_settings_defaults_overrides_and_validation(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(extract.ocr_settings(), {
                'base_url': 'http://192.168.10.109:12340/v1', 'model': 'PaddleOCR-VL-1.6-MLX-8bit',
                'protocol_version': extract.OCR_PROTOCOL_VERSION})
        with patch.dict(os.environ, {'CB_OCR_BASE_URL': 'https://ocr.test/v1/', 'CB_OCR_MODEL': 'other'}):
            self.assertEqual(extract.ocr_settings()['base_url'], 'https://ocr.test/v1')
            self.assertEqual(extract.ocr_settings()['model'], 'other')
        for url in ['', 'file:///tmp/ocr', 'http://user:pass@ocr.test/v1', 'http://@ocr.test/v1',
                    'http://ocr.test/v1?q=1', 'http://ocr.test/v1#frag', 'http://ocr.test:bad/v1',
                    'http://ocr.test:99999/v1', 'http://ocr. test/v1']:
            with self.subTest(url=url), patch.dict(os.environ, {'CB_OCR_BASE_URL': url}), \
                    self.assertRaises(ValueError):
                extract.ocr_settings()
        with patch.dict(os.environ, {'CB_OCR_MODEL': '  '}), self.assertRaises(ValueError):
            extract.ocr_settings()

    def test_ocr_cache_tracks_endpoint_model_protocol_while_captions_keep_legacy_hash(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                patch.object(extract.subprocess, 'Popen') as spawn, patch.dict(os.environ, {}, clear=True):
            spawn.return_value.pid = 123

            def start(mode):
                state = extract.main({'bilibili_link': URL, 'mode': mode})
                extract.update(Path(directory) / state['job_id'], status='completed')
                return state

            for mode in ('ocr', 'auto'):
                original = start(mode)
                self.assertEqual(original['job_id'], start(mode)['job_id'])
                legacy_key = json.dumps([URL, mode, 'full', 2.0])
                self.assertNotEqual(original['job_id'], hashlib.sha256(legacy_key.encode()).hexdigest()[:16])
                with patch.dict(os.environ, {'CB_OCR_BASE_URL': 'http://other.test:12340/v1'}):
                    alternate_url = start(mode)
                with patch.dict(os.environ, {'CB_OCR_MODEL': 'other'}):
                    alternate_model = start(mode)
                with patch.object(extract, 'OCR_PROTOCOL_VERSION', extract.OCR_PROTOCOL_VERSION + 1):
                    alternate_protocol = start(mode)
                self.assertEqual(len({state['job_id'] for state in
                                      [original, alternate_url, alternate_model, alternate_protocol]}), 4)
                self.assertEqual(original['ocr_settings'], extract.ocr_settings())
            caption = start('captions')
            legacy_key = json.dumps([URL, 'captions', 'full', 2.0])
            self.assertEqual(caption['job_id'], hashlib.sha256(legacy_key.encode()).hexdigest()[:16])
            with patch.dict(os.environ, {'CB_OCR_BASE_URL': 'invalid', 'CB_OCR_MODEL': ''}):
                self.assertEqual(start('captions')['job_id'], caption['job_id'])
                self.assertEqual(extract.main({'action': 'status', 'job_id': original['job_id']})['status'], 'completed')
            self.assertTrue(all(call.args[0][0] == sys.executable for call in spawn.call_args_list))

    def test_worker_keeps_job_settings_and_frame_order_through_document(self):
        texts = ['你好', '你好', '', '后来']

        def respond(body):
            encoded = body['messages'][0]['content'][0]['image_url']['url'].split(',', 1)[1]
            index = base64.b64decode(encoded)[-1]
            return 200, completion('' if index == 255 else texts[index])

        with tempfile.TemporaryDirectory() as directory, patch.object(extract, 'ROOT', Path(directory)), \
                ocr_server(respond) as (settings, requests):
            with patch.dict(os.environ, {'CB_OCR_BASE_URL': settings['base_url'], 'CB_OCR_MODEL': settings['model']}), \
                    patch.object(extract.subprocess, 'Popen') as spawn:
                spawn.return_value.pid = 123
                state = extract.main({'bilibili_link': URL, 'mode': 'ocr'})
            folder = Path(directory) / state['job_id']
            self.assertEqual(json.loads((folder / 'status.json').read_text())['ocr_settings'], settings)

            def sample(command, **kwargs):
                return sample_frames(command, 4)

            info = {'title': '远程测试', 'uploader': 'fixture', 'part': 1, 'duration': 8}
            with patch.dict(os.environ, {'CB_OCR_BASE_URL': 'http://invalid.test/v1', 'CB_OCR_MODEL': 'changed'}), \
                    patch.object(extract, 'metadata', return_value=info), \
                    patch.object(extract, 'download', return_value=folder / 'video.mp4'), \
                    patch.object(extract.subprocess, 'run', side_effect=sample):
                extract.worker(folder)
            final = json.loads((folder / 'status.json').read_text())
            self.assertEqual(final['status'], 'completed', final)
            self.assertEqual(final['ocr_settings'], settings)
            self.assertEqual(final['method'], 'ocr (remote fixture-paddle)')
            self.assertEqual(final['frames_processed'], 4)
            self.assertEqual(json.loads((folder / 'segments.json').read_text()), [
                {'start': 0, 'end': 4, 'text': '你好'}, {'start': 6, 'end': 8, 'text': '后来'}])
            self.assertEqual([request['body']['model'] for request in requests], ['fixture-paddle'] * 8)
            self.assertIn('remote fixture-paddle', (folder / 'document.md').read_text())

    def test_four_inflight_frames_finish_out_of_order_but_merge_chronologically(self):
        barrier = threading.Barrier(4)
        finished = [threading.Event() for _ in range(4)]
        completed = []
        active = 0
        peak = 0
        lock = threading.Lock()

        def recognize(frame, settings):
            nonlocal active, peak
            index = int(frame.stem.split('-')[-1]) - 1
            if frame.name.startswith('bottom-'):
                return ''
            with lock:
                active += 1
                peak = max(peak, active)
            if index < 4:
                barrier.wait(timeout=5)
                if index < 3:
                    self.assertTrue(finished[index + 1].wait(timeout=5))
            with lock:
                completed.append(index)
                active -= 1
            if index < 4:
                finished[index].set()
            return ['你好', '你好', '', '后来', '结束', '结束'][index]

        def sample(command, **kwargs):
            return sample_frames(command, 6)

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract, 'recognize', side_effect=recognize), \
                patch.object(extract.subprocess, 'run', side_effect=sample):
            extract.atomic_json(Path(directory) / 'status.json', {})
            segments = extract.ocr(Path('video.mp4'), Path(directory), 6, 1, 'full', {})
        self.assertEqual(peak, 4)
        self.assertEqual(completed[:4], [3, 2, 1, 0])
        self.assertEqual(segments, [
            {'start': 0, 'end': 2, 'text': '你好'},
            {'start': 3, 'end': 4, 'text': '后来'},
            {'start': 4, 'end': 6, 'text': '结束'}])

    def test_parallel_failure_waits_for_frames_before_cleanup_and_writes_no_document(self):
        barrier = threading.Barrier(4)
        failed = threading.Event()
        calls = []

        def recognize(frame, settings):
            if frame.name.startswith('bottom-'):
                return ''
            calls.append(frame)
            barrier.wait(timeout=5)
            if frame.stem.endswith('000001'):
                failed.set()
                raise RuntimeError('fixture recognition failed')
            self.assertTrue(failed.wait(timeout=5))
            self.assertTrue(frame.exists())
            return '文字'

        def sample(command, **kwargs):
            return sample_frames(command, 8)

        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {
                'bilibili_link': URL, 'mode': 'ocr', 'region': 'full', 'interval_seconds': 1,
                'ocr_settings': extract.ocr_settings()})
            with patch.object(extract, 'metadata', return_value={'duration': 8}), \
                    patch.object(extract, 'download', return_value=folder / 'video.mp4'), \
                    patch.object(extract, 'recognize', side_effect=recognize), \
                    patch.object(extract.subprocess, 'run', side_effect=sample):
                extract.worker(folder)
            final = json.loads((folder / 'status.json').read_text())
            self.assertEqual(final['status'], 'failed')
            self.assertIn('fixture recognition failed', final['error'])
            self.assertEqual(len(calls), 4)
            self.assertFalse((folder / 'document.md').exists())
            self.assertFalse((folder / 'segments.json').exists())
            self.assertFalse(list(folder.glob('frames-*')))

    def test_native_tile_bounds_cover_odd_frames_and_keep_the_proven_lower_half(self):
        for width, height in [(582, 1280), (583, 1281), (1, 1)]:
            top, bottom = extract.tile_bounds(width, height)
            self.assertEqual(top[:3], (0, 0, width))
            self.assertEqual(bottom, (0, height // 2, width, height))
            self.assertGreaterEqual(top[3], bottom[1])
            self.assertLessEqual(top[3], height)
        self.assertEqual(extract.tile_bounds(582, 1280), [(0, 0, 582, 768), (0, 640, 582, 1280)])

    def test_second_tile_failure_retains_raw_evidence_and_completed_frame_prefix(self):
        def sample(command, **kwargs):
            if command[0] == 'ffmpeg':
                self.assertLess(command.index('-t'), command.index('-i'))
                self.assertIn('eof_action=pass', command[command.index('-filter_complex') + 1])
                self.assertIn('start_time=0', command[command.index('-filter_complex') + 1])
                self.assertIn('trim=duration=3', command[command.index('-filter_complex') + 1])
            return sample_frames(command, 3)

        def recognize(frame, settings):
            if frame.name == 'bottom-000002.jpg':
                raise RuntimeError('second tile failed')
            return 'upper evidence' if frame.name.startswith('top-') else 'lower evidence'

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=sample), \
                patch.object(extract, 'recognize', side_effect=recognize):
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {})
            with self.assertRaisesRegex(RuntimeError, 'second tile failed'):
                extract.ocr(Path('video.mp4'), folder, 3, 1, 'full', {})
            partial = json.loads((folder / 'segments.partial.json').read_text())
            self.assertEqual(partial, [{'start': 0, 'end': 1, 'text': 'upper evidence\nlower evidence'}])
            evidence = [json.loads(line) for line in (folder / 'frames.raw.jsonl').read_text().splitlines()]
            failed = next(row for row in evidence if row['start'] == 1)
            self.assertFalse(failed['complete'])
            self.assertEqual(failed['tiles'][0]['raw_text'], 'upper evidence')
            self.assertIn('second tile failed', failed['tiles'][1]['error'])
            self.assertFalse(list(folder.glob('frames-*')))

    def test_missing_or_truncated_tile_pairs_fail_before_inference(self):
        for defect in ['one_tile', 'both_tiles']:
            with self.subTest(defect=defect), tempfile.TemporaryDirectory() as directory:
                def sample(command, **kwargs):
                    result = sample_frames(command, 3)
                    if command[0] == 'ffmpeg':
                        for arg in command:
                            if '%06d' in arg and (defect == 'both_tiles' or 'bottom-' in arg):
                                Path(arg.replace('%06d', '000002')).unlink()
                    return result
                folder = Path(directory)
                extract.atomic_json(folder / 'status.json', {})
                with patch.object(extract.subprocess, 'run', side_effect=sample), \
                        patch.object(extract, 'recognize') as recognize, \
                        self.assertRaisesRegex(RuntimeError, 'missing|count or order'):
                    extract.ocr(Path('video.mp4'), folder, 3, 1, 'full', {})
                recognize.assert_not_called()

    def test_retry_resumes_complete_evidence_prefix_and_keeps_failed_attempt(self):
        state = {'offset': 0, 'fail': True}
        calls = []

        def sample(command, **kwargs):
            if command[0] == 'ffprobe':
                return sample_frames(command, 0)
            state['offset'] = int(float(command[command.index('-ss') + 1]))
            count = int(float(command[command.index('-t') + 1]))
            return sample_frames(command, count)

        def recognize(frame, settings):
            index = state['offset'] + int(frame.stem.split('-')[-1]) - 1
            side = frame.name.split('-')[0]
            calls.append((index, side))
            if state['fail'] and index == 1 and side == 'bottom':
                raise RuntimeError('transient failure')
            return f'{index} {side}'

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=sample), \
                patch.object(extract, 'recognize', side_effect=recognize):
            folder = Path(directory)
            extract.atomic_json(folder / 'status.json', {})
            with self.assertRaisesRegex(RuntimeError, 'transient failure'):
                extract.ocr(Path('video.mp4'), folder, 3, 1, 'full', {})
            state['fail'] = False
            calls.clear()
            segments = extract.ocr(Path('video.mp4'), folder, 3, 1, 'full', {})
            self.assertNotIn((0, 'top'), calls)
            self.assertEqual([s['text'] for s in segments], ['0 top\n0 bottom', '1 top\n1 bottom', '2 top\n2 bottom'])
            rows = [json.loads(line) for line in (folder / 'frames.raw.jsonl').read_text().splitlines()]
            self.assertEqual([row['start'] for row in rows], [0, 1, 2])
            self.assertTrue(all(row['complete'] for row in rows))
            backups = list(folder.glob('frames.raw.previous-*.jsonl'))
            self.assertEqual(len(backups), 1)
            self.assertIn('transient failure', backups[0].read_text())

    def test_resume_rejects_evidence_from_a_different_protocol(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=lambda command, **kwargs: sample_frames(command, 0)), \
                patch.object(extract, 'recognize') as recognize:
            folder = Path(directory)
            (folder / 'frames.raw.jsonl').write_text(json.dumps({'complete': True, 'protocol_version': 1}) + '\n')
            with self.assertRaisesRegex(RuntimeError, 'cannot resume safely'):
                extract.ocr(Path('video.mp4'), folder, 3, 1, 'full', {})
            recognize.assert_not_called()

    def test_token_limit_splits_only_the_failed_tile_once_with_full_pixel_coverage(self):
        def crop(command, **kwargs):
            for arg in command:
                if arg.endswith('.jpg') and '-recovery-' in arg:
                    Path(arg).write_bytes(JPEG)
            return subprocess.CompletedProcess(command, 0)

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=crop), \
                patch.object(extract, 'recognize', side_effect=[
                    extract.OCRTokenLimitError('client max_tokens=4096'), '对白。', '回顾 隐藏 自动中']) as recognize:
            image = Path(directory) / 'bottom.jpg'
            image.write_bytes(JPEG)
            reading = extract.recognize_frame((image,), [(0, 640, 582, 1280)], {})
            self.assertTrue(reading['complete'])
            self.assertEqual(reading['text'], '对白。\n回顾 隐藏 自动中')
            self.assertEqual(recognize.call_count, 3)
            recovered = reading['tiles'][0]['recovery']['tiles']
            self.assertEqual([child['bounds'] for child in recovered],
                             [[0, 640, 582, 1024], [0, 960, 582, 1280]])
            self.assertIn('4096', reading['tiles'][0]['recovery']['reason'])

    def test_failed_recovery_keeps_first_child_and_does_not_retry_forever(self):
        def crop(command, **kwargs):
            for arg in command:
                if arg.endswith('.jpg') and '-recovery-' in arg:
                    Path(arg).write_bytes(JPEG)
            return subprocess.CompletedProcess(command, 0)

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(extract.subprocess, 'run', side_effect=crop), \
                patch.object(extract, 'recognize', side_effect=[
                    extract.OCRTokenLimitError('initial limit'), '已识别正文。',
                    extract.OCRTokenLimitError('child limit')]) as recognize:
            image = Path(directory) / 'bottom.jpg'
            image.write_bytes(JPEG)
            reading = extract.recognize_frame((image,), [(0, 640, 582, 1280)], {})
            self.assertFalse(reading['complete'])
            self.assertEqual(recognize.call_count, 3)
            self.assertEqual(reading['tiles'][0]['recovery']['tiles'][0]['raw_text'], '已识别正文。')
            self.assertIn('child limit', reading['error'])

    def test_remote_failure_and_legacy_missing_settings_fail_without_document(self):
        with ocr_server([(400, {'error': 'image unsupported'})]) as (settings, _):
            for saved_settings in (None, settings):
                with self.subTest(settings=saved_settings), tempfile.TemporaryDirectory() as directory:
                    folder = Path(directory)
                    state = {'bilibili_link': URL, 'mode': 'ocr', 'region': 'full', 'interval_seconds': 2}
                    if saved_settings is not None:
                        state['ocr_settings'] = saved_settings
                    extract.atomic_json(folder / 'status.json', state)

                    def sample(command, **kwargs):
                        return sample_frames(command, 1)

                    with patch.object(extract, 'metadata', return_value={'duration': 2}), \
                            patch.object(extract, 'download', return_value=folder / 'video.mp4') as download, \
                            patch.object(extract.subprocess, 'run', side_effect=sample):
                        extract.worker(folder)
                    final = json.loads((folder / 'status.json').read_text())
                    self.assertEqual(final['status'], 'failed')
                    self.assertFalse((folder / 'document.md').exists())
                    if saved_settings is None:
                        self.assertIn('no compatible remote OCR settings', final['error'])
                        download.assert_not_called()
                    else:
                        self.assertIn('HTTP 400', final['error'])


if __name__ == '__main__':
    unittest.main()
