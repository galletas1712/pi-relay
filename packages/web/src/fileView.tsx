import { memo, useMemo } from "react";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { bytesToUtf8Prefix, decodeBase64 } from "./fileBrowser.ts";
import type { WorkspaceFilePrefix } from "./types.ts";
import { browsePathBasename, joinBrowsePath, parentBrowsePath, validateBrowsePath } from "./filePath.ts";

const MARKDOWN_EXTENSIONS = new Set(["md", "markdown"]);
const IMAGE_EXTENSIONS = new Set(["png", "jpg", "jpeg", "gif", "webp"]);
const CODE_EXTENSIONS: Record<string, string> = {
	rs: "rust",
	ts: "typescript",
	tsx: "typescript",
	js: "javascript",
	jsx: "javascript",
	mjs: "javascript",
	cjs: "javascript",
	css: "css",
	html: "xml",
	htm: "xml",
	json: "json",
	yaml: "yaml",
	yml: "yaml",
	toml: "toml",
	py: "python",
	sh: "bash",
	bash: "bash",
	zsh: "bash",
	sql: "sql",
	md: "markdown",
	go: "go",
	java: "java",
	kt: "kotlin",
	c: "c",
	h: "c",
	cpp: "cpp",
	hpp: "cpp",
	rb: "ruby",
	php: "php",
	swift: "swift",
	xml: "xml",
	svg: "xml",
};

const TEXT_RENDER_CAP = 256 * 1024;
const HIGHLIGHT_CAP = 64 * 1024;
const MARKDOWN_PARSE_CAP = 128 * 1024;
const IMAGE_MAX = 1024 * 1024;

function extensionOf(path: string): string {
	const base = browsePathBasename(path);
	const idx = base.lastIndexOf(".");
	if (idx <= 0) return "";
	return base.slice(idx + 1).toLowerCase();
}

function sniffRasterMime(bytes: Uint8Array): string | null {
	if (bytes.length >= 8 && bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47) {
		return "image/png";
	}
	if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) {
		return "image/jpeg";
	}
	if (
		bytes.length >= 6 &&
		bytes[0] === 0x47 &&
		bytes[1] === 0x49 &&
		bytes[2] === 0x46 &&
		bytes[3] === 0x38 &&
		(bytes[4] === 0x39 || bytes[4] === 0x37) &&
		bytes[5] === 0x61
	) {
		return "image/gif";
	}
	if (
		bytes.length >= 12 &&
		bytes[0] === 0x52 &&
		bytes[1] === 0x49 &&
		bytes[2] === 0x46 &&
		bytes[3] === 0x46 &&
		bytes[8] === 0x57 &&
		bytes[9] === 0x45 &&
		bytes[10] === 0x42 &&
		bytes[11] === 0x50
	) {
		return "image/webp";
	}
	return null;
}

function fileMarkdownComponents(onNavigate: ((path: string) => void) | undefined, currentPath: string): Components {
	return {
		a: ({ href, children, ...props }) => {
			if (href && !/^[a-z][a-z0-9+.-]*:/i.test(href) && !href.startsWith("//") && !href.startsWith("#")) {
				const resolved = validateBrowsePath(joinBrowsePath(parentBrowsePath(currentPath), href.replace(/^\.\//, "")));
				if (resolved != null && onNavigate) {
					return (
						<a
							href={`?file=${encodeURIComponent(resolved)}`}
							{...props}
							onClick={(event) => {
								event.preventDefault();
								onNavigate(resolved);
							}}
						>
							{children}
						</a>
					);
				}
			}
			return (
				<a href={href} target="_blank" rel="noreferrer" {...props}>
					{children}
				</a>
			);
		},
		img: ({ alt, src }) => (
			<span className="file-md-image-fallback" title={src ?? undefined}>
				{alt || src || "image"}
			</span>
		),
	};
}

export const FileMarkdownView = memo(function FileMarkdownView({
	text,
	path,
	onNavigate,
}: {
	text: string;
	path: string;
	onNavigate?: (path: string) => void;
}) {
	const components = useMemo(() => fileMarkdownComponents(onNavigate, path), [onNavigate, path]);
	const clipped = text.length > MARKDOWN_PARSE_CAP ? text.slice(0, MARKDOWN_PARSE_CAP) : text;
	return (
		<div className="assistant-markdown file-markdown">
			<ReactMarkdown
				rehypePlugins={[[rehypeHighlight, { detect: false }]]}
				remarkPlugins={[remarkGfm]}
				components={components}
			>
				{clipped}
			</ReactMarkdown>
			{text.length > MARKDOWN_PARSE_CAP ? (
				<p className="muted file-truncated-note">Showing first {MARKDOWN_PARSE_CAP.toLocaleString()} characters.</p>
			) : null}
		</div>
	);
});

export const FileView = memo(function FileView({
	file,
	onNavigate,
}: {
	file: WorkspaceFilePrefix;
	onNavigate?: (path: string) => void;
}) {
	const bytes = useMemo(() => decodeBase64(file.content_base64), [file.content_base64]);
	const ext = extensionOf(file.path);

	if (IMAGE_EXTENSIONS.has(ext) && file.eof && file.total_size <= IMAGE_MAX) {
		const mime = sniffRasterMime(bytes);
		if (mime) {
			const dataUrl = `data:${mime};base64,${file.content_base64}`;
			return (
				<div className="file-image-view">
					<img src={dataUrl} alt={browsePathBasename(file.path)} />
				</div>
			);
		}
	}

	if (MARKDOWN_EXTENSIONS.has(ext)) {
		const decoded = bytesToUtf8Prefix(bytes);
		if (!decoded.binary) {
			return <FileMarkdownView text={decoded.text} path={file.path} onNavigate={onNavigate} />;
		}
	}

	const decoded = bytesToUtf8Prefix(bytes.slice(0, TEXT_RENDER_CAP));
	if (!decoded.binary) {
		const language = CODE_EXTENSIONS[ext];
		const highlightSource =
			decoded.text.length > HIGHLIGHT_CAP ? decoded.text.slice(0, HIGHLIGHT_CAP) : decoded.text;
		if (language) {
			return (
				<div className="file-code-view">
					<pre className="file-code-pre">
						{/* Use a fenced block so rehype-highlight runs through the same path as chat. */}
						<div className="assistant-markdown file-code-markdown">
							<ReactMarkdown rehypePlugins={[[rehypeHighlight, { detect: false }]]}>
								{`\`\`\`${language}\n${highlightSource}\n\`\`\``}
							</ReactMarkdown>
						</div>
					</pre>
					{!file.eof || decoded.text.length > HIGHLIGHT_CAP ? (
						<p className="muted file-truncated-note">
							Showing first {Math.min(file.byte_len, HIGHLIGHT_CAP).toLocaleString()} of{" "}
							{file.total_size.toLocaleString()} bytes.
						</p>
					) : null}
				</div>
			);
		}
		return (
			<div className="file-text-view">
				<pre className="file-text-pre">{decoded.text.slice(0, TEXT_RENDER_CAP)}</pre>
				{!file.eof ? (
					<p className="muted file-truncated-note">
						Showing first {file.byte_len.toLocaleString()} of {file.total_size.toLocaleString()} bytes.
					</p>
				) : null}
			</div>
		);
	}

	return (
		<div className="file-unavailable">
			<p className="muted">Preview unavailable.</p>
			<p className="muted">
				{file.total_size.toLocaleString()} bytes
				{file.mtime_ms ? ` · mtime ${new Date(file.mtime_ms).toLocaleString()}` : ""}
			</p>
		</div>
	);
});
