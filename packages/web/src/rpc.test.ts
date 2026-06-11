import { describe, expect, it } from "vitest";
import { resolveWsUrl } from "./rpc.ts";

describe("resolveWsUrl", () => {
	it("honors an explicit VITE_PI_AGENT_WS override", () => {
		expect(resolveWsUrl("wss://agent.example.test/ws", localLocation())).toBe("wss://agent.example.test/ws");
	});

	it("defaults local web clients to the daemon port", () => {
		expect(resolveWsUrl(undefined, localLocation())).toBe("ws://127.0.0.1:8787");
		expect(resolveWsUrl("", { protocol: "http:", hostname: "localhost", port: "8788" })).toBe("ws://127.0.0.1:8787");
	});

	it("uses same-origin /ws for non-local served clients", () => {
		expect(resolveWsUrl(undefined, { protocol: "https:", hostname: "odin.smelt-anaconda.ts.net", port: "" })).toBe(
			"wss://odin.smelt-anaconda.ts.net/ws",
		);
		expect(resolveWsUrl(undefined, { protocol: "http:", hostname: "example.test", port: "9000" })).toBe(
			"ws://example.test:9000/ws",
		);
	});
});

function localLocation(): Pick<Location, "hostname" | "port" | "protocol"> {
	return { protocol: "http:", hostname: "127.0.0.1", port: "8788" };
}
