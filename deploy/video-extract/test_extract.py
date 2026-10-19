import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('extract', Path(__file__).with_name('extract.py'))
extract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(extract)
URL = 'https://www.bilibili.com/video/BV1NzHfz7EA8/'


class ExtractionTests(unittest.TestCase):
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
        extract.merge_segments(rows, 2, 4, '你好 世界')
        extract.merge_segments(rows, 4, 6, 'Goodbye')
        extract.merge_segments(rows, 6, 8, '你好，世界')
        self.assertEqual(len(rows), 3)
        self.assertEqual(rows[0]['end'], 4)
        extract.merge_segments(rows, 12, 14, '你好，世界')
        self.assertEqual(len(rows), 4, 'A blank interval must prevent merging')

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


if __name__ == '__main__':
    unittest.main()
