// Workspace backend seam for the diff/file views.
//
// M8: the real bridge workspace.* contract is implemented; BridgeApp builds
// createBridgeWorkspaceBackend(client, selectedSessionId) and rebuilds it when
// the selection changes. The M6 LOCAL MOCK stays exported for tests.
//
// Method shapes mirror the bridge workspace.* namespace
// (workspace.list_dir / read_file / git_status / git_diff).
export interface WorkspaceEntry {
	path: string;
	kind: "file" | "directory";
}

export interface WorkspaceFile {
	path: string;
	contents: string;
	byteLength: number;
	truncated: boolean;
}

export interface GitStatusEntry {
	path: string;
	status: "added" | "modified" | "deleted" | "renamed" | "untracked";
	staged: boolean;
}

export interface WorkspaceBackend {
	/** Cache-scope key for react-query (session id for the real backend) so
	 * switching sessions never renders another session's cached workspace. */
	scopeKey?: string;
	listDir(params: { path?: string }): Promise<{ entries: WorkspaceEntry[] }>;
	readFile(params: { path: string }): Promise<WorkspaceFile>;
	gitStatus(): Promise<{ entries: GitStatusEntry[] }>;
	/** unified patch text — exactly what parsePatchFiles consumes */
	gitDiff(params: { against?: string }): Promise<{ diff: string }>;
}

// ---- real bridge backend (M8) --------------------------------------------------

/** Minimal surface of the bridge client the workspace backend needs
 * (structurally compatible with BridgeClient.request). */
export interface WorkspaceRpcClient {
	request<T>(method: string, params?: Record<string, unknown>): Promise<T>;
}

interface BridgeListDirResult {
	entries: Array<{ name: string; kind: "file" | "directory" | "symlink" | "other" }>;
	nextAfterName: string | null;
}

interface BridgeReadResult {
	path: string;
	contentBase64: string;
	byteLength: number;
	eof: boolean;
}

interface BridgeStatusResult {
	roots: Array<{
		workspaceDir: string;
		entries: Array<{ path: string; status: GitStatusEntry["status"]; staged: string | null; worktree: string | null }>;
	}>;
}

/** Real WorkspaceBackend over the bridge workspace.* contract (M8). Scoped to
 * one session; BridgeApp rebuilds it when the selection changes. */
export function createBridgeWorkspaceBackend(client: WorkspaceRpcClient, sessionId: string): WorkspaceBackend {
	return {
		scopeKey: sessionId,
		async listDir({ path } = {}) {
			// collect all pages (200-entry pages; workspace trees are small)
			const entries: WorkspaceEntry[] = [];
			let afterName: string | undefined;
			const base = path && path !== "" ? path : undefined;
			for (;;) {
				const page = await client.request<BridgeListDirResult>("workspace.list_dir", {
					sessionId,
					path: base ?? "",
					...(afterName ? { afterName } : {}),
					limit: 500,
				});
				for (const e of page.entries) {
					if (e.kind !== "file" && e.kind !== "directory") continue; // symlinks/others hidden from the tree
					entries.push({ path: base ? `${base}/${e.name}` : e.name, kind: e.kind });
				}
				if (!page.nextAfterName) break;
				afterName = page.nextAfterName;
			}
			return { entries };
		},
		async readFile({ path }) {
			const res = await client.request<BridgeReadResult>("workspace.read_file", { sessionId, path, maxBytes: 4 * 1024 * 1024 });
			return {
				path: res.path,
				contents: new TextDecoder().decode(Uint8Array.from(atob(res.contentBase64), (ch) => ch.charCodeAt(0))),
				byteLength: res.byteLength,
				truncated: !res.eof,
			};
		},
		async gitStatus() {
			const res = await client.request<BridgeStatusResult>("workspace.git_status", { sessionId, against: "working_tree" });
			const entries: GitStatusEntry[] = [];
			for (const root of res.roots) {
				for (const e of root.entries) {
					entries.push({ path: e.path, status: e.status, staged: e.staged !== null });
				}
			}
			return { entries };
		},
		async gitDiff({ against } = {}) {
			const res = await client.request<{ diff: string; truncated: boolean }>("workspace.git_diff", {
				sessionId,
				...(against ? { against } : {}),
			});
			return { diff: res.diff };
		},
	};
}

export const MOCK_FIXTURE_DIFF = `diff --git a/src/agent/planner.ts b/src/agent/planner.ts
index 3f9a1c2..8b7d4e1 100644
--- a/src/agent/planner.ts
+++ b/src/agent/planner.ts
@@ -12,7 +12,7 @@ export interface PlanStep {
 export function planTurn(goal: string): PlanStep[] {
 	const steps: PlanStep[] = [];
-	steps.push({ kind: "think", detail: goal });
+	steps.push({ kind: "think", detail: goal.trim() });
 	steps.push({ kind: "act" });
 	return steps;
 }
@@ -24,6 +24,11 @@ export function planTurn(goal: string): PlanStep[] {
 	return steps;
 }

+export function summarizePlan(steps: PlanStep[]): string {
+	return steps.map((step) => step.kind).join(" -> ");
+}
+
 export function isNoOp(plan: PlanStep[]): boolean {
 	return plan.every((step) => step.kind === "think");
 }
diff --git a/src/agent/tools/ipython.ts b/src/agent/tools/ipython.ts
deleted file mode 100644
index 7c1e9b0..0000000
--- a/src/agent/tools/ipython.ts
+++ /dev/null
@@ -1,8 +0,0 @@
-// Legacy synchronous ipython shim — replaced by the bridge kernel proxy.
-export function runCell(source: string): string {
-	return source;
-}
-
-export function resetKernel(): void {
-	// no-op
-}
diff --git a/docs/bridge-contract.md b/docs/bridge-contract.md
new file mode 100644
index 0000000..e61f0b2
--- /dev/null
+++ b/docs/bridge-contract.md
@@ -0,0 +1,6 @@
+# Bridge contract notes
+
+The bridge owns session supervision; the workspace RPCs land in M7.
+Until then the diff views render fixture data from the local mock.
+
+See packages/bridge/README.md for the event taxonomy.
`;

const MOCK_FILES = new Map<string, string>([
	[
		"src/agent/planner.ts",
		`export interface PlanStep {
	kind: "think" | "act";
	detail?: string;
}

export function planTurn(goal: string): PlanStep[] {
	const steps: PlanStep[] = [];
	steps.push({ kind: "think", detail: goal.trim() });
	steps.push({ kind: "act" });
	return steps;
}

export function summarizePlan(steps: PlanStep[]): string {
	return steps.map((step) => step.kind).join(" -> ");
}

export function isNoOp(plan: PlanStep[]): boolean {
	return plan.every((step) => step.kind === "think");
}
`,
	],
	["docs/bridge-contract.md", `# Bridge contract notes

The bridge owns session supervision; the workspace RPCs land in M7.
Until then the diff views render fixture data from the local mock.

See packages/bridge/README.md for the event taxonomy.
`],
]);

/** Fixture-backed WorkspaceBackend. Deterministic, async (microtask) so React
 * Query behaves exactly as it will against the real bridge adapter. */
export function createMockWorkspaceBackend(): WorkspaceBackend {
	return {
		async listDir({ path = "" } = {}) {
			const entries: WorkspaceEntry[] = [];
			for (const filePath of MOCK_FILES.keys()) {
				if (path && !filePath.startsWith(path)) continue;
				entries.push({ path: filePath, kind: "file" });
			}
			entries.push({ path: "src/agent/tools", kind: "directory" });
			return { entries };
		},
		async readFile({ path }) {
			const contents = MOCK_FILES.get(path);
			if (contents === undefined) {
				throw new Error(`workspace.readFile: no such fixture file: ${path}`);
			}
			return { path, contents, byteLength: contents.length, truncated: false };
		},
		async gitStatus() {
			return {
				entries: [
					{ path: "src/agent/planner.ts", status: "modified", staged: false },
					{ path: "src/agent/tools/ipython.ts", status: "deleted", staged: false },
					{ path: "docs/bridge-contract.md", status: "added", staged: true },
				] satisfies GitStatusEntry[],
			};
		},
		async gitDiff() {
			return { diff: MOCK_FIXTURE_DIFF };
		},
	};
}
