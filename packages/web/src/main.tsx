import { createRoot } from "react-dom/client";
import { ServerApp } from "./serverApp.tsx";
import "./styles.css";

const rootEl = document.getElementById("root");
if (!rootEl) throw new Error("missing #root element");

createRoot(rootEl).render(<ServerApp />);
