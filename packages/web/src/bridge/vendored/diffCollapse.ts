// Vendored from github.com/pingdotgg/t3code @ 6f69b4407f1e6e1aa56e46bbb51a0b133374eeae
// (MIT, (c) 2026 T3 Tools Inc.) — see .pi/migration-research/t3-code-frontend-assessment.md.
// Source: apps/web/src/lib/diffCollapse.ts
// Local adaptation: none.
export function areAllDiffFilesCollapsed(
  fileKeys: ReadonlyArray<string>,
  collapsedFileKeys: ReadonlySet<string>,
): boolean {
  return fileKeys.length > 0 && fileKeys.every((fileKey) => collapsedFileKeys.has(fileKey));
}

export function toggleAllDiffFiles(
  fileKeys: ReadonlyArray<string>,
  collapsedFileKeys: ReadonlySet<string>,
): ReadonlySet<string> {
  return areAllDiffFilesCollapsed(fileKeys, collapsedFileKeys) ? new Set() : new Set(fileKeys);
}
