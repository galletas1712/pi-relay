// M8: createBridgeWorkspaceBackend maps the bridge workspace.* contract onto
// the WorkspaceBackend seam the DiffPanel consumes. Hermetic: fake RPC client.
import { describe, expect, it } from "vitest";
import { createBridgeWorkspaceBackend, createMockWorkspaceBackend } from "./workspace.ts";

interface Call {
	method: string;
	params: Record<string, unknown>;
}

function fakeClient(handlers: Record<string, (params: Record<string, unknown>) => unknown>) {
	const calls: Call[] = [];
	const client = {
		async request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
			calls.push({ method, params });
			const h = handlers[method];
			if (!h) throw new Error(`unexpected method ${method}`);
			return h(params) as T;
		},
	};
	return { client, calls };
}

describe("createBridgeWorkspaceBackend", () => {
	it("scopes every call by sessionId and pages listDir", async () => {
		const { client, calls } = fakeClient({
			"workspace.list_dir": (p) =>
				p.afterName === undefined
					? { entries: [{ name: "docs", kind: "directory" }, { name: "repo", kind: "directory" }], nextAfterName: "repo" }
					: { entries: [{ name: "zzz.txt", kind: "file" }, { name: "link", kind: "symlink" }], nextAfterName: null },
		});
		const backend = createBridgeWorkspaceBackend(client, "sess-1");
		const res = await backend.listDir({ path: "" });
		expect(res.entries.map((e) => e.path)).toEqual(["docs", "repo", "zzz.txt"]); // symlink filtered
		expect(calls.every((c) => c.params.sessionId === "sess-1")).toBe(true);
		expect(calls.length).toBe(2);
		expect(backend.scopeKey).toBe("sess-1");
	});

	it("readFile decodes base64 and flags truncation", async () => {
		const { client } = fakeClient({
			"workspace.read_file": () => ({
				path: "repo/a.ts",
				contentBase64: btoa("hello"),
				byteLength: 5,
				eof: false,
			}),
		});
		const backend = createBridgeWorkspaceBackend(client, "sess-1");
		const f = await backend.readFile({ path: "repo/a.ts" });
		expect(f.contents).toBe("hello");
		expect(f.truncated).toBe(true);
	});

	it("gitStatus flattens roots and marks staged", async () => {
		const { client, calls } = fakeClient({
			"workspace.git_status": () => ({
				roots: [
					{
						workspaceDir: "repo",
						entries: [
							{ path: "repo/a.ts", status: "modified", staged: null, worktree: "modified" },
							{ path: "repo/b.ts", status: "added", staged: "added", worktree: null },
						],
					},
					{ workspaceDir: "docs", entries: [{ path: "docs/n.md", status: "untracked", staged: null, worktree: "untracked" }] },
				],
			}),
		});
		const backend = createBridgeWorkspaceBackend(client, "sess-1");
		const res = await backend.gitStatus();
		expect(res.entries).toEqual([
			{ path: "repo/a.ts", status: "modified", staged: false },
			{ path: "repo/b.ts", status: "added", staged: true },
			{ path: "docs/n.md", status: "untracked", staged: false },
		]);
		expect(calls[0].params.against).toBe("working_tree");
	});

	it("gitDiff passes the patch through verbatim (parsePatchFiles input)", async () => {
		const { client } = fakeClient({
			"workspace.git_diff": () => ({ diff: "diff --git a/x b/x\n", truncated: false }),
		});
		const backend = createBridgeWorkspaceBackend(client, "sess-1");
		expect((await backend.gitDiff({})).diff).toBe("diff --git a/x b/x\n");
	});

	it("mock backend stays available for tests (M6 seam preserved)", async () => {
		const mock = createMockWorkspaceBackend();
		const d = await mock.gitDiff({});
		expect(d.diff).toContain("diff --git");
		const s = await mock.gitStatus();
		expect(s.entries.length).toBeGreaterThan(0);
	});
});
