// Compatibility entry point for ChatGPT connections that do not expose bilibili_run.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const run = promisify(execFile);
export async function main(params = {}) {
  // The extractor's CLI accepts the same params as bilibili_run.
  const env = { ...process.env };
  delete env.CB_PARAMS;
  delete env.CB_RESULT;
  const { stdout } = await run(
    '/usr/bin/python3',
    ['/home/user/.local/share/cocoonbox/video-extract/extract.py', JSON.stringify(params)],
    { env, timeout: 95000, maxBuffer: 1024 * 1024 },
  );
  return JSON.parse(stdout);
}

