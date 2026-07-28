// @vitest-environment jsdom

import { act, cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SystemPromptDisclosure } from "./systemPromptDisclosure.tsx";

afterEach(() => {
	cleanup();
});

describe("SystemPromptDisclosure", () => {
	it("expands rendered markdown inline, exposes no dialog, and hides it without refetching", async () => {
		const loadPrompt = vi.fn(async () => "Rendered **instructions** with [docs](https://example.com).");
		const user = userEvent.setup();
		render(<SystemPromptDisclosure loadPrompt={loadPrompt} />);

		const see = screen.getByRole("button", { name: "See system prompt" });
		expect(see.getAttribute("aria-expanded")).toBe("false");
		await user.click(see);

		expect(await screen.findByText("instructions")).toBeTruthy();
		expect(screen.getByRole("link", { name: "docs" }).getAttribute("target")).toBe("_blank");
		expect(screen.queryByRole("dialog")).toBeNull();
		const hide = screen.getByRole("button", { name: "Hide system prompt" });
		expect(hide.getAttribute("aria-expanded")).toBe("true");

		await user.click(hide);
		expect(screen.queryByText("instructions")).toBeNull();
		await user.click(screen.getByRole("button", { name: "See system prompt" }));
		expect(await screen.findByText("instructions")).toBeTruthy();
		expect(loadPrompt).toHaveBeenCalledTimes(1);
	});

	it("shows loading and an inline error, then retries successfully", async () => {
		const first = deferred<string | null>();
		const loadPrompt = vi.fn()
			.mockImplementationOnce(() => first.promise)
			.mockResolvedValueOnce("Recovered prompt");
		const user = userEvent.setup();
		render(<SystemPromptDisclosure loadPrompt={loadPrompt} />);

		await user.click(screen.getByRole("button", { name: "See system prompt" }));
		expect(screen.getByRole("status").textContent).toContain("Loading system prompt…");
		await act(async () => first.reject(new Error("prompt fetch failed")));

		expect((await screen.findByRole("alert")).textContent).toContain("prompt fetch failed");
		await user.click(screen.getByRole("button", { name: "Retry" }));
		expect(await screen.findByText("Recovered prompt")).toBeTruthy();
		expect(loadPrompt).toHaveBeenCalledTimes(2);
	});

	it("can collapse a pending request and ignores its late response", async () => {
		const prompt = deferred<string | null>();
		const user = userEvent.setup();
		render(<SystemPromptDisclosure loadPrompt={() => prompt.promise} />);

		await user.click(screen.getByRole("button", { name: "See system prompt" }));
		expect(screen.getByText("Loading system prompt…")).toBeTruthy();
		await user.click(screen.getByRole("button", { name: "Hide system prompt" }));
		await act(async () => prompt.resolve("Late prompt"));

		expect(screen.queryByText("Late prompt")).toBeNull();
		expect(screen.getByRole("button", { name: "See system prompt" })).toBeTruthy();
	});

	it("blocks remote loads while disconnected but always permits collapse", async () => {
		const loadPrompt = vi.fn(async () => "Persisted prompt");
		const user = userEvent.setup();
		const { rerender } = render(
			<SystemPromptDisclosure
				loadPrompt={loadPrompt}
				remoteReadBlockedReason="Waiting for connection"
			/>,
		);

		expect(screen.getByRole<HTMLButtonElement>("button", { name: "See system prompt" }).disabled).toBe(true);
		expect(screen.getByText("Waiting for connection")).toBeTruthy();
		rerender(<SystemPromptDisclosure loadPrompt={loadPrompt} />);
		await user.click(screen.getByRole("button", { name: "See system prompt" }));
		expect(await screen.findByText("Persisted prompt")).toBeTruthy();

		rerender(
			<SystemPromptDisclosure
				loadPrompt={loadPrompt}
				remoteReadBlockedReason="Waiting for connection"
			/>,
		);
		expect(screen.getByRole<HTMLButtonElement>("button", { name: "Hide system prompt" }).disabled).toBe(false);
		await user.click(screen.getByRole("button", { name: "Hide system prompt" }));
		expect(screen.queryByText("Persisted prompt")).toBeNull();
		expect(screen.getByRole<HTMLButtonElement>("button", { name: "See system prompt" }).disabled).toBe(false);
		await user.click(screen.getByRole("button", { name: "See system prompt" }));
		expect(screen.getByText("Persisted prompt")).toBeTruthy();
		expect(loadPrompt).toHaveBeenCalledTimes(1);
	});
});

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (error: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}
