// Render a matrix run (matrix.ts JSONL) as an aligned markdown table for a
// PR comment.
//
//   --in <path>   read JSONL from <path> instead of stdin
//   --out <path>  write markdown to <path> instead of stdout

import { readFileSync, writeFileSync } from 'node:fs'

import type { Result } from './cells.ts'
import { arg, parseJsonl } from './cli.ts'
import { fmtBytes, fmtNs } from './format.ts'

const inPath = arg('--in')
const outPath = arg('--out')

function readInput(): string {
  return inPath ? readFileSync(inPath, 'utf8') : readFileSync(0, 'utf8')
}

const results = parseJsonl<Result>(readInput())

const sorted = [...results].sort(
  (a, b) =>
    a.cell.op.localeCompare(b.cell.op) ||
    a.cell.accessPattern.localeCompare(b.cell.accessPattern) ||
    a.cell.docShape.localeCompare(b.cell.docShape) ||
    a.cell.docSize - b.cell.docSize,
)

const header = ['operation', 'document', 'bote'] as const

const rows = sorted.map((r): string[] => {
  const op = `${r.cell.op} · ${r.cell.accessPattern}`
  const doc = `${r.cell.docShape} · n=${r.cell.docSize.toLocaleString()}`
  if (r.error) return [op, doc, '⚠️ error']
  let bote = fmtNs(r.timing.min_ns)
  if (r.timing.first_item_ns) bote += ' _(to 1st)_'
  return [op, doc, bote]
})

const widths = header.map((h, i) => Math.max(h.length, ...rows.map((row) => row[i].length)))
const tableRow = (cols: readonly string[]): string => `| ${cols.map((c, i) => c.padEnd(widths[i])).join(' | ')} |`
const sep = `| ${widths.map((w) => '-'.repeat(w)).join(' | ')} |`

const chunk = results.find((r) => !r.error)?.cell.chunkBytes
const caption = chunk ? `file source, chunk=${fmtBytes(chunk)}` : 'no cells'

// Column legend (moved here from the matrix driver): the table is the report,
// so explain its columns next to it rather than the raw cell-id format.
const legend = [
  '`operation` - `<op> · <access>`: op is get | has | walk | iter; access is how far the lookup reaches (shallow/deep) or the traversal kind (iter-all/walk-all/walk-get-name/walk-first).',
  '`document` - `<shape> · n=<size>`: shape is the JSON document shape; n is the array/object member or key count, or nesting depth for deep-nested.',
  '`bote` - fastest-sample whole-operation wall-clock (compare across runs on the same machine); `_(to 1st)_` marks time to the first child (walk-first).',
]
  .map((l) => `- ${l}`)
  .join('\n')

const md = `_${caption}._\n\n` + [tableRow(header), sep, ...rows.map(tableRow)].join('\n') + `\n\n${legend}\n`

if (outPath) {
  writeFileSync(outPath, md)
} else {
  process.stdout.write(md)
}
