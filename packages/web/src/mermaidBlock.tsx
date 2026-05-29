import { memo, useEffect, useRef, useState } from "react";

// Mermaid is large (>1MB gzipped). Load it on first use only, and share a
// single promise across all blocks on the page.
type MermaidApi = {
	initialize: (config: Record<string, unknown>) => void;
	parse: (text: string, options?: { suppressErrors?: boolean }) => Promise<unknown>;
	render: (id: string, text: string) => Promise<{ svg: string }>;
};

let mermaidPromise: Promise<MermaidApi> | null = null;
let currentTheme: "default" | "dark" | null = null;

function loadMermaid(theme: "default" | "dark"): Promise<MermaidApi> {
	if (!mermaidPromise) {
		mermaidPromise = import("mermaid").then((mod) => {
			const api = (mod.default ?? mod) as unknown as MermaidApi;
			api.initialize({
				startOnLoad: false,
				securityLevel: "strict",
				theme,
				fontFamily: "var(--font-sans), system-ui, sans-serif"
			});
			currentTheme = theme;
			return api;
		});
		return mermaidPromise;
	}
	return mermaidPromise.then((api) => {
		if (currentTheme !== theme) {
			api.initialize({
				startOnLoad: false,
				securityLevel: "strict",
				theme,
				fontFamily: "var(--font-sans), system-ui, sans-serif"
			});
			currentTheme = theme;
		}
		return api;
	});
}

function prefersDark(): boolean {
	if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
	return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

// Monotonic counter so each render() call gets a unique DOM id, as required by
// mermaid.render(). The id only needs to be unique per render attempt.
let nextMermaidId = 0;

export type MermaidBlockProps = {
	code: string;
};

export const MermaidBlock = memo(function MermaidBlock({ code }: MermaidBlockProps) {
	const [svg, setSvg] = useState<string | null>(null);
	const [error, setError] = useState<string | null>(null);
	const [isDark, setIsDark] = useState<boolean>(prefersDark);
	const renderIdRef = useRef(0);

	// Track prefers-color-scheme changes so the diagram restyles with the page.
	useEffect(() => {
		if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
		const media = window.matchMedia("(prefers-color-scheme: dark)");
		const handler = (event: MediaQueryListEvent) => setIsDark(event.matches);
		media.addEventListener("change", handler);
		return () => media.removeEventListener("change", handler);
	}, []);

	useEffect(() => {
		const trimmed = code.trim();
		if (trimmed.length === 0) {
			setSvg(null);
			setError(null);
			return;
		}

		// Each effect run gets a fresh id so an in-flight stale render cannot
		// clobber a newer one (we compare ids in the promise chain).
		const runId = ++renderIdRef.current;
		const domId = `mermaid-svg-${++nextMermaidId}`;
		const theme = isDark ? "dark" : "default";
		let cancelled = false;

		loadMermaid(theme)
			.then(async (api) => {
				// parse() in suppressErrors mode resolves to a falsy value when
				// the input isn't a valid diagram yet (common while streaming).
				const ok = await api.parse(trimmed, { suppressErrors: true });
				if (cancelled || runId !== renderIdRef.current) return null;
				if (!ok) return null;
				return api.render(domId, trimmed);
			})
			.then((result) => {
				if (cancelled || runId !== renderIdRef.current) return;
				if (!result) {
					// Not (yet) a valid diagram — fall back to showing source.
					setSvg(null);
					setError(null);
					return;
				}
				setSvg(result.svg);
				setError(null);
			})
			.catch((err: unknown) => {
				if (cancelled || runId !== renderIdRef.current) return;
				setSvg(null);
				setError(err instanceof Error ? err.message : String(err));
			});

		return () => {
			cancelled = true;
		};
	}, [code, isDark]);

	if (error) {
		return (
			<pre className="mermaid-source mermaid-source-error" data-mermaid-error={error}>
				<code className="language-mermaid">{code}</code>
			</pre>
		);
	}

	if (!svg) {
		// Streaming / pre-render placeholder. Render the source so users still
		// see something useful while the diagram parses.
		return (
			<pre className="mermaid-source">
				<code className="language-mermaid">{code}</code>
			</pre>
		);
	}

	return (
		<div className="mermaid-diagram" role="img" aria-label="Mermaid diagram" dangerouslySetInnerHTML={{ __html: svg }} />
	);
});
