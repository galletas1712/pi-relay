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
		if (!mainWindow || mainWindow.isDestroyed()) {
			createWindow();
			return;
		}
		if (mainWindow.isMinimized()) mainWindow.restore();
		if (!mainWindow.isVisible()) mainWindow.show();
		mainWindow.focus();
	});

	app.whenReady().then(() => {
		if (process.platform === "win32") {
			app.setAppUserModelId("dev.pi-relay.desktop");
		}
		createWindow();

		app.on("activate", () => {
			if (BrowserWindow.getAllWindows().length === 0) {
				createWindow();
			} else if (mainWindow) {
				if (mainWindow.isMinimized()) mainWindow.restore();
				if (!mainWindow.isVisible()) mainWindow.show();
				mainWindow.focus();
			}
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
	const handleNavigation = (event, url) => {
		const decision = navigationPolicy(url, appOrigin);
		if (decision.action === "allow") return;
		event.preventDefault();
		if (decision.action === "external") shell.openExternal(decision.url);
	};
	webContents.on("will-navigate", handleNavigation);
	webContents.on("will-redirect", handleNavigation);

	mainWindow.on("hide", () => {
		markBackgrounded();
	});
	mainWindow.on("minimize", () => {
		markBackgrounded();
	});
	mainWindow.on("blur", markBackgrounded);
	mainWindow.on("show", refreshOnForeground);
	mainWindow.on("restore", refreshOnForeground);
	mainWindow.on("focus", refreshOnForeground);
	mainWindow.on("closed", () => {
		mainWindow = undefined;
		hiddenAt = null;
	});

	// Reload the document while preserving the app's storage and service worker.
	webContents.loadURL(webUrl.href, { extraHeaders: "Cache-Control: no-cache\r\n" });
}

function markBackgrounded() {
	if (hiddenAt === null) hiddenAt = Date.now();
}

function refreshOnForeground() {
	const backgroundedAt = hiddenAt;
	hiddenAt = null;
	if (shouldRefreshOnForeground(backgroundedAt, Date.now())) {
		mainWindow?.webContents.reloadIgnoringCache();
	}
}

function openExternalOrDeny(candidate) {
	const decision = navigationPolicy(candidate, appOrigin);
	if (decision.action === "external") {
		shell.openExternal(decision.url);
	}
	return { action: "deny" };
}
