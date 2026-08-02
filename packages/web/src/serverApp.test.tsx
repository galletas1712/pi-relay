// @vitest-environment jsdom

import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useQueryClient } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentApi } from "./agentApi.ts";
import { ServerProfileStore } from "./serverProfiles.ts";
import { browserWorkspaceRouteHistory } from "./workspaceRoute.ts";

const boundary = vi.hoisted(() => ({
	clients: [] as {
		url: string;
		close: ReturnType<typeof vi.fn>;
	}[],
	queryClients: [] as unknown[],
}));

vi.mock("./rpc.ts", async (importOriginal) => {
	const actual = await importOriginal<typeof import("./rpc.ts")>();
	return {
		...actual,
		AgentRpcClient: class {
			readonly connect = vi.fn(() => Promise.resolve());
			readonly reconnect = vi.fn(() => Promise.resolve());
			readonly close = vi.fn();
			readonly isOpen = vi.fn(() => false);
			readonly onEvent = vi.fn(() => () => undefined);
			readonly onStatus = vi.fn(() => () => undefined);
			readonly request = vi.fn();

			constructor(readonly url: string) {
				boundary.clients.push(this);
			}
		},
	};
});

vi.mock("./App.tsx", async () => {
	const React = await import("react");
	return {
		App: ({
			api,
			entityStorage,
			serverControls,
		}: {
			api: AgentApi;
			entityStorage: Storage;
			serverControls?: ReactNode;
		}) => {
			const queryClient = useQueryClient();
			if (!boundary.queryClients.includes(queryClient)) {
				boundary.queryClients.push(queryClient);
			}
			React.useEffect(() => {
				void api.connect();
				return () => api.close();
			}, [api]);
			return (
				<>
					<aside className="sidebar">
						{serverControls}
						<section className="project-section">Projects</section>
					</aside>
					<div data-testid="entity-draft">{entityStorage.getItem("draft")}</div>
				</>
			);
		},
	};
});

import { ServerApp } from "./serverApp.tsx";

beforeEach(() => {
	boundary.clients = [];
	boundary.queryClients = [];
	window.localStorage.clear();
	window.sessionStorage.clear();
	window.history.replaceState(null, "", "/");
});

afterEach(() => {
	cleanup();
	vi.restoreAllMocks();
});

describe("ServerApp profile boundary", () => {
	it("places compact server controls above Projects without exposing its URL", () => {
		const store = localStore();
		const { container } = render(<ServerApp store={store} />);
		const sidebar = container.querySelector<HTMLElement>(".sidebar");
		const controls = container.querySelector<HTMLElement>(".server-sidebar-controls");

		expect(container.querySelector(".server-bar")).toBeNull();
		expect(container.querySelector(".log-controls")).toBeNull();
		expect(sidebar).toBeTruthy();
		expect(controls).toBeTruthy();
		expect(within(controls!).getAllByRole("combobox", { name: "Active control server" }))
			.toHaveLength(1);
		expect(within(controls!).getByRole("button", { name: "Manage control servers" }))
			.toBeTruthy();
		expect(controls!.textContent).not.toContain("Manage");
		expect(controls!.textContent).not.toContain("ws://127.0.0.1:8787/");
		expect(
			controls!.compareDocumentPosition(
				container.querySelector(".project-section")!,
			) & Node.DOCUMENT_POSITION_FOLLOWING,
		).toBeTruthy();
	});

	it("replaces route, entity storage, query cache, and client when selecting another profile", async () => {
		const store = localStore();
		const user = userEvent.setup();
		const remote = store.add("Remote", "wss://control.example.test/socket")
			.profiles.find((profile) => profile.name === "Remote")!;
		store.storageFor("local").setItem("draft", "local draft");
		store.storageFor(remote.id).setItem("draft", "remote draft");
		render(<ServerApp store={store} />);

		window.history.replaceState(
			null,
			"",
			"/server/local/w/host/run/local-session/conversation/local-session",
		);
		await user.selectOptions(screen.getByLabelText("Active control server"), remote.id);

		expect(screen.getByTestId("entity-draft").textContent).toBe("remote draft");
		expect(window.location.pathname).toBe(`/server/${remote.id}/`);
		await waitFor(() => expect(boundary.clients).toHaveLength(2));
		expect(boundary.clients[0].close).toHaveBeenCalledOnce();
		expect(boundary.clients[1]).toMatchObject({
			url: remote.url,
		});
		expect(boundary.queryClients[1]).not.toBe(boundary.queryClients[0]);
	});

	it("edits the URL and rebuilds the active server boundary", async () => {
		const store = localStore();
		const user = userEvent.setup();
		render(<ServerApp store={store} />);
		await user.click(screen.getByRole("button", { name: "Manage control servers" }));
		const edit = screen.getByRole("button", { name: "Edit Local" });
		await user.click(edit);

		const url = screen.getByLabelText<HTMLInputElement>("WebSocket URL");
		expect(url.readOnly).toBe(false);
		const name = screen.getByLabelText("Name");
		expect(document.activeElement).toBe(name);
		await user.clear(name);
		await user.type(name, "Renamed");
		await user.clear(url);
		await user.type(url, "wss://renamed.example.test/");
		window.history.replaceState(
			null,
			"",
			"/server/local/w/host/run/old/conversation/old",
		);
		await user.click(screen.getByRole("button", { name: "Save changes" }));

		await waitFor(() => expect(store.current().profiles[0].name).toBe("Renamed"));
		expect(store.current().profiles[0].name).toBe("Renamed");
		expect(store.current().profiles[0].url).toBe("wss://renamed.example.test/");
		expect(window.location.pathname).toBe("/server/local/");
		await waitFor(() => expect(boundary.clients).toHaveLength(2));
		expect(boundary.clients[0].close).toHaveBeenCalledOnce();
		expect(boundary.clients[1].url).toBe("wss://renamed.example.test/");
		expect(boundary.queryClients[1]).not.toBe(boundary.queryClients[0]);
		await waitFor(() =>
			expect(document.activeElement).toBe(
				screen.getByRole("button", { name: "Edit Renamed" }),
			)
		);
	});

	it("selects the next profile root when removing the active profile", async () => {
		const store = localStore();
		const user = userEvent.setup();
		const remote = store.add("Remote", "wss://remote.example.test/")
			.profiles.find((profile) => profile.name === "Remote")!;
		render(<ServerApp store={store} />);
		window.history.replaceState(
			null,
			"",
			"/server/local/w/host/run/old/conversation/old",
		);
		await user.click(screen.getByRole("button", { name: "Manage control servers" }));
		const remove = screen.getByRole("button", { name: "Remove Local" });
		await user.click(remove);
		let confirmation = screen.getByRole("alertdialog", {
			name: "Remove Local from this browser?",
		});
		const cancel = within(confirmation).getByRole("button", { name: "Cancel" });
		expect(cancel).toBe(document.activeElement);
		await user.click(cancel);
		expect(store.current().profiles).toHaveLength(2);
		await waitFor(() => expect(document.activeElement).toBe(remove));

		await user.click(remove);
		confirmation = screen.getByRole("alertdialog", {
			name: "Remove Local from this browser?",
		});
		await user.click(within(confirmation).getByRole("button", { name: "Remove server" }));

		await waitFor(() => expect(boundary.clients).toHaveLength(2));
		expect(store.current().activeProfileId).toBe(remote.id);
		expect(window.location.pathname).toBe(`/server/${remote.id}/`);
		expect(boundary.clients[0].close).toHaveBeenCalledOnce();
	});

	it("opens first-run setup directly in add mode and restores focus after setup disappears", async () => {
		const store = emptyStore();
		const user = userEvent.setup();
		render(<ServerApp store={store} />);

		expect(screen.queryByLabelText("Active control server")).toBeNull();
		expect(screen.getByRole("heading", { name: "Connect a control server" })).toBeTruthy();
		const setup = screen.getByRole("button", { name: "Add control server" });
		await user.click(setup);

		expect(screen.getByRole("dialog", { name: "Control servers" })).toBeTruthy();
		const name = screen.getByLabelText("Name");
		expect(document.activeElement).toBe(name);
		expect(screen.queryByRole("button", { name: /^Add server$/u })).toBeTruthy();
		await user.type(name, "Tailnet");
		await user.type(screen.getByLabelText("WebSocket URL"), "wss://control.tail.test/");
		await user.click(screen.getByRole("button", { name: /^Add server$/u }));

		await waitFor(() => expect(store.current().profiles[0]?.name).toBe("Tailnet"));
		expect(screen.getByLabelText<HTMLSelectElement>("Active control server").value).toBe(
			store.current().profiles[0].id,
		);
		await user.click(screen.getByRole("button", { name: "Close server manager" }));
		await waitFor(() =>
			expect(document.activeElement).toBe(
				screen.getByRole("button", { name: "Manage control servers" }),
			)
		);
	});

	it("restores editor focus on cancel and keeps profile actions specifically named", async () => {
		const store = localStore();
		const user = userEvent.setup();
		store.add("Remote", "wss://remote.example.test/");
		render(<ServerApp store={store} />);

		await user.click(screen.getByRole("button", { name: "Manage control servers" }));
		expect(screen.getByRole("button", { name: "Edit Local" })).toBeTruthy();
		expect(screen.getByRole("button", { name: "Remove Local" })).toBeTruthy();
		expect(screen.getByRole("button", { name: "Edit Remote" })).toBeTruthy();
		expect(screen.getByRole("button", { name: "Remove Remote" })).toBeTruthy();
		const editRemote = screen.getByRole("button", { name: "Edit Remote" });
		await user.click(editRemote);
		expect(document.activeElement).toBe(screen.getByLabelText("Name"));
		await user.click(screen.getByRole("button", { name: "Cancel" }));
		await waitFor(() => expect(document.activeElement).toBe(editRemote));
	});

	it("switches profiles from the manager with an explicit accessible name", async () => {
		const store = localStore();
		const user = userEvent.setup();
		const remote = store.add("Remote", "wss://remote.example.test/")
			.profiles.find((profile) => profile.name === "Remote")!;
		render(<ServerApp store={store} />);

		await user.click(screen.getByRole("button", { name: "Manage control servers" }));
		expect(
			screen.getByRole("button", { name: "Local, active control server" })
				.getAttribute("aria-current"),
		).toBe("true");
		await user.click(screen.getByRole("button", { name: "Switch to Remote" }));

		expect(screen.queryByRole("dialog", { name: "Control servers" })).toBeNull();
		expect(store.current().activeProfileId).toBe(remote.id);
		expect(window.location.pathname).toBe(`/server/${remote.id}/`);
		await waitFor(() => expect(boundary.clients).toHaveLength(2));
	});

	it("does not deliver another profile's popstate to the departing route adapter", () => {
		window.history.replaceState(null, "", "/server/local/w/host/run/a/conversation/a");
		const history = browserWorkspaceRouteHistory("local")!;
		const listener = vi.fn();
		const unsubscribe = history.subscribe(listener);

		window.history.replaceState(null, "", "/server/remote/w/host/run/b/conversation/b");
		window.dispatchEvent(new PopStateEvent("popstate"));

		expect(listener).not.toHaveBeenCalled();
		unsubscribe();
	});
});

function localStore(): ServerProfileStore {
	return new ServerProfileStore(
		window.localStorage,
		window.sessionStorage,
		"ws://127.0.0.1:8787",
		idSequence(),
	);
}

function emptyStore(): ServerProfileStore {
	return new ServerProfileStore(
		window.localStorage,
		window.sessionStorage,
		null,
		idSequence(),
	);
}

function idSequence(): () => string {
	let next = 0;
	return () => `id-${++next}`;
}
