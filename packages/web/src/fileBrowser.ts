import type { AgentApi } from "./agentApi.ts";
import { queryKeys } from "./queryKeys.ts";
import type { WorkspaceDirListing, WorkspaceFilePrefix } from "./types.ts";

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

export async function fetchWorkspaceFile(
	api: AgentApi,
	sessionId: string,
	path: string,
	maxBytes = 262144,
): Promise<WorkspaceFilePrefix> {
	return api.readWorkspaceFile({
		sessionId,
		path,
		maxBytes,
	});
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
