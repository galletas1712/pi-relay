import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { applySelectedSnapshot } from "./selectedSessionCache.ts";
import { useSelectedSessionStore } from "./selectedSessionStore.ts";
import type { SessionSnapshot } from "./types.ts";

describe("useSelectedSessionStore", () => {
	it("warms an unfocused session cache", () => {
		let warmedTitle: unknown = null;

		function Probe() {
			const store = useSelectedSessionStore("session_selected");
			store.warm("session_background", (current) => applySelectedSnapshot(current, snapshot("session_background", "Background")));
			warmedTitle = store.get("session_background")?.snapshot?.metadata.title;
			return null;
		}

		renderToStaticMarkup(<Probe />);

		expect(warmedTitle).toBe("Background");
	});
});

function snapshot(sessionId: string, title: string): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: null,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: { title },
		pending_actions: [],
		queued_inputs: [],
		last_event_id: 1,
		server_time_ms: 1_700_000_000_000,
	};
}
