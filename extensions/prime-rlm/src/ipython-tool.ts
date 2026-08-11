// M1 port of prime-agent's ipython tool (core/tools/ipython.ts); M3 added the
// snapshot/restore wiring (per-session kernel-state.{dill,json} revived before
// bootstrap). Remaining differences: per-session provisioner registry (this
// extension instance is shared across parent + in-process child sessions, so
// kernels are keyed by sessionId), no busy-kernel UI prompt (rpc/print mode:
// kill+restart is never right, so busy kernels surface an error), no
// attachment/diff display plumbing.
import { existsSync } from "node:fs";
import path from "node:path";
import type { ExtensionContext, ToolDefinition } from "@earendil-works/pi-coding-agent";
import type { ImageContent, TextContent } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import type { KernelBootstrapContribution } from "./api.ts";
import type {
	ExecuteResult,
	HostRequestHandlers,
	KernelAttachment,
	KernelCellMeta,
	KernelManagerOptions,
	KernelSentAgentMessage,
} from "./kernel.ts";
import { KernelManager } from "./kernel.ts";

// M4: mirror of prime-agent utils/mime.ts (image attachment allowlist).
const IMAGE_MIME_TYPES = new Set(["image/jpeg", "image/png", "image/gif", "image/webp"]);

/** Turn kernel image attachments into `ImageContent` blocks; non-image types are dropped. */
export function imageBlocksFromAttachments(attachments: readonly KernelAttachment[] | undefined): ImageContent[] {
	if (!attachments) return [];
	return attachments
		.filter((a) => IMAGE_MIME_TYPES.has(a.mimeType))
		.map((a) => ({ type: "image" as const, data: a.data, mimeType: a.mimeType }));
}
import { ensureKernelPython } from "./provision.ts";

const RLM_BOOTSTRAP_CODE = `
import asyncio
import os as _prime_rlm_os

_prime_rlm_os.environ["NO_COLOR"] = "1"
get_ipython().colors = "nocolor"

try:
    import nest_asyncio as _prime_rlm_nest_asyncio
    _prime_rlm_nest_asyncio.apply()
except Exception:
    pass

try:
    import prime_rlm_runtime as _prime_rlm_runtime_module
    rlm = _prime_rlm_runtime_module.rlm
except Exception as _prime_rlm_import_error:
    _PRIME_RLM_IMPORT_ERROR = str(_prime_rlm_import_error)

    class _PrimeRlmMissingRuntime:
        def _raise_missing(self):
            raise RuntimeError(
                "prime-rlm-runtime is not installed in this IPython kernel. "
                "Delete the kernel venv so prime-rlm can rebuild it, or set "
                "PRIME_RLM_KERNEL_PYTHON to a kernel environment with prime-rlm-runtime installed. "
                f"Import error: {_PRIME_RLM_IMPORT_ERROR}"
            )

        async def run(self, prompt, **kwargs):
            self._raise_missing()

        async def list_subagents(self):
            self._raise_missing()

        async def delete_subagent(self, target):
            self._raise_missing()

        async def snapshot_save(self):
            self._raise_missing()

        async def snapshot_restore(self):
            self._raise_missing()

        async def __call__(self, prompt, **kwargs):
            return await self.run(prompt, **kwargs)

    rlm = _PrimeRlmMissingRuntime()
`.trim();

const ipythonSchema = Type.Object({
	code: Type.String({
		description:
			"Python scratchpad code or `%%bash` shell cells to execute in the agent kernel. Use the target project's own environment for project imports, tests, scripts, CLIs, and dependency checks instead of direct kernel imports.",
	}),
});

export interface IpythonToolDetails {
	durationMs?: number;
	status?: "ok" | "error" | "aborted" | "starting";
	errorEname?: string;
	stdout?: string;
	stderr?: string;
	result?: string;
	diffs?: import("./kernel.ts").KernelDiffDisplay[];
	attachments?: KernelAttachment[];
	sentAgentMessages?: KernelSentAgentMessage[];
	error?: {
		ename: string;
		evalue: string;
		traceback: string[];
	};
}

function createAbortError(): Error {
	return new Error("IPython execution aborted");
}

function raceWithAbort<T>(promise: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
	if (!signal) {
		return promise;
	}
	if (signal.aborted) {
		return Promise.reject(createAbortError());
	}
	return new Promise<T>((resolve, reject) => {
		let settled = false;
		const cleanup = () => signal.removeEventListener("abort", abort);
		const abort = () => {
			if (settled) return;
			settled = true;
			cleanup();
			reject(createAbortError());
		};
		signal.addEventListener("abort", abort, { once: true });
		promise.then(
			(value) => {
				if (settled) return;
				settled = true;
				cleanup();
				resolve(value);
			},
			(error: unknown) => {
				if (settled) return;
				settled = true;
				cleanup();
				reject(error);
			},
		);
	});
}

/**
 * Owns the lazy create+start+runtime-bootstrap of one session's IPython kernel.
 * Concurrent ensure() calls await the same in-flight startup; a failed startup
 * clears the memo so the next call retries fresh.
 */
export class KernelProvisioner {
	private managerPromise?: Promise<KernelManager>;
	private startedManager?: KernelManager;
	private readonly disposeController = new AbortController();

	/** Warnings from sibling-extension bootstrap contributions (M2 seam). */
	private bootstrapWarnings: string[] = [];
	private bootstrapWarningsPending = false;

	constructor(
		private readonly options: {
			cwd: string;
			agentDir: string;
			sessionId: string;
			hostHandlers: HostRequestHandlers;
			/** M2 seam: sibling-extension kernel contributions, resolved at start. */
			contributions?: () => KernelBootstrapContribution[];
			/** M3: directory holding kernel-state.{dill,json}; enables namespace snapshots. */
			snapshotDir?: string;
			/** M4 (O2): receipts arriving after their owning cell's tool result shipped. */
			onLateSentAgentMessage?: (message: KernelSentAgentMessage) => void;
			/** M9: repl-console cell lifecycle/output hook (repl pane). */
			onCellEvent?: KernelManagerOptions["onCellEvent"];
		},
	) {}

	/** Drain bootstrap warnings once so the first tool result after kernel start
	 * can surface them to the model/user. */
	consumeBootstrapWarnings(): string[] {
		if (!this.bootstrapWarningsPending) return [];
		this.bootstrapWarningsPending = false;
		return [...this.bootstrapWarnings];
	}

	get manager(): KernelManager | undefined {
		return this.startedManager;
	}

	get hasRunningKernel(): boolean {
		return this.startedManager?.isRunning ?? false;
	}

	async dispose(): Promise<void> {
		this.disposeController.abort();
		const pending = this.managerPromise;
		this.managerPromise = undefined;
		this.startedManager = undefined;
		if (!pending) return;
		try {
			const m = await pending;
			await m.dispose();
		} catch {
			// a failed startup already cleaned up after itself
		}
	}

	ensure(signal?: AbortSignal): Promise<KernelManager> {
		if (signal?.aborted) {
			return Promise.reject(createAbortError());
		}
		if (!this.managerPromise) {
			const startup = this.startKernel(signal);
			this.managerPromise = startup;
			startup.then(
				(m) => {
					if (this.managerPromise === startup) {
						this.startedManager = m;
					}
				},
				() => {
					if (this.managerPromise === startup) {
						this.managerPromise = undefined;
					}
				},
			);
		}
		return raceWithAbort(this.managerPromise, signal);
	}

	private async startKernel(signal?: AbortSignal): Promise<KernelManager> {
		if (this.disposeController.signal.aborted) {
			throw new Error("Kernel provisioner disposed before start");
		}
		const python = await ensureKernelPython({ agentDir: this.options.agentDir });
		if (this.disposeController.signal.aborted || signal?.aborted) {
			throw createAbortError();
		}
		// M2 seam: sibling-extension contributions (env at spawn, python after
		// core bootstrap). Resolved here — lazily — so registration order and
		// per-session info are correct regardless of extension load order.
		const contributions = this.options.contributions?.() ?? [];
		const env: Record<string, string> = {};
		for (const contribution of contributions) {
			Object.assign(env, contribution.env ?? {});
		}
		// M3: per-session kernel-state snapshot (dill). Debounce override keeps
		// the demo harness fast; PA's default is 1500 ms.
		const snapshotDir = this.options.snapshotDir;
		const debounceOverride = Number(process.env.PRIME_RLM_SNAPSHOT_DEBOUNCE_MS ?? "");
		const m = new KernelManager({
			python,
			cwd: this.options.cwd,
			env: Object.keys(env).length > 0 ? env : undefined,
			sessionId: this.options.sessionId,
			hostHandlers: this.options.hostHandlers,
			onLateSentAgentMessage: this.options.onLateSentAgentMessage,
			onCellEvent: this.options.onCellEvent,
			snapshot: snapshotDir
				? {
						path: path.join(snapshotDir, "kernel-state.dill"),
						manifestPath: path.join(snapshotDir, "kernel-state.json"),
						debounceMs: Number.isFinite(debounceOverride) && debounceOverride > 0 ? debounceOverride : undefined,
					}
				: undefined,
		});
		try {
			await m.start({ signal });
			// Revive the previous namespace BEFORE the runtime bootstrap so the
			// bootstrap's fresh handles (rlm, asyncio patches) override anything
			// restored — the same ordering PA uses.
			if (snapshotDir && existsSync(path.join(snapshotDir, "kernel-state.dill"))) {
				const restore = await m.restoreState();
				if (restore && (restore.restored.length > 0 || restore.failed.length > 0)) {
					this.bootstrapWarnings.push(
						`kernel namespace restored from snapshot: ${restore.restored.length} variable(s)` +
							(restore.failed.length > 0
								? `; ${restore.failed.length} could not be revived: ${restore.failed.map((f) => `${f.name} (${f.reason})`).join(", ")}`
								: ""),
					);
				}
			}
			const bootstrap = await m.execute(RLM_BOOTSTRAP_CODE, { signal });
			if (bootstrap.status !== "ok") {
				const details = [bootstrap.stderr, bootstrap.error?.traceback.join("\n")].filter(Boolean).join("\n");
				throw new Error(`Failed to initialize rlm runtime in the IPython kernel:\n${details}`);
			}
			// Sibling bootstrap cells run individually: a failure in one extension
			// must not kill the kernel or block the others; each failure is
			// recorded and surfaced as a warning on the next ipython tool result.
			for (const contribution of contributions) {
				if (!contribution.python) continue;
				const r = await m.execute(contribution.python, { signal, internal: true });
				if (r.status !== "ok") {
					const details = [r.stderr, r.error?.traceback.join("\n")].filter(Boolean).join("\n");
					this.bootstrapWarnings.push(
						`A kernel bootstrap contribution failed (kernel remains usable):\n${details}`,
					);
				}
			}
			if (this.bootstrapWarnings.length > 0) this.bootstrapWarningsPending = true;
		} catch (error) {
			void m.dispose();
			throw error;
		}
		return m;
	}
}

/** M9: repl-console reporter seam — announces model cells so the console
 * shows EVERYTHING the kernel runs (provenance "model", tool_call_id link). */
export interface ReplReporter {
	queued(toolCallId: string, code: string, ctx: ExtensionContext): KernelCellMeta;
	executeError(meta: KernelCellMeta, error: unknown): void;
	settled(meta: KernelCellMeta, result: ExecuteResult): void;
}

export function createIpythonToolDefinition(
	getProvisioner: (ctx: ExtensionContext) => KernelProvisioner,
	/** M4 (O2): host hook for in-execution agent-message receipts (appendEntry). */
	onSentAgentMessage?: (toolCallId: string, message: KernelSentAgentMessage) => void,
	/** M9: optional repl-console reporter (omitted in non-console contexts). */
	repl?: ReplReporter,
): ToolDefinition<typeof ipythonSchema, IpythonToolDetails> {
	return {
		name: "ipython",
		label: "ipython",
		description:
			"Execute Python scratchpad code and `%%bash` shell cells in a persistent IPython kernel. Variables, imports, and loaded data persist across calls, and are revived on a best-effort basis when a session is resumed (objects that cannot be serialized are dropped and reported). Project imports, tests, scripts, CLIs, and dependency checks should run through the target project's own environment.",
		promptSnippet: "ipython - persistent agent notebook for Python scratchpad code and %%bash orchestration",
		// The kernel is single-threaded — pi must not run two ipython calls in parallel within a batch.
		executionMode: "sequential",
		parameters: ipythonSchema,
		execute: async (toolCallId, params, signal, onUpdate, ctx) => {
			const provisioner = getProvisioner(ctx);
			const reportStartupProgress = (message: string) => {
				onUpdate?.({
					content: [{ type: "text", text: message }],
					details: { status: "starting" as const },
				});
			};

			// M9: announce the model cell to the repl console BEFORE ensure() so a
			// cold kernel start still shows a queued cell. Undefined when no
			// reporter is wired (tests, non-console embedders).
			const cellMeta = repl?.queued(toolCallId, params.code, ctx);

			let manager: KernelManager;
			try {
				reportStartupProgress("Starting IPython kernel...");
				manager = await provisioner.ensure(signal);
			} catch (error) {
				if (cellMeta) repl?.executeError(cellMeta, error);
				const message = error instanceof Error ? error.message : String(error);
				return {
					content: [{ type: "text", text: `Failed to start the IPython kernel: ${message}` }],
					details: { status: "error" as const },
					isError: true,
				};
			}

			let r: ExecuteResult;
			try {
				r = await manager.execute(params.code, {
					signal,
					...(cellMeta ? { cell: cellMeta } : {}),
					onStream: (chunk) => {
						onUpdate?.({
							content: [{ type: "text", text: chunk }],
							details: { status: "ok" as const },
						});
					},
				});
			} catch (error) {
				if (cellMeta) repl?.executeError(cellMeta, error);
				throw error;
			}
			if (cellMeta) repl?.settled(cellMeta, r);

			let text = r.stdout;
			if (r.stderr) text += (text ? "\n" : "") + r.stderr;
			if (r.result) text += (text ? "\n" : "") + r.result;
			const bootstrapWarnings = provisioner.consumeBootstrapWarnings();
			if (bootstrapWarnings.length > 0) {
				text = `[prime-rlm bootstrap warnings]\n${bootstrapWarnings.join("\n")}\n\n${text}`;
			}
			if (r.status === "error" && r.error) {
				text += (text ? "\n" : "") + r.error.traceback.join("\n");
			}

			// M4 (A1): image attachments become ImageContent blocks so the model
			// SEES them; M4 (O2): receipts are recorded as session entries.
			const imageBlocks = imageBlocksFromAttachments(r.attachments);
			const content: (TextContent | ImageContent)[] = [{ type: "text", text: text || "" }, ...imageBlocks];
			if (r.sentAgentMessages) {
				for (const message of r.sentAgentMessages) {
					onSentAgentMessage?.(toolCallId, message);
				}
			}

			return {
				content,
				details: {
					durationMs: r.durationMs,
					status: r.status,
					errorEname: r.error?.ename,
					stdout: r.stdout,
					stderr: r.stderr,
					result: r.result,
					diffs: r.diffs,
					attachments: r.attachments,
					sentAgentMessages: r.sentAgentMessages,
					error: r.error,
				},
				isError: r.status === "error" || r.status === "aborted",
			};
		},
	};
}
