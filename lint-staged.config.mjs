// Gated by file type: Rust commands only fire when *.rs is staged, the
// type-check only when *.ts is staged. Within a glob, commands run in order
// (fix, then format, then check), so writers never race the reader.
export default {
  '*.{ts,tsx}': ['oxlint --fix', 'prettier --write', () => 'npm run typecheck'],
  '*.js': ['oxlint --fix', 'prettier --write'],
  '*.{yml,yaml,md,json}': ['prettier --write'],
  '*.rs': [() => 'cargo fmt', () => 'cargo clippy --all-targets -- -D warnings'],
};
