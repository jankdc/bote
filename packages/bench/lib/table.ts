export function columnWidths(header: readonly string[], rows: readonly (readonly string[])[]): number[] {
  return header.map((h, i) => Math.max(h.length, ...rows.map((row) => row[i].length)))
}

export function row(cells: readonly string[], widths: readonly number[], gap = '  '): string {
  return cells.map((c, i) => c.padEnd(widths[i])).join(gap)
}

export function rule(widths: readonly number[], char = '─', gap = '  '): string {
  return widths.map((w) => char.repeat(w)).join(gap)
}
