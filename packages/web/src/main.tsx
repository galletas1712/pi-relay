import { createRoot } from "react-dom/client";
import { ServerApp } from "./serverApp.tsx";
import "./styles.css";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root element");

const standaloneDisplayMode =
	typeof window.matchMedia === "function" && window.matchMedia("(display-mode: standalone)").matches;
const legacyStandalone = Reflect.get(navigator, "standalone") === true;
if (standaloneDisplayMode || legacyStandalone) rootEl.classList.add("standalone-mode");

createRoot(rootEl).render(<ServerApp />);

if (import.meta.env.PROD && "serviceWorker" in navigator) {
	void navigator.serviceWorker.register("/service-worker.js", { scope: "/" }).catch(() => {
		// The app remains usable when a static host does not support service workers.
	});
}
