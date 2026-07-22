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
let mermaidOperationQueue: Promise<void> = Promise.resolve();
const MERMAID_VIEWPORT_MARGIN = "600px 0px";

function loadMermaid(theme: "default" | "dark"): Promise<MermaidApi> {
	if (!mermaidPromise) {
		const importedPromise = import("mermaid").then((mod) => {
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
		const loadingPromise = importedPromise.catch((error: unknown) => {
			// A failed import must not poison every later Mermaid block. Only
			// clear state if this is still the active load; a later retry may
			// already have replaced it.
			if (mermaidPromise === loadingPromise) {
				mermaidPromise = null;
				currentTheme = null;
			}
			throw error;
		});
		mermaidPromise = loadingPromise;
		return loadingPromise;
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

function enqueueMermaidOperation<T>(operation: () => Promise<T>): Promise<T> {
	const result = mermaidOperationQueue.then(operation);
	// Keep the queue usable after either a successful or failed operation.
	mermaidOperationQueue = result.then(
		() => undefined,
		() => undefined,
	);
	return result;
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
	const [shouldRender, setShouldRender] = useState(false);
	const sourceRef = useRef<HTMLPreElement>(null);
	const renderIdRef = useRef(0);

	// Render shortly before the source scrolls into view. Once activated, keep
	// rendering updates so scrolling away does not replace the SVG with source.
	useEffect(() => {
		const source = sourceRef.current;
		if (!source) return;
		if (typeof IntersectionObserver === "undefined") {
			setShouldRender(true);
			return;
		}

		const observer = new IntersectionObserver(
			(entries) => {
				if (!entries.some((entry) => entry.isIntersecting)) return;
				observer.disconnect();
				setShouldRender(true);
			},
			{
				root: source.closest<HTMLElement>(".message-scroll"),
				rootMargin: MERMAID_VIEWPORT_MARGIN,
			},
		);
		observer.observe(source);
		return () => observer.disconnect();
	}, []);

	// Track prefers-color-scheme changes so the diagram restyles with the page.
	useEffect(() => {
		if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
		const media = window.matchMedia("(prefers-color-scheme: dark)");
		const handler = (event: MediaQueryListEvent) => setIsDark(event.matches);
		media.addEventListener("change", handler);
		return () => media.removeEventListener("change", handler);
	}, []);

	useEffect(() => {
		if (!shouldRender) return;

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

		enqueueMermaidOperation(async () => {
			const api = await loadMermaid(theme);
			if (cancelled || runId !== renderIdRef.current) return null;

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
	}, [code, isDark, shouldRender]);

	if (error) {
		return (
			<pre ref={sourceRef} className="mermaid-source mermaid-source-error" data-mermaid-error={error}>
				<code className="language-mermaid">{code}</code>
			</pre>
		);
	}

	if (!svg) {
		// Streaming / pre-render placeholder. Render the source so users still
		// see something useful while the diagram parses.
		return (
			<pre ref={sourceRef} className="mermaid-source">
				<code className="language-mermaid">{code}</code>
			</pre>
		);
	}

	return (
		<div className="mermaid-diagram" role="img" aria-label="Mermaid diagram" dangerouslySetInnerHTML={{ __html: svg }} />
	);
});
