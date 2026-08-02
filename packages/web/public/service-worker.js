const CACHE_NAME = "pi-relay-shell-v1";
const APP_SHELL = [
	"/",
	"/index.html",
	"/manifest.webmanifest",
	"/service-worker.js",
	"/icon.svg",
	"/icons/icon-192.png",
	"/icons/icon-512.png",
];
const STATIC_DESTINATIONS = new Set(["document", "font", "image", "manifest", "script", "style"]);

async function cacheShell() {
	const cache = await caches.open(CACHE_NAME);
	const shell = await fetch("/", { cache: "no-cache" });
	if (!shell.ok) throw new Error("unable to fetch app shell");
	await cache.put("/", shell.clone());

	const html = await shell.text();
	const assetUrls = [...html.matchAll(/(?:src|href)="([^"]+)"/g)]
		.map(([, url]) => url)
		.filter((url) => url.startsWith("/") && !url.startsWith("//"));
	await cache.addAll([...new Set([...APP_SHELL, ...assetUrls])]);
}

self.addEventListener("install", (event) => {
	event.waitUntil(cacheShell().then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
	event.waitUntil(self.clients.claim());
});

function isStaticRequest(request) {
	if (request.method !== "GET" || request.mode === "websocket" || !STATIC_DESTINATIONS.has(request.destination)) {
		return false;
	}

	const url = new URL(request.url);
	if (url.origin !== self.location.origin || /^\/(?:api|rpc)(?:\/|$)/.test(url.pathname)) {
		return false;
	}

	return true;
}

async function networkFirst(request) {
	const cache = await caches.open(CACHE_NAME);
	try {
		const response = await fetch(request);
		if (response.ok) {
			await cache.put(request, response.clone());
		}
		return response;
	} catch {
		const cached = await cache.match(request);
		if (cached) return cached;
		if (request.destination === "document") {
			const shell = await cache.match("/");
			if (shell) return shell;
		}
		throw new Error("offline and no cached static resource");
	}
}

self.addEventListener("fetch", (event) => {
	if (isStaticRequest(event.request)) {
		event.respondWith(networkFirst(event.request));
	}
});
