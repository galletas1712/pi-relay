// Bounded child_process exec: no shell, captured output, timeout kills the child.
import { execFile } from "node:child_process";

export interface RunResult {
	stdout: string;
	stderr: string;
	code: number;
}

export class RunFailure extends Error {
	readonly result: RunResult;
	constructor(cmd: string, args: readonly string[], result: RunResult) {
		super(`${cmd} ${args.join(" ")} failed (code=${result.code}): ${result.stderr.trim()}`.slice(0, 2000));
		this.result = result;
	}
}

export function run(
	cmd: string,
	args: readonly string[],
	opts: { cwd?: string; env?: NodeJS.ProcessEnv; timeoutMs?: number; maxBuffer?: number; okCodes?: number[] } = {},
): Promise<RunResult> {
	const okCodes = opts.okCodes ?? [0];
	return new Promise((resolve, reject) => {
		execFile(
			cmd,
			[...args],
			{
				cwd: opts.cwd,
				env: opts.env,
				timeout: opts.timeoutMs ?? 120_000,
				maxBuffer: opts.maxBuffer ?? 8 * 1024 * 1024,
				killSignal: "SIGKILL",
			},
			(error, stdout, stderr) => {
				const code = typeof (error as { code?: number } | null)?.code === "number" && error
					? ((error as { code?: number }).code as number)
					: error
						? -1
						: 0;
				const result: RunResult = { stdout: String(stdout), stderr: String(stderr), code };
				if (error && !okCodes.includes(code)) {
					reject(new RunFailure(cmd, args, result));
					return;
				}
				resolve(result);
			},
		);
	});
}

/** Run that returns failure instead of throwing (probe-style checks). */
export async function tryRun(cmd: string, args: readonly string[], opts: Parameters<typeof run>[2] = {}): Promise<RunResult> {
	try {
		return await run(cmd, args, { ...opts, okCodes: opts.okCodes ?? [0, 1] });
	} catch (err) {
		if (err instanceof RunFailure) return err.result;
		throw err;
	}
}
