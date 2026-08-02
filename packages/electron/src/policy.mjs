const HTTP_PROTOCOLS = new Set(["http:", "https:"]);

export function parseAppUrl(value) {
	const raw = value?.trim();
	if (!raw) {
		throw new Error("PI_RELAY_WEB_URL must be a non-empty HTTP(S) URL");
	}

	let url;
	try {
		url = new URL(raw);
	} catch {
		throw new Error(`PI_RELAY_WEB_URL is not a valid URL: ${value}`);
	}
	if (!HTTP_PROTOCOLS.has(url.protocol)) {
		throw new Error(`PI_RELAY_WEB_URL must use http or https, got ${url.protocol}`);
	}
	if (url.username || url.password) {
		throw new Error("PI_RELAY_WEB_URL must not contain credentials");
	}
	return url;
}

export function navigationPolicy(candidate, appOrigin) {
	let url;
	try {
		url = new URL(candidate);
	} catch {
		return { action: "deny" };
	}

	if (url.username || url.password) return { action: "deny" };
	if (url.origin === appOrigin && HTTP_PROTOCOLS.has(url.protocol)) {
		return { action: "allow" };
	}
	if (HTTP_PROTOCOLS.has(url.protocol)) {
		return { action: "external", url: url.href };
	}
	return { action: "deny" };
}
