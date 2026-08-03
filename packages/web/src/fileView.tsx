import { memo, useEffect, useMemo, useState } from "react";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import { bytesToUtf8Prefix } from "./fileBrowser.ts";
import type { CachedWorkspaceFile } from "./workspaceFileCache.ts";
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
	file: CachedWorkspaceFile;
	onNavigate?: (path: string) => void;
}) {
	const bytes = file.bytes;
	const ext = extensionOf(file.path);
	const mime = IMAGE_EXTENSIONS.has(ext) ? sniffRasterMime(bytes) : null;
	const [imageUrl, setImageUrl] = useState<string | null>(null);

	useEffect(() => {
		if (!mime) {
			setImageUrl(null);
			return;
		}
		const url = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
		setImageUrl(url);
		return () => URL.revokeObjectURL(url);
	}, [bytes, mime]);

	if (mime && imageUrl) {
		return (
			<div className="file-image-view">
				<img src={imageUrl} alt={browsePathBasename(file.path)} />
			</div>
		);
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
		const truncatedDisplay = bytes.byteLength > TEXT_RENDER_CAP || highlightSource.length < decoded.text.length;
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
					{truncatedDisplay ? (
						<p className="muted file-truncated-note">
							Showing first {Math.min(bytes.byteLength, HIGHLIGHT_CAP).toLocaleString()} of{" "}
							{file.totalSize.toLocaleString()} bytes.
						</p>
					) : null}
				</div>
			);
		}
		return (
			<div className="file-text-view">
				<pre className="file-text-pre">{decoded.text.slice(0, TEXT_RENDER_CAP)}</pre>
				{bytes.byteLength > TEXT_RENDER_CAP ? (
					<p className="muted file-truncated-note">
						Showing first {TEXT_RENDER_CAP.toLocaleString()} of {file.totalSize.toLocaleString()} bytes.
					</p>
				) : null}
			</div>
		);
	}

	return (
		<div className="file-unavailable">
			<p className="muted">Preview unavailable.</p>
			<p className="muted">
				{file.totalSize.toLocaleString()} bytes
				{file.mtimeMs ? ` · mtime ${new Date(file.mtimeMs).toLocaleString()}` : ""}
			</p>
		</div>
	);
});
