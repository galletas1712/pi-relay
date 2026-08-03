import { app, BrowserWindow, nativeTheme, powerMonitor, shell } from "electron";
import { navigationPolicy, parseAppUrl } from "./policy.mjs";

const DEFAULT_WEB_URL = "https://pi-relay.pages.dev";
const webUrl = parseAppUrl(process.env.PI_RELAY_WEB_URL ?? DEFAULT_WEB_URL);
const appOrigin = webUrl.origin;
const LOAD_RETRY_MS = 500;
let mainWindow;
let loadRetryTimer = null;

if (!app.requestSingleInstanceLock()) {
	app.quit();
} else {
	app.on("second-instance", () => {
		app.whenReady().then(() => {
			if (!mainWindow || mainWindow.isDestroyed()) {
				createWindow();
				return;
			}
			if (mainWindow.isMinimized()) mainWindow.restore();
			if (!mainWindow.isVisible()) mainWindow.show();
			mainWindow.focus();
		});
	});

	app.whenReady().then(() => {
		if (process.platform === "win32") {
			app.setAppUserModelId("dev.pi-relay.desktop");
		}
		createWindow();

		powerMonitor.on("resume", () => {
			if (!mainWindow || mainWindow.isDestroyed()) return;
			mainWindow.webContents.invalidate();
		});

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

function windowBackgroundColor() {
	return nativeTheme.shouldUseDarkColors ? "#282828" : "#fbf1c7";
}

function createWindow() {
	mainWindow = new BrowserWindow({
		width: 1280,
		height: 850,
		minWidth: 800,
		minHeight: 600,
		backgroundColor: windowBackgroundColor(),
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
	webContents.on("did-fail-load", (_event, errorCode, _description, _url, isMainFrame) => {
		// -3 is ERR_ABORTED (superseded navigation); skip retry.
		if (!isMainFrame || errorCode === -3) return;
		scheduleLoadRetry();
	});
	webContents.on("did-finish-load", clearLoadRetry);
	webContents.on("render-process-gone", () => {
		clearLoadRetry();
		loadApp();
	});
	mainWindow.on("closed", () => {
		clearLoadRetry();
		mainWindow = undefined;
	});

	loadApp();
}

function loadApp() {
	if (!mainWindow || mainWindow.isDestroyed()) return;
	mainWindow.webContents.loadURL(webUrl.href);
}

function scheduleLoadRetry() {
	if (loadRetryTimer !== null) return;
	loadRetryTimer = setTimeout(() => {
		loadRetryTimer = null;
		loadApp();
	}, LOAD_RETRY_MS);
}

function clearLoadRetry() {
	if (loadRetryTimer === null) return;
	clearTimeout(loadRetryTimer);
	loadRetryTimer = null;
}

function openExternalOrDeny(candidate) {
	const decision = navigationPolicy(candidate, appOrigin);
	if (decision.action === "external") {
		shell.openExternal(decision.url);
	}
	return { action: "deny" };
}
