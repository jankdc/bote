// Render a matrix run (matrix.ts JSONL) as a markdown table for a PR
// comment.
//
//   --in <path>   read JSONL from <path> instead of stdin
//   --out <path>  write markdown to <path> instead of stdout

import { readFileSync, writeFileSync } from 'node:fs'

import type { Result } from './cells.ts'
import { arg } from './cli.ts'
import { fmtBytes, fmtNs } from './format.ts'

const inPath = arg('--in')
const outPath = arg('--out')

function readInput(): string {
  return inPath ? readFileSync(inPath, 'utf8') : readFileSync(0, 'utf8')
}

function parseJsonl(text: string): Result[] {
  const out: Result[] = []
  for (const line of text.split('\n')) {
    const trimmed = line.trim()
    if (!trimmed) continue
    try {
      out.push(JSON.parse(trimmed) as Result)
    } catch {
      // skip malformed lines
    }
  }
  return out
}

// Keep small sub-1 ratios legible (a point op can be ~0.001x of a
// full-doc parse) instead of rounding to "0.00".
function fmtRatio(r: number): string {
  if (r >= 1) return r.toFixed(2)
  return r.toPrecision(2).replace(/\.?0+$/, '')
}

const results = parseJsonl(readInput())

const rows = results.map((r) => {
  const label = `${r.cell.op} · ${r.cell.accessPattern}`
  if (r.error) return `| ${label} | — | — | ⚠️ error |`
  const streaming = r.cell.op === 'walk' || r.cell.op === 'iter'
  const bote = r.timing.first_item_ns
    ? `${fmtNs(r.timing.first_item_ns)} to 1st`
    : streaming && r.timing.ns_per_item
      ? `${fmtNs(r.timing.ns_per_item)}/item`
      : fmtNs(r.timing.p50_ns)
  const parse = r.reference ? fmtNs(r.reference.parse_ns) : '—'
  const ratio = r.reference ? `${fmtRatio(r.reference.ratio)}x` : '—'
  return `| ${label} | ${bote} | ${parse} | ${ratio} |`
})

const sample = results.find((r) => !r.error)?.cell
const caption = sample
  ? `${sample.docShape}, ${sample.source} source, n=${sample.docSize.toLocaleString()}, chunk=${fmtBytes(sample.chunkBytes)}`
  : 'no cells'

const md =
  `_${caption}._\n\n` +
  `| operation | bote | JSON.parse | bote/parse |\n` +
  `| --- | --- | --- | --- |\n` +
  rows.join('\n') +
  '\n'

if (outPath) writeFileSync(outPath, md)
else process.stdout.write(md)
