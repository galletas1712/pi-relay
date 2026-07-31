try {
	// Only an explicit user override pins a theme. Otherwise CSS follows the
	// system preference live without JavaScript updating the root element.
	const stored = localStorage.getItem("pi-relay-theme");
	if (stored === "dark") document.documentElement.classList.add("dark");
	else if (stored === "light") document.documentElement.classList.add("light");
} catch {
	// localStorage may be unavailable.
}
