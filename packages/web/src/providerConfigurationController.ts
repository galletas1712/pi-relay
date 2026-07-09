import type { ConfigureSessionResult } from "./agentApi.ts";
import type { ProviderConfig } from "./types.ts";

export interface ProviderConfigurationTarget {
	sessionId: string;
	projectId: string | null;
}

export type ProviderConfigurationEdit = "model" | "reasoning effort";

export interface ProviderConfigurationControllerOptions {
	configure: (
		target: ProviderConfigurationTarget,
		provider: ProviderConfig,
	) => Promise<ConfigureSessionResult>;
	commit: (
		target: ProviderConfigurationTarget,
		provider: ProviderConfig,
		result: ConfigureSessionResult,
	) => void;
	fail: (
		target: ProviderConfigurationTarget,
		edit: ProviderConfigurationEdit,
		error: unknown,
	) => void;
	change: () => void;
}

interface PendingConfiguration {
	readonly target: ProviderConfigurationTarget;
	desired: ProviderConfig;
	edit: ProviderConfigurationEdit;
	running: boolean;
	task: Promise<void> | null;
	transforms: Array<(current: ProviderConfig) => ProviderConfig>;
}

/**
 * Serializes provider configuration per session and coalesces edits to the
 * latest complete provider. Targets are captured once so navigation cannot
 * redirect an in-flight mutation or its cache/error callbacks.
 */
export class ProviderConfigurationController {
	private readonly pending = new Map<string, PendingConfiguration>();
	private disposed = false;

	constructor(private readonly options: ProviderConfigurationControllerOptions) {}

	update(
		target: ProviderConfigurationTarget,
		base: ProviderConfig,
		transform: (current: ProviderConfig) => ProviderConfig,
		edit: ProviderConfigurationEdit,
	): void {
		if (this.disposed) return;
		let pending = this.pending.get(target.sessionId);
		if (!pending) {
			pending = {
				target: { ...target },
				desired: base,
				edit,
				running: false,
				task: null,
				transforms: [],
			};
			this.pending.set(target.sessionId, pending);
		}
		pending.desired = transform(pending.desired);
		pending.edit = edit;
		if (pending.running) pending.transforms.push(transform);
		this.options.change();
		if (!pending.running) pending.task = this.run(pending);
	}

	desired(sessionId: string): ProviderConfig | null {
		return this.pending.get(sessionId)?.desired ?? null;
	}

	settled(sessionId: string): Promise<void> {
		return this.pending.get(sessionId)?.task ?? Promise.resolve();
	}

	dispose(): void {
		if (this.disposed) return;
		this.disposed = true;
		this.pending.clear();
		this.options.change();
	}

	private async run(pending: PendingConfiguration): Promise<void> {
		pending.running = true;
		while (!this.disposed && this.pending.get(pending.target.sessionId) === pending) {
			const requested = pending.desired;
			pending.transforms = [];
			try {
				const result = await this.options.configure(pending.target, requested);
				if (this.disposed) return;
				const provider = result.provider ?? requested;
				this.options.commit(pending.target, provider, result);
				if (pending.transforms.length > 0) {
					pending.desired = pending.transforms.reduce(
						(current, transform) => transform(current),
						provider,
					);
					continue;
				}
				this.pending.delete(pending.target.sessionId);
				this.options.change();
				return;
			} catch (error) {
				if (this.disposed) return;
				if (pending.desired !== requested) continue;
				this.pending.delete(pending.target.sessionId);
				this.options.fail(pending.target, pending.edit, error);
				this.options.change();
				return;
			}
		}
	}
}
