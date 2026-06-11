import { closeSync, existsSync, openSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';

import { fmtBytes, fmtNs } from '#lib/format.ts';

export function makeRecord(i: number): object {
  return {
    id: `evt_${i.toString(36)}`,
    type: ['PushEvent', 'PullRequestEvent', 'IssueCommentEvent', 'ForkEvent', 'WatchEvent'][i % 5],
    actor: { id: i * 31 + 7, login: `user_${i}`, url: `https://api.github.com/users/user_${i}` },
    repo: { id: i * 17 + 3, name: `org_${i % 1000}/repo_${i % 5000}` },
    payload: {
      ref: `refs/heads/branch_${i % 200}`,
      size: (i % 7) + 1,
      message: `Commit ${i}: tidy file_${i % 500}.ts`,
    },
    public: true,
    created_at: new Date(1700000000000 + i * 1000).toISOString(),
  };
}

function generate(path: string, targetBytes: number): { count: number; bytes: number } {
  const fd = openSync(path, 'w');
  try {
    const chunks: Buffer[] = [];
    let buffered = 0;
    let written = 0;
    const flush = (): void => {
      if (buffered === 0) {
        return;
      }
      const merged = Buffer.concat(chunks, buffered);
      writeSync(fd, merged);
      written += merged.byteLength;
      chunks.length = 0;
      buffered = 0;
    };
    const push = (s: string): void => {
      const b = Buffer.from(s, 'utf8');
      chunks.push(b);
      buffered += b.byteLength;
      if (buffered >= 4 * 1024 * 1024) {
        flush();
      }
    };
    push('[');
    let count = 0;
    while (written + buffered < targetBytes - 2) {
      push((count === 0 ? '' : ',') + JSON.stringify(makeRecord(count)));
      count++;
    }
    push(']');
    flush();
    return { count, bytes: written };
  } finally {
    closeSync(fd);
  }
}

export function ensureFixture(targetBytes: number): { filePath: string; count: number; bytes: number } {
  const filePath = join(tmpdir(), `bote-showcase-${targetBytes}.json`);
  const sidecarPath = `${filePath}.meta.json`;
  if (existsSync(filePath) && existsSync(sidecarPath)) {
    const stat = statSync(filePath);
    const meta = JSON.parse(readFileSync(sidecarPath, 'utf8')) as { count: number; bytes: number };
    if (stat.size === meta.bytes) {
      console.error(`reusing fixture: ${filePath} (${fmtBytes(stat.size)}, ${meta.count.toLocaleString()} items)`);
      return { filePath, count: meta.count, bytes: meta.bytes };
    }
  }
  console.error(`generating fixture ~${fmtBytes(targetBytes)} at ${filePath}...`);
  const t0 = performance.now();
  const { count, bytes } = generate(filePath, targetBytes);
  writeFileSync(sidecarPath, JSON.stringify({ count, bytes }));
  console.error(
    `wrote ${fmtBytes(bytes)} (${count.toLocaleString()} items) in ${fmtNs((performance.now() - t0) * 1e6)}`,
  );
  return { filePath, count, bytes };
}
