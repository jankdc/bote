export function fmtNs(ns: number): string {
  if (ns < 1_000) {
    return `${ns.toFixed(0)} ns`;
  }
  if (ns < 1_000_000) {
    return `${(ns / 1_000).toFixed(1)} µs`;
  }
  if (ns < 1_000_000_000) {
    return `${(ns / 1_000_000).toFixed(2)} ms`;
  }
  return `${(ns / 1_000_000_000).toFixed(2)} s`;
}

export function fmtBytes(bytes: number): string {
  const sign = bytes < 0 ? '-' : '';
  const abs = Math.abs(bytes);
  if (abs < 1024) {
    return `${sign}${abs} B`;
  }
  if (abs < 1024 ** 2) {
    return `${sign}${(abs / 1024).toFixed(1)} KB`;
  }
  if (abs < 1024 ** 3) {
    return `${sign}${(abs / 1024 ** 2).toFixed(1)} MB`;
  }
  return `${sign}${(abs / 1024 ** 3).toFixed(2)} GB`;
}
