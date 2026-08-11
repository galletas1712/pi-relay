// Typed errors for workspace-lib. Codes mirror the Rust anyhow bail messages
// categories so the bridge can map them onto contract error codes.
export class WorkspaceError extends Error {
	readonly code: string;
	constructor(code: string, message: string) {
		super(message);
		this.code = code;
	}
}

export function asWorkspaceError(err: unknown, code = "internal"): WorkspaceError {
	if (err instanceof WorkspaceError) return err;
	const msg = err instanceof Error ? err.message : String(err);
	return new WorkspaceError(code, msg);
}
