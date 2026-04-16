import * as Diff from "diff";

export interface Edit {
	oldText: string;
	newText: string;
}

export interface AppliedEditsResult {
	baseContent: string;
	newContent: string;
}

function normalizeForFuzzyMatch(text: string): string {
	return text
		.normalize("NFKC")
		.split("\n")
		.map((line) => line.trimEnd())
		.join("\n")
		.replace(/[\u2018\u2019\u201A\u201B]/g, "'")
		.replace(/[\u201C\u201D\u201E\u201F]/g, '"')
		.replace(/[\u2010\u2011\u2012\u2013\u2014\u2015\u2212]/g, "-")
		.replace(/[\u00A0\u2002-\u200A\u202F\u205F\u3000]/g, " ");
}

type FuzzyMatchResult = {
	found: boolean;
	index: number;
	matchLength: number;
	usedFuzzyMatch: boolean;
	contentForReplacement: string;
};

type MatchedEdit = {
	editIndex: number;
	matchIndex: number;
	matchLength: number;
	newText: string;
};

function fuzzyFindText(content: string, oldText: string): FuzzyMatchResult {
	const exactIndex = content.indexOf(oldText);
	if (exactIndex !== -1) {
		return {
			found: true,
			index: exactIndex,
			matchLength: oldText.length,
			usedFuzzyMatch: false,
			contentForReplacement: content,
		};
	}

	const fuzzyContent = normalizeForFuzzyMatch(content);
	const fuzzyOldText = normalizeForFuzzyMatch(oldText);
	const fuzzyIndex = fuzzyContent.indexOf(fuzzyOldText);
	if (fuzzyIndex === -1) {
		return {
			found: false,
			index: -1,
			matchLength: 0,
			usedFuzzyMatch: false,
			contentForReplacement: content,
		};
	}

	return {
		found: true,
		index: fuzzyIndex,
		matchLength: fuzzyOldText.length,
		usedFuzzyMatch: true,
		contentForReplacement: fuzzyContent,
	};
}

function countOccurrences(content: string, oldText: string): number {
	const fuzzyContent = normalizeForFuzzyMatch(content);
	const fuzzyOldText = normalizeForFuzzyMatch(oldText);
	return fuzzyContent.split(fuzzyOldText).length - 1;
}

function getNotFoundError(path: string, editIndex: number, totalEdits: number): Error {
	if (totalEdits === 1) {
		return new Error(
			`Could not find the exact text in ${path}. The old text must match exactly including all whitespace and newlines.`,
		);
	}
	return new Error(
		`Could not find edits[${editIndex}] in ${path}. The oldText must match exactly including all whitespace and newlines.`,
	);
}

function getDuplicateError(path: string, editIndex: number, totalEdits: number, occurrences: number): Error {
	if (totalEdits === 1) {
		return new Error(
			`Found ${occurrences} occurrences of the text in ${path}. The text must be unique. Please provide more context to make it unique.`,
		);
	}
	return new Error(
		`Found ${occurrences} occurrences of edits[${editIndex}] in ${path}. Each oldText must be unique. Please provide more context to make it unique.`,
	);
}

export function detectLineEnding(content: string): "\r\n" | "\n" {
	const crlfIndex = content.indexOf("\r\n");
	const lfIndex = content.indexOf("\n");
	if (lfIndex === -1 || crlfIndex === -1) {
		return "\n";
	}
	return crlfIndex < lfIndex ? "\r\n" : "\n";
}

export function normalizeToLF(text: string): string {
	return text.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}

export function restoreLineEndings(text: string, ending: "\r\n" | "\n"): string {
	return ending === "\r\n" ? text.replace(/\n/g, "\r\n") : text;
}

export function stripBom(content: string): { bom: string; text: string } {
	return content.startsWith("\uFEFF") ? { bom: "\uFEFF", text: content.slice(1) } : { bom: "", text: content };
}

export function applyEditsToNormalizedContent(
	normalizedContent: string,
	edits: Edit[],
	path: string,
): AppliedEditsResult {
	const normalizedEdits = edits.map((edit) => ({
		oldText: normalizeToLF(edit.oldText),
		newText: normalizeToLF(edit.newText),
	}));

	for (let i = 0; i < normalizedEdits.length; i++) {
		if (normalizedEdits[i]?.oldText.length === 0) {
			throw new Error(
				normalizedEdits.length === 1 ? `oldText must not be empty in ${path}.` : `edits[${i}].oldText must not be empty in ${path}.`,
			);
		}
	}

	const initialMatches = normalizedEdits.map((edit) => fuzzyFindText(normalizedContent, edit.oldText));
	const baseContent = initialMatches.some((match) => match.usedFuzzyMatch)
		? normalizeForFuzzyMatch(normalizedContent)
		: normalizedContent;

	const matchedEdits: MatchedEdit[] = [];
	for (let i = 0; i < normalizedEdits.length; i++) {
		const edit = normalizedEdits[i]!;
		const matchResult = fuzzyFindText(baseContent, edit.oldText);
		if (!matchResult.found) {
			throw getNotFoundError(path, i, normalizedEdits.length);
		}

		const occurrences = countOccurrences(baseContent, edit.oldText);
		if (occurrences > 1) {
			throw getDuplicateError(path, i, normalizedEdits.length, occurrences);
		}

		matchedEdits.push({
			editIndex: i,
			matchIndex: matchResult.index,
			matchLength: matchResult.matchLength,
			newText: edit.newText,
		});
	}

	matchedEdits.sort((a, b) => a.matchIndex - b.matchIndex);
	for (let i = 1; i < matchedEdits.length; i++) {
		const previous = matchedEdits[i - 1]!;
		const current = matchedEdits[i]!;
		if (previous.matchIndex + previous.matchLength > current.matchIndex) {
			throw new Error(
				`edits[${previous.editIndex}] and edits[${current.editIndex}] overlap in ${path}. Merge them into one edit or target disjoint regions.`,
			);
		}
	}

	let newContent = baseContent;
	for (let i = matchedEdits.length - 1; i >= 0; i--) {
		const edit = matchedEdits[i]!;
		newContent =
			newContent.substring(0, edit.matchIndex) +
			edit.newText +
			newContent.substring(edit.matchIndex + edit.matchLength);
	}

	if (baseContent === newContent) {
		throw new Error(
			normalizedEdits.length === 1
				? `No changes made to ${path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.`
				: `No changes made to ${path}. The replacements produced identical content.`,
		);
	}

	return { baseContent, newContent };
}

export function generateDiffString(
	oldContent: string,
	newContent: string,
	contextLines = 4,
): { diff: string; firstChangedLine: number | undefined } {
	const parts = Diff.diffLines(oldContent, newContent);
	const output: string[] = [];
	const oldLines = oldContent.split("\n");
	const newLines = newContent.split("\n");
	const lineNumberWidth = String(Math.max(oldLines.length, newLines.length)).length;

	let oldLineNumber = 1;
	let newLineNumber = 1;
	let lastWasChange = false;
	let firstChangedLine: number | undefined;

	for (let i = 0; i < parts.length; i++) {
		const part = parts[i]!;
		const rawLines = part.value.split("\n");
		if (rawLines[rawLines.length - 1] === "") {
			rawLines.pop();
		}

		if (part.added || part.removed) {
			if (firstChangedLine === undefined) {
				firstChangedLine = newLineNumber;
			}

			for (const line of rawLines) {
				if (part.added) {
					output.push(`+${String(newLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
					newLineNumber++;
				} else {
					output.push(`-${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
					oldLineNumber++;
				}
			}
			lastWasChange = true;
			continue;
		}

		const nextPartIsChange = i < parts.length - 1 && (parts[i + 1]?.added || parts[i + 1]?.removed);
		const hasLeadingChange = lastWasChange;
		const hasTrailingChange = nextPartIsChange;

		if (hasLeadingChange && hasTrailingChange) {
			if (rawLines.length <= contextLines * 2) {
				for (const line of rawLines) {
					output.push(` ${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
					oldLineNumber++;
					newLineNumber++;
				}
			} else {
				const leadingLines = rawLines.slice(0, contextLines);
				const trailingLines = rawLines.slice(rawLines.length - contextLines);
				const skippedLines = rawLines.length - leadingLines.length - trailingLines.length;
				for (const line of leadingLines) {
					output.push(` ${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
					oldLineNumber++;
					newLineNumber++;
				}
				output.push(` ${"".padStart(lineNumberWidth, " ")} ...`);
				oldLineNumber += skippedLines;
				newLineNumber += skippedLines;
				for (const line of trailingLines) {
					output.push(` ${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
					oldLineNumber++;
					newLineNumber++;
				}
			}
		} else if (hasLeadingChange) {
			const shownLines = rawLines.slice(0, contextLines);
			const skippedLines = rawLines.length - shownLines.length;
			for (const line of shownLines) {
				output.push(` ${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
				oldLineNumber++;
				newLineNumber++;
			}
			if (skippedLines > 0) {
				output.push(` ${"".padStart(lineNumberWidth, " ")} ...`);
				oldLineNumber += skippedLines;
				newLineNumber += skippedLines;
			}
		} else if (hasTrailingChange) {
			const skippedLines = Math.max(0, rawLines.length - contextLines);
			if (skippedLines > 0) {
				output.push(` ${"".padStart(lineNumberWidth, " ")} ...`);
				oldLineNumber += skippedLines;
				newLineNumber += skippedLines;
			}
			for (const line of rawLines.slice(skippedLines)) {
				output.push(` ${String(oldLineNumber).padStart(lineNumberWidth, " ")} ${line}`);
				oldLineNumber++;
				newLineNumber++;
			}
		} else {
			oldLineNumber += rawLines.length;
			newLineNumber += rawLines.length;
		}

		lastWasChange = false;
	}

	return { diff: output.join("\n"), firstChangedLine };
}
