import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRoot } from "react-dom/client";
import { App } from "./App.tsx";
import { TooltipProvider } from "@/components/ui/tooltip";
import "./styles.css";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root element");

const queryClient = new QueryClient({
	defaultOptions: {
		queries: {
			staleTime: 15_000,
			gcTime: 10 * 60_000,
			refetchOnWindowFocus: false,
			retry: 1,
		},
	},
});

createRoot(rootEl).render(
	<QueryClientProvider client={queryClient}>
		<TooltipProvider>
			<App />
		</TooltipProvider>
	</QueryClientProvider>,
);
