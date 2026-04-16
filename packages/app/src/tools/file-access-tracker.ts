import { createHash } from "node:crypto";

type TrackedFileState = {
	fingerprint: string;
	knowsFullContent: boolean;
};

function buildReadBeforeMutateError(displayPath: string, action: string, requireFullContent: boolean): Error {
	if (requireFullContent) {
		return new Error(`Read the full current contents of ${displayPath} with read before using ${action}.`);
	}
	return new Error(`Read ${displayPath} with read before using ${action}.`);
}

function buildStaleReadError(displayPath: string, action: string): Error {
	return new Error(`Read ${displayPath} again before using ${action} because the file changed since the last read.`);
}

function buildFullContentError(displayPath: string, action: string): Error {
	return new Error(`Read the full current contents of ${displayPath} with read before using ${action}.`);
}

export function fingerprintFileContent(content: string): string {
	return createHash("sha1").update(content).digest("hex");
}

export class FileAccessTracker {
	private readonly files = new Map<string, TrackedFileState>();

	shouldReturnCachedRead(path: string, fingerprint: string): boolean {
		const state = this.files.get(path);
		return state?.fingerprint === fingerprint && state.knowsFullContent;
	}

	recordRead(path: string, fingerprint: string, deliveredFullContent: boolean): void {
		const previous = this.files.get(path);
		const knowsFullContent =
			deliveredFullContent ||
			(previous?.fingerprint === fingerprint && previous.knowsFullContent);
		this.files.set(path, { fingerprint, knowsFullContent });
	}

	assertFreshRead(
		path: string,
		displayPath: string,
		fingerprint: string,
		action: string,
		requireFullContent = false,
	): void {
		const state = this.files.get(path);
		if (!state) {
			throw buildReadBeforeMutateError(displayPath, action, requireFullContent);
		}
		if (state.fingerprint !== fingerprint) {
			throw buildStaleReadError(displayPath, action);
		}
		if (requireFullContent && !state.knowsFullContent) {
			throw buildFullContentError(displayPath, action);
		}
	}

	recordMutation(path: string, fingerprint: string, knowsFullContent = false): void {
		const previous = this.files.get(path);
		this.files.set(path, {
			fingerprint,
			knowsFullContent: knowsFullContent || previous?.knowsFullContent === true,
		});
	}

	knowsFullContent(path: string): boolean {
		return this.files.get(path)?.knowsFullContent === true;
	}

	forget(path: string): void {
		this.files.delete(path);
	}
}

export function createFileAccessTracker(): FileAccessTracker {
	return new FileAccessTracker();
}
