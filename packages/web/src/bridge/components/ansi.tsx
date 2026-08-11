// Minimal "ansi-ish" SGR renderer for the repl console (M9): colors, bright
// colors, bold/dim/italic/underline. Anything unrecognized passes through as
// plain text; non-SGR escape sequences are stripped.
import type { CSSProperties, ReactNode } from "react";

const FG_COLORS: Record<number, string> = {
	30: "#4b5563",
	31: "#ef4444",
	32: "#22c55e",
	33: "#eab308",
	34: "#3b82f6",
	35: "#a855f7",
	36: "#06b6d4",
	37: "#e5e7eb",
	90: "#9ca3af",
	91: "#f87171",
	92: "#4ade80",
	93: "#facc15",
	94: "#60a5fa",
	95: "#c084fc",
	96: "#22d3ee",
	97: "#f9fafb",
};

const BG_COLORS: Record<number, string> = Object.fromEntries(
	Object.entries(FG_COLORS).map(([code, color]) => [Number(code) + 10, color]),
);

interface SpanState {
	fg?: string;
	bg?: string;
	bold?: boolean;
	dim?: boolean;
	italic?: boolean;
	underline?: boolean;
}

function applyCodes(state: SpanState, codes: number[]): SpanState {
	let next = { ...state };
	for (const code of codes.length === 0 ? [0] : codes) {
		if (code === 0) next = {};
		else if (code === 1) next.bold = true;
		else if (code === 2) next.dim = true;
		else if (code === 3) next.italic = true;
		else if (code === 4) next.underline = true;
		else if (code === 22) {
			next.bold = false;
			next.dim = false;
		} else if (code === 23) next.italic = false;
		else if (code === 24) next.underline = false;
		else if (code === 39) next.fg = undefined;
		else if (code === 49) next.bg = undefined;
		else if (FG_COLORS[code]) next.fg = FG_COLORS[code];
		else if (BG_COLORS[code]) next.bg = BG_COLORS[code];
	}
	return next;
}

function styleOf(state: SpanState): CSSProperties {
	return {
		color: state.fg,
		backgroundColor: state.bg,
		fontWeight: state.bold ? 600 : undefined,
		opacity: state.dim ? 0.7 : undefined,
		fontStyle: state.italic ? "italic" : undefined,
		textDecoration: state.underline ? "underline" : undefined,
	};
}

// SGR sequences only; other CSI/escape sequences are stripped.
const ANSI_RE = /\x1b\[([0-9;]*)m|\x1b\[[0-9;?]*[a-zA-Z]|\x1b./g;

export function ansiToNodes(text: string): ReactNode[] {
	const nodes: ReactNode[] = [];
	let state: SpanState = {};
	let last = 0;
	let key = 0;
	for (const match of text.matchAll(ANSI_RE)) {
		const index = match.index ?? 0;
		if (index > last) {
			const chunk = text.slice(last, index);
			nodes.push(
				Object.keys(styleOf(state)).some((k) => (styleOf(state) as Record<string, unknown>)[k] !== undefined) ? (
					<span key={key++} style={styleOf(state)}>
						{chunk}
					</span>
				) : (
					chunk
				),
			);
		}
		if (match[1] !== undefined) {
			state = applyCodes(state, match[1].split(";").map((v) => (v === "" ? 0 : Number(v))));
		}
		last = index + match[0].length;
	}
	if (last < text.length) {
		const chunk = text.slice(last);
		nodes.push(
			Object.keys(styleOf(state)).some((k) => (styleOf(state) as Record<string, unknown>)[k] !== undefined) ? (
				<span key={key++} style={styleOf(state)}>
					{chunk}
				</span>
			) : (
				chunk
			),
		);
	}
	return nodes;
}

export function AnsiText({ text }: { text: string }) {
	return <>{ansiToNodes(text)}</>;
}
