import { createRoot } from "react-dom/client";
import { resolveBackendProfile } from "./bridge/profile.ts";
import { ServerApp } from "./serverApp.tsx";
import "./styles.css";
import "./bridge/bridge.css";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root element");

const standaloneDisplayMode =
	typeof window.matchMedia === "function" && window.matchMedia("(display-mode: standalone)").matches;
const legacyStandalone = Reflect.get(navigator, "standalone") === true;
if (standaloneDisplayMode || legacyStandalone) rootEl.classList.add("standalone-mode");

const root = createRoot(rootEl);
if (resolveBackendProfile() === "bridge") {
	// Bridge profile: separate data layer over contract v0 (packages/bridge).
	// Lazy-imported so the legacy bundle never carries it. The legacy path
	// below is untouched; the default profile remains legacy.
	void import("./bridge/BridgeApp.tsx").then(({ BridgeApp }) => root.render(<BridgeApp />));
} else {
	root.render(<ServerApp />);
}

if (import.meta.env.PROD && "serviceWorker" in navigator) {
	void navigator.serviceWorker.register("/service-worker.js", { scope: "/" }).catch(() => {
		// The app remains usable when a static host does not support service workers.
	});
}
