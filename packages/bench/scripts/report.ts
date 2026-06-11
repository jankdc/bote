// Render a matrix run (matrix.ts JSONL) as an aligned markdown table for a
// PR comment.
//
//   --in <path>   read JSONL from <path> instead of stdin
//   --out <path>  write markdown to <path> instead of stdout

import { readFileSync, writeFileSync } from 'node:fs';

import type { Result } from '#lib/cells.ts';
import { arg, parseJsonl } from '#lib/cli.ts';
import { fmtBytes, fmtNs } from '#lib/format.ts';
import { columnWidths, row, rule } from '#lib/table.ts';

const inPath = arg('--in');
const outPath = arg('--out');

function readInput(): string {
  return inPath ? readFileSync(inPath, 'utf8') : readFileSync(0, 'utf8');
}

const results = parseJsonl<Result>(readInput());

const sorted = [...results].sort(
  (a, b) =>
    a.cell.op.localeCompare(b.cell.op) ||
    a.cell.accessPattern.localeCompare(b.cell.accessPattern) ||
    (a.cell.consume ?? '').localeCompare(b.cell.consume ?? '') ||
    a.cell.docShape.localeCompare(b.cell.docShape) ||
    a.cell.docSize - b.cell.docSize ||
    (a.cell.batch ?? 0) - (b.cell.batch ?? 0),
);

const header = ['operation', 'document', 'bote'] as const;

const rows = sorted.map((r): string[] => {
  const batch = r.cell.batch !== undefined ? ` · batch=${r.cell.batch.toLocaleString()}` : '';
  const access = r.cell.consume ?? r.cell.accessPattern;
  const op = `${r.cell.op} · ${access}${batch}`;
  const doc = `${r.cell.docShape} · n=${r.cell.docSize.toLocaleString()}`;
  if (r.error) {
    return [op, doc, '⚠️ error'];
  }
  let bote = fmtNs(r.timing.min_ns);
  if (r.timing.first_item_ns) {
    bote += ' _(to 1st)_';
  }
  return [op, doc, bote];
});

const widths = columnWidths(header, rows);
const tableRow = (cols: readonly string[]): string => `| ${row(cols, widths, ' | ')} |`;
const sep = `| ${rule(widths, '-', ' | ')} |`;

const chunk = results.find((r) => !r.error)?.cell.chunkBytes;
const caption = chunk ? `file source, chunk=${fmtBytes(chunk)}` : 'no cells';

// Column legend (moved here from the matrix driver): the table is the report,
// so explain its columns next to it rather than the raw cell-id format.
const legend = [
  '`operation` - `<op> · <access>`: op is get | has | iter; access is how far the lookup reaches (shallow/deep), the traversal kind (iter-all/obj-iter/obj-iter-name/obj-iter-first), or the `iter` consumption mode (raw/toArray/forEach/reduce/find/some/every/map/filter/take/drop).',
  '`document` - `<shape> · n=<size>`: shape is the JSON document shape; n is the array/object member or key count, or nesting depth for deep-nested.',
  '`bote` - fastest-sample whole-operation wall-clock (compare across runs on the same machine); `_(to 1st)_` marks time to the first member (obj-iter-first).',
]
  .map((l) => `- ${l}`)
  .join('\n');

const md = `_${caption}._\n\n` + [tableRow(header), sep, ...rows.map(tableRow)].join('\n') + `\n\n${legend}\n`;

if (outPath) {
  writeFileSync(outPath, md);
} else {
  process.stdout.write(md);
}
