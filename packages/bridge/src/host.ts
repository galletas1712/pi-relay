// HostProcess: one `pi --mode rpc` child per session. Speaks pi's stdio JSONL
// protocol (commands in, responses + AgentSessionEvents out). The supervisor
// owns lifecycle policy; this class owns pipes, framing, and ack correlation.
import { spawn, type ChildProcess } from "node:child_process";
import { createWriteStream, mkdirSync, type WriteStream } from "node:fs";
import path from "node:path";
import { config, logsDir, sessionsDir } from "./config.ts";

export interface HostExit {
	code: number | null;
	signal: NodeJS.Signals | null;
}

interface Pending {
	resolve: (obj: Record<string, unknown>) => void;
	reject: (err: Error) => void;
	timer: NodeJS.Timeout;
	kind: string;
}

export class HostProcess {
	readonly sessionId: string;
	readonly generation: number;
	private child: ChildProcess | null = null;
	private buf = "";
	private pending = new Map<string, Pending>();
	private reqCounter = 0;
	private logStream: WriteStream | null = null;
	private exitedResolve: ((e: HostExit) => void) | null = null;
	private exitInfo: HostExit | null = null;
	/** line handler for non-response objects (events) */
	onEvent: (obj: Record<string, unknown>) => void = () => {};
	/** called exactly once when the child exits */
	onExit: (exit: HostExit) => void = () => {};

	constructor(sessionId: string, generation: number) {
		this.sessionId = sessionId;
		this.generation = generation;
	}

	get pid(): number | null {
		return this.child?.pid ?? null;
	}

	get alive(): boolean {
		return this.child !== null && this.exitInfo === null;
	}

	/** Spawn the pi host. `sessionFile` resumes a transcript; `sessionIdForNew`
	 * pins the id for sessions created before their first flush. */
	async start(opts: { cwd: string; sessionFile?: string | null; extraEnv?: Record<string, string> }): Promise<void> {
		mkdirSync(logsDir(), { recursive: true });
		mkdirSync(sessionsDir(), { recursive: true });
		const args = [
			config.piCli,
			"--mode",
			"rpc",
			"--tools",
			"ipython",
			"--session-dir",
			sessionsDir(),
		];
		if (opts.sessionFile) args.push("--session", opts.sessionFile);
		else args.push("--session-id", this.sessionId);
		const env: NodeJS.ProcessEnv = {
			...process.env,
			PI_CODING_AGENT_DIR: config.agentDir,
			PRIME_RLM_KERNEL_VENV: config.kernelVenv,
			...(opts.extraEnv ?? {}),
		};
		this.logStream = createWriteStream(path.join(logsDir(), `${this.sessionId}.log`), { flags: "a" });
		this.logStream.write(`\n# === host spawn gen=${this.generation} pid pending args=${JSON.stringify(args.slice(1))} ===\n`);
		const child = spawn(process.execPath, args, {
			env,
			cwd: opts.cwd,
			stdio: ["pipe", "pipe", "pipe"],
		});
		this.child = child;
		this.exitInfo = null;
		this.logStream.write(`# pid=${child.pid}\n`);
		child.stderr!.on("data", (chunk: Buffer) => {
			this.logStream?.write(chunk);
		});
		child.stdout!.on("data", (chunk: Buffer) => {
			this.buf += chunk.toString("utf8");
			let idx: number;
			while ((idx = this.buf.indexOf("\n")) >= 0) {
				let line = this.buf.slice(0, idx);
				this.buf = this.buf.slice(idx + 1);
				if (line.endsWith("\r")) line = line.slice(0, -1);
				if (line.trim()) this.handleLine(line);
			}
		});
		const exited = new Promise<HostExit>((res) => {
			this.exitedResolve = res;
		});
		child.on("exit", (code, signal) => {
			this.exitInfo = { code, signal };
			this.logStream?.write(`# exit code=${code} signal=${signal}\n`);
			this.logStream?.end();
			// fail all pending rpcs (transport-uncertain from the caller's view)
			for (const [, p] of this.pending) {
				clearTimeout(p.timer);
				p.reject(new Error(`host exited (code=${code} signal=${signal})`));
			}
			this.pending.clear();
			this.child = null;
			this.exitedResolve?.(this.exitInfo);
			this.onExit(this.exitInfo);
		});
		child.on("error", (err) => {
			this.logStream?.write(`# spawn error ${err.message}\n`);
		});
		// keep the exit promise from leaking unhandled
		void exited.catch(() => {});
	}

	private handleLine(line: string): void {
		let obj: Record<string, unknown>;
		try {
			obj = JSON.parse(line);
		} catch {
			this.logStream?.write(`# unparsable stdout line: ${line.slice(0, 200)}\n`);
			return;
		}
		if (obj.type === "response" && typeof obj.id === "string" && this.pending.has(obj.id)) {
			const p = this.pending.get(obj.id)!;
			this.pending.delete(obj.id);
			clearTimeout(p.timer);
			p.resolve(obj);
			return;
		}
		this.onEvent(obj);
	}

	/** Send an rpc command; resolves with the response object (check success). */
	send(command: Record<string, unknown>, kind: string, timeoutMs = config.ackTimeoutMs): Promise<Record<string, unknown>> {
		if (!this.alive) return Promise.reject(new Error("host not running"));
		const id = `b${this.generation}-${++this.reqCounter}`;
		return new Promise((resolve, reject) => {
			const timer = setTimeout(() => {
				this.pending.delete(id);
				reject(new Error(`ack timeout after ${timeoutMs}ms`));
			}, timeoutMs);
			this.pending.set(id, { resolve, reject, timer, kind });
			try {
				this.child!.stdin!.write(JSON.stringify({ ...command, id }) + "\n");
			} catch (err) {
				clearTimeout(timer);
				this.pending.delete(id);
				reject(err instanceof Error ? err : new Error(String(err)));
			}
		});
	}

	/** Ask the host to die gracefully, then SIGKILL after grace. */
	async stop(graceMs = 3000): Promise<void> {
		const child = this.child;
		if (!child) return;
		const exited = new Promise<void>((res) => {
			const t = setTimeout(res, graceMs);
			child.once("exit", () => {
				clearTimeout(t);
				res();
			});
		});
		try {
			child.stdin!.end();
		} catch {
			/* already closed */
		}
		await exited;
		if (this.alive) {
			try {
				child.kill("SIGKILL");
			} catch {
				/* gone */
			}
		}
	}
}
