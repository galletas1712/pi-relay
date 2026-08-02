import { app, BrowserWindow, shell } from "electron";
import { navigationPolicy, parseAppUrl } from "./policy.mjs";
import { shouldRefreshOnForeground } from "./refresh.mjs";

const DEFAULT_WEB_URL = "https://pi-relay.pages.dev";
const webUrl = parseAppUrl(process.env.PI_RELAY_WEB_URL ?? DEFAULT_WEB_URL);
const appOrigin = webUrl.origin;
let mainWindow;
let hiddenAt = null;

if (!app.requestSingleInstanceLock()) {
	app.quit();
} else {
	app.on("second-instance", () => {
		if (!mainWindow) return;
		if (mainWindow.isMinimized()) mainWindow.restore();
		mainWindow.focus();
	});

	app.whenReady().then(() => {
		if (process.platform === "win32") {
			app.setAppUserModelId("dev.pi-relay.desktop");
		}
		createWindow();

		app.on("activate", () => {
			if (BrowserWindow.getAllWindows().length === 0) createWindow();
		});
	});

	app.on("window-all-closed", () => {
		if (process.platform !== "darwin") app.quit();
	});
}

function createWindow() {
	mainWindow = new BrowserWindow({
		width: 1280,
		height: 850,
		minWidth: 800,
		minHeight: 600,
		webPreferences: {
			nodeIntegration: false,
			contextIsolation: true,
			sandbox: true,
		},
	});

	const { webContents } = mainWindow;
	webContents.setWindowOpenHandler(({ url }) => openExternalOrDeny(url));
	webContents.on("will-navigate", (event, url) => {
		const decision = navigationPolicy(url, appOrigin);
		if (decision.action === "allow") return;
		event.preventDefault();
		if (decision.action === "external") shell.openExternal(decision.url);
	});

	mainWindow.on("hide", () => {
		hiddenAt = Date.now();
	});
	mainWindow.on("minimize", () => {
		hiddenAt = Date.now();
	});
	mainWindow.on("show", () => {
		if (shouldRefreshOnForeground(hiddenAt, Date.now())) {
			webContents.reloadIgnoringCache();
		}
		hiddenAt = null;
	});
	mainWindow.on("restore", () => {
		if (shouldRefreshOnForeground(hiddenAt, Date.now())) {
			webContents.reloadIgnoringCache();
		}
		hiddenAt = null;
	});
	mainWindow.on("closed", () => {
		mainWindow = undefined;
		hiddenAt = null;
	});

	// Reload the document while preserving the app's storage and service worker.
	webContents.loadURL(webUrl.href, { extraHeaders: "Cache-Control: no-cache\r\n" });
}

function openExternalOrDeny(candidate) {
	const decision = navigationPolicy(candidate, appOrigin);
	if (decision.action === "allow") {
		return { action: "allow" };
	}
	if (decision.action === "external") {
		shell.openExternal(decision.url);
	}
	return { action: "deny" };
}
