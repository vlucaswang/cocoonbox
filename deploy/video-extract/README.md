# Bilibili text extraction

An optional Cocoonbox script-tool plugin. `bilibili_run` starts a CPU extraction
job, reports progress, and returns a timestamped Markdown document. HTML and plain
text copies are saved alongside it. The existing agent's script-plugin interface
handles MCP discovery and execution; no proxy or model service is added.

## Install

Copy this directory into the **guest**, then run:

```bash
sudo bash install.sh
sudo systemctl restart cocoonbox-agent cocoonbox-toolhost
```

Restarting these services interrupts their active tool calls. Do the restart from
a guest terminal or schedule it separately after the installing MCP call returns.
For a scheduled restart, `systemd-run --on-active=3s --timer-property=AccuracySec=1s`
can launch `/bin/systemctl restart cocoonbox-agent cocoonbox-toolhost`.

The installer adds a descriptor under `/etc/cocoonbox/tools.d/`, a short routing
paragraph under `/etc/cocoonbox/routing.d/`, and files in
`/home/user/.local/share/cocoonbox/video-extract/`. FFmpeg and Chinese/English
Tesseract packages are installed if absent. A user-owned virtual environment holds
RapidOCR ONNX Runtime 1.4.4 for scene text detection and Chinese/English recognition;
Tesseract is the fallback when running without that environment. yt-dlp's official release zip is
downloaded over HTTPS and checked against the release's SHA-256 manifest.
Its version is printed by the installer. Nothing configures a paid API.

The agent must support script plugins and the guest must have the desk runtime.
Refresh the ChatGPT connection's tools after installation; a new conversation may
be needed if ChatGPT retains the old tool list. Follow [GUIDE.md](GUIDE.md) for calls.

The installer also saves `/workspace/flows/bilibili-extract.mts`. Existing
ChatGPT Work connections can invoke it through `script_run` when their tool
catalog still lacks `bilibili_run`. The same params and pagination apply.
Validate this path through the actual connected ChatGPT tools, not just the
server's MCP endpoint: server discovery alone does not prove Work exposes a
new tool.

For a direct custom MCP connection, open its connection details in ChatGPT
Plugins, select Refresh, and start a new conversation. For a published plugin,
server changes pass separate checks; its developer MCP Issues page offers
Rescan. See the official [connection refresh instructions](https://developers.openai.com/plugins/deploy/connect-chatgpt)
and [published plugin checks](https://developers.openai.com/plugins/deploy/submission).

`box-rpc.py` is an operator helper specific to this deployment's dedicated isabel
account. Run it as root on the host; it reads that account's token locally, wakes
only its box, and calls the normal authenticated MCP endpoint. It never prints
the token. Supply tool arguments in a JSON file:

```bash
sudo python3 box-rpc.py list
sudo python3 box-rpc.py bilibili_run /path/to/arguments.json
```

## Lifecycle and limits

- `action: discover` detects all P parts of a BV and returns their ordered URLs.
  It uses yt-dlp's flat playlist metadata without downloading the media, with a
  ten-minute cache. Start, poll and paginate each part sequentially; each retains
  its own cached job and document. Separate BV links can be processed the same
  way; related recommendations and collections are not expanded.

- Source links are restricted to HTTPS Bilibili BV video pages, with optional
  `p=N` parts. Tracking parameters are discarded; identical jobs are cached.
- `auto` uses accessible captions when available, then OCR. `ocr` always extracts
  sampled visible text. `captions` fails if no caption track is accessible. No
  speech recognition is included.
- Defaults: whole image, one frame every two seconds, two OCR workers, one job at
  a time, up to four hours per part, 2 GiB media, 30-minute download deadline.
- OCR retains timestamps and merges consecutive near-duplicates. It is approximate
  and can miss text between samples. The printable document states this limitation.
- Downloads and frame batches are temporary. Documents, status, logs, and partial
  OCR live in `/workspace/video-extractions/<job_id>/` and survive normal state saves.
- Media uses HTTP byte-range chunks. Source metadata is cached in the job directory
  for 30 minutes to avoid repeating extraction requests during retries.
- Worker processes detach from the short MCP invocation. Hibernate preserves them.
  Restarting the toolhost or the guest can interrupt a job; the status action
  detects missing workers and reports failure. Retry restarts the whole job.
- Configuration and apt packages under `/etc` and `/usr` survive hibernation, but
  **an image rebase requires reinstalling this add-on**. User documents remain in
  the normal state roots. For new images, run this installer as a runtime provision
  step instead of modifying the agent's core tool list.
- Login-required sources can use an operator-provided Netscape cookie file at
  `~/.local/share/cocoonbox/video-extract/cookies.txt`; it is never returned by the
  tool. No browser cookies are read by default.
- The tool does not determine copyright status or override the calling model's
  policies. Bilibili availability, authentication, and site changes can cause
  download failures; these are reported without manufacturing a document.

## Verify

```bash
python3 -m unittest discover -s deploy/video-extract -p 'test_*.py' -v
bash -n deploy/video-extract/install.sh
```

Exercise the installed MCP tool through start → status → document. For a real OCR
check without depending on Bilibili, generate a short FFmpeg video with known text
in the guest, call `ocr()` on it, and verify that text appears in the segments.

On this deployment's host, validate a completed real job and export the exact
documents through authenticated MCP:

```bash
sudo python3 validate-job.py JOB_ID /path/to/results
```

This retrieves every document page, compares its SHA-256 with the saved document,
checks frame/timestamp coverage and label removal, and verifies the printable
HTML matches the Markdown. It writes `document.md`, `document.html`, and a
`validation.json` report. Exit code 2 means the job is still running.
