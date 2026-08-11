// Confined browsing of a session cwd (port of workspaces/fs.rs).
//
// Rust used cap_std to open beneath the session cwd; Node has no cap_std, so
// confinement = strict syntactic validation (identical rules to
// validate_browse_path) + per-component lstat refusing symlinks and mount
// (dev) boundary crossings before any open/read/write.
import { constants } from "node:fs";
import { lstat, open, readdir, realpath, stat, writeFile, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { WorkspaceError } from "./errors.ts";
import { run } from "./exec.ts";
import type { DirEntry, DirListing, FilePrefix, SearchMatch, SearchReport } from "./types.ts";

export const DEFAULT_LIST_LIMIT = 200;
export const MAX_LIST_LIMIT = 500;
export const DEFAULT_CHUNK_BYTES = 1024 * 1024;
export const MAX_CHUNK_BYTES = 4 * 1024 * 1024;
export const MAX_WRITE_BYTES = 4 * 1024 * 1024;
export const MAX_SEARCH_MATCHES = 500;
export const MAX_SEARCH_LINE = 512;

/** validate_browse_path: "" is the root; otherwise slash-separated normal
 * components — no absolute forms, backslashes, controls, ".", "..", doubles. */
export function validateBrowsePath(path: string): string {
	if (path === "") return "";
	if (path.includes("\0")) throw new WorkspaceError("bad_path", "path contains NUL");
	if (path.startsWith("/") || path.startsWith("\\")) throw new WorkspaceError("bad_path", "path must be relative");
	// eslint-disable-next-line no-control-regex
	if (/[\x00-\x1f\x7f]/.test(path) || path.includes("\\")) {
		throw new WorkspaceError("bad_path", "path contains illegal characters");
	}
	const parts = path.split("/");
	for (const part of parts) {
		if (part === "" || part === "." || part === "..") {
			throw new WorkspaceError("bad_path", "path must be relative and normal");
		}
	}
	if (path.includes("//") || path.endsWith("/")) throw new WorkspaceError("bad_path", "path must be relative and normal");
	return parts.join("/");
}

function clamp(v: number | undefined, dflt: number, max: number): number {
	if (v === undefined) return dflt;
	if (!Number.isInteger(v) || v <= 0) throw new WorkspaceError("bad_path", "limit/byte counts must be positive integers");
	return Math.min(v, max);
}

/** Walk components from root with lstat: refuse symlink components and
 * dev-boundary crossings (mount points). Returns absolute confined path. */
async function confinedPath(root: string, normalized: string, rootDev: number): Promise<string> {
	let current = root;
	if (normalized === "") return current;
	const parts = normalized.split("/");
	for (let i = 0; i < parts.length; i++) {
		current = join(current, parts[i]!);
		let st;
		try {
			st = await lstat(current);
		} catch {
			throw new WorkspaceError("not_found", `no such path: ${normalized}`);
		}
		const isLeaf = i === parts.length - 1;
		if (st.isSymbolicLink()) {
			throw new WorkspaceError("bad_path", `refusing to follow symlink: ${parts.slice(0, i + 1).join("/")}`);
		}
		if (st.dev !== rootDev && !isLeaf) {
			throw new WorkspaceError("bad_path", "refusing to cross a mount boundary");
		}
		if (!isLeaf && !st.isDirectory()) {
			throw new WorkspaceError("bad_path", `path component is not a directory: ${parts.slice(0, i + 1).join("/")}`);
		}
	}
	return current;
}

async function rootDev(cwd: string): Promise<number> {
	const st = await stat(cwd);
	return st.dev;
}

export async function listDir(cwd: string, path: string, afterName?: string, limit?: number): Promise<DirListing> {
	const normalized = validateBrowsePath(path);
	const lim = clamp(limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT);
	const dev = await rootDev(cwd);
	const dir = await confinedPath(cwd, normalized, dev);
	const st = await lstat(dir);
	if (!st.isDirectory()) throw new WorkspaceError("bad_path", `path is not a directory: ${normalized}`);
	if (st.dev !== dev) throw new WorkspaceError("bad_path", "refusing to list across a mount boundary");

	const names = await readdir(dir);
	const entries: DirEntry[] = [];
	for (const name of names) {
		if (name === "." || name === "..") continue;
		const meta = await lstat(join(dir, name));
		if (meta.dev !== dev) {
			entries.push({ name, kind: "other", mtimeMs: Math.round(meta.mtimeMs) });
			continue;
		}
		const kind: DirEntry["kind"] = meta.isFile() ? "file" : meta.isDirectory() ? "directory" : "other";
		const entry: DirEntry = { name, kind, mtimeMs: Math.round(meta.mtimeMs) };
		if (kind === "file") entry.size = meta.size;
		entries.push(entry);
	}
	entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
	let start = 0;
	if (afterName !== undefined) {
		const pos = entries.findIndex((e) => e.name === afterName);
		if (pos < 0) throw new WorkspaceError("bad_path", "after_name not found in directory");
		start = pos + 1;
	}
	const end = Math.min(start + lim, entries.length);
	const page = entries.slice(start, end);
	const listing: DirListing = { path: normalized, entries: page };
	if (end < entries.length && page.length > 0) listing.nextAfterName = page[page.length - 1]!.name;
	return listing;
}

export async function readFileRange(cwd: string, path: string, offset = 0, maxBytes?: number): Promise<FilePrefix> {
	const normalized = validateBrowsePath(path);
	if (normalized === "") throw new WorkspaceError("bad_path", "path must name a regular file");
	const cap = clamp(maxBytes, DEFAULT_CHUNK_BYTES, MAX_CHUNK_BYTES);
	if (!Number.isInteger(offset) || offset < 0) throw new WorkspaceError("bad_path", "offset must be a non-negative integer");
	const dev = await rootDev(cwd);
	const abs = await confinedPath(cwd, normalized, dev);
	const meta = await lstat(abs);
	if (meta.isSymbolicLink()) throw new WorkspaceError("bad_path", "refusing to follow symlink");
	if (!meta.isFile()) throw new WorkspaceError("bad_path", "path is not a regular file");
	if (meta.dev !== dev) throw new WorkspaceError("bad_path", "refusing to read across a mount boundary");
	const totalSize = meta.size;
	if (offset > totalSize) throw new WorkspaceError("bad_path", "offset is past end of file");
	const fh = await open(abs, constants.O_RDONLY | constants.O_NOFOLLOW);
	try {
		const fst = await fh.stat();
		if (!fst.isFile()) throw new WorkspaceError("bad_path", "path is not a regular file");
		if (fst.dev !== dev) throw new WorkspaceError("bad_path", "refusing to read across a mount boundary");
		const len = Math.min(cap, totalSize - offset);
		const buf = Buffer.alloc(len);
		await fh.read(buf, 0, len, offset);
		return {
			path: normalized,
			contentBase64: buf.toString("base64"),
			byteLen: len,
			totalSize,
			eof: offset + len >= totalSize,
			mtimeMs: Math.round(fst.mtimeMs),
		};
	} finally {
		await fh.close();
	}
}

/** Control-plane write (the bridge owns the workspace; model writes happen
 * in-session through host tools). Same confinement as read; leaf symlink refused. */
export async function writeFileConfined(cwd: string, path: string, contentBase64: string): Promise<{ path: string; bytes: number }> {
	const normalized = validateBrowsePath(path);
	if (normalized === "") throw new WorkspaceError("bad_path", "path must name a file");
	const bytes = Buffer.from(contentBase64, "base64");
	if (bytes.length > MAX_WRITE_BYTES) {
		throw new WorkspaceError("bad_path", `write exceeds ${MAX_WRITE_BYTES} bytes`);
	}
	const dev = await rootDev(cwd);
	const parentRel = dirname(normalized);
	const parent = parentRel === "." ? cwd : await confinedPath(cwd, parentRel, dev);
	const pst = await lstat(parent);
	if (!pst.isDirectory()) throw new WorkspaceError("bad_path", "parent is not a directory");
	if (pst.dev !== dev) throw new WorkspaceError("bad_path", "refusing to write across a mount boundary");
	const abs = join(parent, normalized.split("/").pop()!);
	try {
		const leaf = await lstat(abs);
		if (leaf.isSymbolicLink()) throw new WorkspaceError("bad_path", "refusing to write through a symlink");
		if (!leaf.isFile()) throw new WorkspaceError("bad_path", "path exists and is not a regular file");
		if (leaf.dev !== dev) throw new WorkspaceError("bad_path", "refusing to write across a mount boundary");
	} catch (err) {
		if (err instanceof WorkspaceError) throw err;
		/* not found → creating a new file is fine */
	}
	await writeFile(abs, bytes);
	return { path: normalized, bytes: bytes.length };
}

/** Bounded recursive grep across the cwd (pi-relay has no search RPC; this is
 * the bridge's own addition for the SPA search box — same confinement rules). */
export async function search(
	cwd: string,
	query: string,
	opts: { fixedString?: boolean; maxMatches?: number; include?: string } = {},
): Promise<SearchReport> {
	if (query === "") throw new WorkspaceError("bad_path", "query is required");
	const cap = Math.min(opts.maxMatches ?? MAX_SEARCH_MATCHES, MAX_SEARCH_MATCHES);
	const dev = await rootDev(cwd);
	const rootReal = await realpath(cwd);
	const args = ["-rEnI", "--binary-files=without-match", "--"];
	if (opts.fixedString) args.splice(0, 1, "-rFnI");
	// grep -r prints ./-prefixed paths when given "."; use cd via cwd option.
	const res = await run(
		"grep",
		[...args.slice(0, -2), ...(opts.include ? [`--include=${opts.include}`] : []), "--", query, "."],
		{ cwd, timeoutMs: 20_000, maxBuffer: 4 * 1024 * 1024, okCodes: [0, 1] },
	);
	const matches: SearchMatch[] = [];
	let truncated = false;
	for (const line of res.stdout.split("\n")) {
		if (line === "") continue;
		if (matches.length >= cap) {
			truncated = true;
			break;
		}
		const m = /^\.\/(.*?):(\d+):(.*)$/.exec(line) ?? /^(.*?):(\d+):(.*)$/.exec(line);
		if (!m) continue;
		const rel = m[1]!;
		if (rel.includes("\0") || rel.split("/").some((p) => p === "..")) continue;
		// defense in depth: resolve and require containment + same dev
		const abs = join(cwd, rel);
		try {
			const st = await lstat(abs);
			if (!st.isFile() || st.dev !== dev) continue;
			const real = await realpath(abs);
			if (!real.startsWith(rootReal + "/")) continue;
		} catch {
			continue;
		}
		matches.push({ path: rel, line: Number(m[2]), text: (m[3] ?? "").slice(0, MAX_SEARCH_LINE) });
	}
	return { matches, truncated };
}
