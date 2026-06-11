// Child node-process helpers shared by the spawning drivers.

import { spawn } from 'node:child_process';

export interface NodeRun {
  /** Everything the child wrote to stdout (stderr is inherited, not captured). */
  stdout: string;
  /** Exit code, or `null` if the child died from a signal or never spawned. */
  code: number | null;
  /** Set only when the spawn itself failed (binary missing, etc.). */
  error?: Error;
}

/** Spawn a fresh `node` running `args`, optionally feeding `input` to stdin, and
 *  resolve once it closes with the collected stdout. stderr streams through to
 *  our own. Never rejects — a spawn failure resolves with `error` set. */
export function runNode(args: readonly string[], opts?: { input?: string }): Promise<NodeRun> {
  return new Promise((resolve) => {
    const stdin = opts?.input !== undefined ? 'pipe' : 'ignore';
    const child = spawn(process.execPath, [...args], { stdio: [stdin, 'pipe', 'inherit'] });
    let stdout = '';
    // stdout is always piped (see stdio above); stdin only when `input` is given.
    child.stdout!.setEncoding('utf8');
    child.stdout!.on('data', (d) => {
      stdout += d;
    });
    child.on('error', (error) => resolve({ stdout, code: null, error }));
    child.on('close', (code) => resolve({ stdout, code }));
    if (opts?.input !== undefined) {
      child.stdin!.write(opts.input);
      child.stdin!.end();
    }
  });
}
