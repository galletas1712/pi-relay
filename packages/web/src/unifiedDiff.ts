/** Parse unified diff text into display rows (skips file headers). */

export type UnifiedDiffRowKind = "add" | "remove" | "context" | "hunk" | "meta";

export interface UnifiedDiffRow {
	kind: UnifiedDiffRowKind;
	text: string;
}

export function parseUnifiedDiff(unified: string): UnifiedDiffRow[] {
	const rows: UnifiedDiffRow[] = [];
	for (const line of unified.split("\n")) {
		if (line.startsWith("diff --git ") || line.startsWith("index ") || line.startsWith("+++ ") || line.startsWith("--- ")) {
			rows.push({ kind: "meta", text: line });
			continue;
		}
		if (line.startsWith("@@")) {
			rows.push({ kind: "hunk", text: line });
			continue;
		}
		if (line.startsWith("+")) {
			rows.push({ kind: "add", text: line.slice(1) });
			continue;
		}
		if (line.startsWith("-")) {
			rows.push({ kind: "remove", text: line.slice(1) });
			continue;
		}
		if (line.startsWith("\\")) {
			rows.push({ kind: "meta", text: line });
			continue;
		}
		rows.push({ kind: "context", text: line.startsWith(" ") ? line.slice(1) : line });
	}
	if (rows.length === 1 && rows[0]?.text === "") return [];
	return rows;
}

export function diffMarker(kind: UnifiedDiffRowKind): string {
	switch (kind) {
		case "add":
			return "+";
		case "remove":
			return "-";
		case "hunk":
			return "@";
		default:
			return " ";
	}
}
