/**
 * Pending-input buffer for the interactive TUI.
 *
 * Rationale — "Enter becomes no-op during session switch" symptom:
 *   When the user presses Enter while the main loop is blocked inside
 *   `await session.prompt("/agents")` (the slash-command path for agent switch),
 *   the `onInputCallback` that `getUserInput()` would have resolved is unset.
 *   Without this buffer, `submitValue()` had already cleared the editor text,
 *   so the user saw "text disappeared and nothing happened." No error, no
 *   status, no log. See the H1 investigation from tui-unresponsive-on-agent-switch.
 *
 * Shape:
 *   - `push(text)` — called from the onSubmit handler's else branch when
 *     `onInputCallback` is undefined.
 *   - `tryShift()` — called from `getUserInput()` before installing a fresh
 *     callback; if a buffered input exists, returns it immediately and the
 *     caller resolves the promise synchronously without waiting for the next
 *     physical keypress.
 *
 * Caveat (documented here, not hidden):
 *   If a user typed input intended for the OLD agent but the text arrives
 *   AFTER the switch completes, the buffered text is delivered to the NEW
 *   (switched-to) agent. The alternative (capture attached-agent id at submit
 *   time and drop if unchanged) is more complex and has not been empirically
 *   needed. This trade-off is documented at both call sites as well.
 *
 * The surrounding interactive mode is free to also emit a status/log when
 * push() is called, so the queueing is observable. This class deliberately
 * does not own UI concerns.
 */
export class PendingInputBuffer {
	private buffer: string[] = [];

	push(text: string): void {
		this.buffer.push(text);
	}

	tryShift(): string | undefined {
		return this.buffer.shift();
	}

	get length(): number {
		return this.buffer.length;
	}

	clear(): void {
		this.buffer.length = 0;
	}

	/** Non-destructive peek at buffered items. Primarily for tests. */
	snapshot(): readonly string[] {
		return [...this.buffer];
	}
}
