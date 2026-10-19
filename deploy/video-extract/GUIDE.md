# Extract Bilibili video text

Use `bilibili_run` to hand off extraction to the box. This tool uses FFmpeg,
RapidOCR (Chinese and English, on the CPU), and yt-dlp. Tesseract is a fallback
when the optional Python environment is absent. It makes no paid API calls.
It processes accessible videos; it does not establish a video's license.

Start with:

First detect all P parts with `params: {"action":"discover","bilibili_link":"URL"}`.
This returns `part_count` and ordered `parts`, each with `part`, `title` and
`bilibili_link`. Detection covers every part of the same BV, even if the supplied
link contains `p=N`; an explicit user request for just one P overrides this workflow.
Discovery failures must be reported, never treated as a single-part video.
This does not expand related-video recommendations or separate-BV collections.

For each requested link, discover its parts, then start each returned part URL in
order using the call below. Wait for that job, read every document page, save a
separate article for that P, then start the next P. Do not merge unrelated parts
or plot branches. Retain P numbers and available source titles. Duplicate links
reuse the same jobs. One extraction worker runs at a time.

```json
{"code":"","screenshot":false,"params":{"bilibili_link":"https://www.bilibili.com/video/BV1NzHfz7EA8/","mode":"ocr"}}
```

The result contains `job_id`. Check progress with:

```json
{"code":"","screenshot":false,"params":{"action":"status","job_id":"RETURNED_ID"}}
```

On `status: completed`, retrieve the document with:

```json
{"code":"","screenshot":false,"params":{"action":"document","job_id":"RETURNED_ID"}}
```

The result includes text, Markdown and HTML paths under `/workspace/video-extractions/`.
If `next_offset` is not null, call `document` again with that offset. Read all chunks
before claiming to have received the whole document. HTML is a standalone, printable
document. `segments.json` contains the original timestamped recognition results.
Preserve the source and extraction-method note when returning the document.

`auto` prefers accessible caption tracks and falls back to OCR. Use `ocr` when
the user specifically wants visible text, including slides. Use `captions` only
when they want a published caption track. No audio recognition is included.
Bullet comments are not caption tracks, although OCR of the full screen may
recognize bullet comments embedded in the actual video.

OCR defaults to one frame every two seconds, across the full image. It can miss
short-lived text. `interval_seconds: 1` samples more densely. `region: subtitles`
crops the bottom 38%, which is faster for videos whose dialogue is in that area,
but misses text elsewhere. OCR is approximate; do not call it a complete or
verified transcript. Consecutive near-duplicate frames are merged; later repeated
text remains. One job runs at a time. Failed jobs need `retry: true` to restart.
Standalone player labels `1X`, `回顾`, `隐藏`, `自动中`, and `自动` are removed from
OCR results. The same words inside a dialogue sentence are preserved.

Keep waiting between status checks; don't continuously poll. Jobs persist after
the tool call and while the box hibernates. A stopped worker is reported as failed;
partial results remain in `segments.partial.json`.

For a login-required video, the user may sign in and supply a Netscape-format
cookie file locally at `~/.local/share/cocoonbox/video-extract/cookies.txt`.
Never ask them to paste cookies into chat. The default uses no browser cookies.

## ChatGPT Work compatibility

If ChatGPT's tool catalog does not expose `bilibili_run`, use the existing
`script_run` tool with the installed saved script. It calls the same extractor,
with the same params, jobs and documents; it does not require a new extraction.
Read `/opt/desk/AGENTS.md` before the first `script_run` call.
For discovery, set `timeout_s: 110` and
`params: {"action":"discover","bilibili_link":"URL"}` on the same saved script.

```json
{"script":"flows/bilibili-extract.mts","screenshot":false,"params":{"bilibili_link":"https://www.bilibili.com/video/BV1NzHfz7EA8/","mode":"ocr","region":"subtitles"}}
```

For status, use that same script with
`params: {"action":"status","job_id":"RETURNED_ID"}`.
For text, use `params: {"action":"document","job_id":"RETURNED_ID","offset":0}`.
The extractor response is inside `result`. Continue document calls with
`offset: result.next_offset` until `next_offset` is null.
If the script file is missing, report the installation issue.

The same interface is also available through shell:

```bash
python3 /home/user/.local/share/cocoonbox/video-extract/extract.py '{"bilibili_link":"https://www.bilibili.com/video/BV1NzHfz7EA8/","mode":"ocr"}'
```
