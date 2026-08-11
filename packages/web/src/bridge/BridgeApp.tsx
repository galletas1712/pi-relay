// M11b: the bridge profile IS the legacy pi-relay app. BridgeAgentApi serves
// the legacy AgentApi surface over the bridge contract (packages/bridge,
// contract v0), so the sidebar / chat pane / inspector rail (Agents + Files +
// REPL) / history + MCP sheets all run unchanged. The M6 interim shell is
// gone; the only bridge-native surface kept is the REPL pane (bash-block
// idiom), toggled from the inspector rail as a third tab.
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useEffect, useMemo } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import { App } from "../App.tsx";
import { BridgeClient } from "./client.ts";
import { BridgeAgentApi } from "./legacyApi.ts";
import { BridgeProvider, bridgeWebSocketUrl } from "./useBridge.tsx";
import { ReplPane } from "./components/ReplPane.tsx";

export function BridgeApp() {
	// One socket, shared by the adapter (AgentApi surface) and the REPL store.
	const client = useMemo(() => new BridgeClient(bridgeWebSocketUrl()), []);
	const api = useMemo(() => new BridgeAgentApi(client), [client]);
	const queryClient = useMemo(
		() =>
			new QueryClient({
				defaultOptions: { queries: { retry: 1, staleTime: 500, refetchOnWindowFocus: true } },
			}),
		[],
	);
	useEffect(() => () => queryClient.clear(), [queryClient]);
	// Dev-only inspection hook: lets headless UI verification drive the adapter
	// surface directly (stripped from production builds by import.meta.env.DEV).
	useEffect(() => {
		if (!import.meta.env.DEV) return;
		(window as unknown as { __bridgeApi?: unknown }).__bridgeApi = api;
		return () => {
			delete (window as unknown as { __bridgeApi?: unknown }).__bridgeApi;
		};
	}, [api]);

	return (
		<QueryClientProvider client={queryClient}>
			<TooltipProvider>
				<BridgeProvider client={client}>
					<App
						api={api}
						renderReplPane={(sessionId) => (
							<ReplPane
								sessionId={sessionId}
								onClose={() => {
									// rail idiom: "closing" the REPL = activating Agents tab
									document.getElementById("inspector-tab-run-board")?.click();
								}}
							/>
						)}
					/>
				</BridgeProvider>
			</TooltipProvider>
		</QueryClientProvider>
	);
}
