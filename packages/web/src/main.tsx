import { createRoot } from "react-dom/client";
import { ServerApp } from "./serverApp.tsx";
import "./styles.css";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root element");

createRoot(rootEl).render(<ServerApp />);

if (import.meta.env.PROD && "serviceWorker" in navigator) {
	void navigator.serviceWorker.register("/service-worker.js", { scope: "/" }).catch(() => {
		// The app remains usable when a static host does not support service workers.
	});
}
