import type { AgentApi } from "./agentApi.ts";
import { queryKeys } from "./queryKeys.ts";
import type { WorkspaceDirListing } from "./types.ts";
import {
	workspaceFileCache,
	type CachedWorkspaceFile,
} from "./workspaceFileCache.ts";

/** Per-chunk raw size for ranged downloads (fits under the 8 MiB WS frame). */
export const WORKSPACE_FILE_CHUNK_BYTES = 1024 * 1024;

export function workspaceDirQueryKey(sessionId: string, path: string, afterName: string | null = null) {
	return queryKeys.workspaceDir(sessionId, path, afterName);
}

export function workspaceFileQueryKey(sessionId: string, path: string) {
	return queryKeys.workspaceFile(sessionId, path);
}

export async function fetchWorkspaceDir(
	api: AgentApi,
	sessionId: string,
	path: string,
	afterName: string | null = null,
	limit = 200,
): Promise<WorkspaceDirListing> {
	return api.listWorkspaceDir({
		sessionId,
		path,
		afterName,
		limit,
	});
}

/** Merge a newly fetched page into the accumulated listing for `path`. */
export function mergeWorkspaceDirPage(
	previous: WorkspaceDirListing | undefined,
	page: WorkspaceDirListing,
): WorkspaceDirListing {
	if (!previous || previous.path !== page.path) {
		return {
			path: page.path,
			entries: [...page.entries],
			next_after_name: page.next_after_name ?? null,
		};
	}
	return {
		path: page.path,
		entries: [...previous.entries, ...page.entries],
		next_after_name: page.next_after_name ?? null,
	};
}

export async function downloadWorkspaceFile(
	api: AgentApi,
	sessionId: string,
	path: string,
	chunkBytes = WORKSPACE_FILE_CHUNK_BYTES,
): Promise<CachedWorkspaceFile> {
	const chunks: Uint8Array[] = [];
	let offset = 0;
	let totalSize: number | null = null;
	let mtimeMs: number | null = null;

	for (;;) {
		const part = await api.readWorkspaceFile({
			sessionId,
			path,
			offset,
			maxBytes: chunkBytes,
		});
		if (totalSize == null) {
			totalSize = part.total_size;
			mtimeMs = part.mtime_ms ?? null;
		} else if (part.total_size !== totalSize) {
			throw new Error("file size changed while downloading");
		}
		const bytes = decodeBase64(part.content_base64);
		if (bytes.byteLength !== part.byte_len) {
			throw new Error("chunk byte_len does not match payload");
		}
		chunks.push(bytes);
		offset += part.byte_len;
		if (part.eof) break;
		if (part.byte_len === 0) {
			throw new Error("read returned empty non-eof chunk");
		}
		if (offset > part.total_size) {
			throw new Error("download offset passed total_size");
		}
	}

	const bytes = concatBytes(chunks);
	if (totalSize != null && bytes.byteLength !== totalSize) {
		throw new Error(
			`downloaded ${bytes.byteLength} bytes but total_size is ${totalSize}`,
		);
	}

	return {
		sessionId,
		path,
		bytes,
		totalSize: totalSize ?? bytes.byteLength,
		mtimeMs,
	};
}

/** Load from cache or download; pins the result for the open pane. */
export async function loadCachedWorkspaceFile(
	api: AgentApi,
	sessionId: string,
	path: string,
): Promise<CachedWorkspaceFile> {
	const hit = workspaceFileCache.get(sessionId, path);
	if (hit) {
		workspaceFileCache.pin(sessionId, path);
		return hit;
	}
	const file = await downloadWorkspaceFile(api, sessionId, path);
	workspaceFileCache.set(file, true);
	return file;
}

export function invalidateCachedWorkspaceFile(sessionId: string, path: string): void {
	workspaceFileCache.invalidate(sessionId, path);
}

/** Decode a base64 payload once for viewers. */
export function decodeBase64(contentBase64: string): Uint8Array {
	const binary = atob(contentBase64);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i += 1) {
		bytes[i] = binary.charCodeAt(i);
	}
	return bytes;
}

export function concatBytes(chunks: Uint8Array[]): Uint8Array {
	const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
	const out = new Uint8Array(total);
	let offset = 0;
	for (const chunk of chunks) {
		out.set(chunk, offset);
		offset += chunk.byteLength;
	}
	return out;
}

export function bytesToUtf8Prefix(bytes: Uint8Array): { text: string; binary: boolean } {
	if (bytes.includes(0)) return { text: "", binary: true };
	try {
		const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
		return { text, binary: false };
	} catch {
		// Tolerate one incomplete trailing UTF-8 sequence on truncated prefixes.
		for (let trim = 1; trim <= 3 && trim < bytes.length; trim += 1) {
			try {
				const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes.slice(0, -trim));
				return { text, binary: false };
			} catch {
				// keep trying
			}
		}
		return { text: "", binary: true };
	}
}
