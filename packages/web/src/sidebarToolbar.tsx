import { Archive, Plus, Search, X } from "lucide-react";
import { useEffect, useRef, useState, type RefObject } from "react";

export function SidebarToolbar({
	disabled,
	query,
	onQueryChange,
	showArchived,
	onToggleArchived,
	onNew,
	newSessionButtonRef,
}: {
	disabled: boolean;
	query: string;
	onQueryChange: (query: string) => void;
	showArchived: boolean;
	onToggleArchived: () => void;
	onNew: () => void;
	newSessionButtonRef?: RefObject<HTMLButtonElement | null>;
}) {
	const [searchOpen, setSearchOpen] = useState(false);
	const searchInputRef = useRef<HTMLInputElement | null>(null);
	const searchVisible = searchOpen || !!query.trim();

	useEffect(() => {
		if (!searchOpen || disabled) return;
		const frame = requestAnimationFrame(() => searchInputRef.current?.focus());
		return () => cancelAnimationFrame(frame);
	}, [disabled, searchOpen]);

	useEffect(() => {
		if (disabled || searchOpen) return;
		const handleKeyDown = (event: KeyboardEvent) => {
			const target = event.target as HTMLElement | null;
			const activeElement = document.activeElement as HTMLElement | null;
			const isTypingTarget =
				target instanceof HTMLInputElement ||
				target instanceof HTMLTextAreaElement ||
				target?.isContentEditable;
			if (isTypingTarget) return;
			if (!activeElement?.closest('[data-slot="sidebar"]')) return;
			if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "f") {
				event.preventDefault();
				setSearchOpen(true);
			}
		};
		window.addEventListener("keydown", handleKeyDown);
		return () => window.removeEventListener("keydown", handleKeyDown);
	}, [disabled, searchOpen]);

	return (
		<div className="sidebar-toolbar">
			<div className="session-section-head">
				<span>Sessions</span>
				<span className="sidebar-section-actions">
					<button
						ref={newSessionButtonRef}
						className="icon-button"
						type="button"
						onClick={onNew}
						disabled={disabled}
						aria-label="new session"
						title="New session"
					>
						<Plus size={14} />
					</button>
					<button
						className={`icon-button ${searchVisible ? "pressed" : ""}`}
						type="button"
						onClick={() => {
							if (searchVisible) {
								onQueryChange("");
								setSearchOpen(false);
							} else {
								setSearchOpen(true);
							}
						}}
						disabled={disabled}
						aria-label={searchVisible ? "Close Filter Sessions" : "Filter Sessions"}
						aria-pressed={searchVisible}
						title={searchVisible ? "Close Filter Sessions" : "Filter Sessions"}
					>
						<Search size={14} />
					</button>
					<button
						className={`icon-button ${showArchived ? "pressed" : ""}`}
						type="button"
						onClick={onToggleArchived}
						disabled={disabled}
						aria-label={showArchived ? "hide archived sessions" : "show archived sessions"}
						aria-pressed={showArchived}
						title={showArchived ? "hide archived sessions" : "show archived sessions"}
					>
						<Archive size={14} />
					</button>
				</span>
			</div>
			{searchVisible ? (
				<label
					className="search-box"
					onBlur={(event) => {
						if (event.currentTarget.contains(event.relatedTarget)) return;
						if (!query.trim()) setSearchOpen(false);
					}}
				>
					<input
						ref={searchInputRef}
						value={query}
						onChange={(event) => onQueryChange(event.target.value)}
						onKeyDown={(event) => {
							if (event.key !== "Escape") return;
							event.preventDefault();
							if (query.trim()) onQueryChange("");
							else setSearchOpen(false);
						}}
						placeholder="Filter Sessions…"
						disabled={disabled}
					/>
					{query ? (
						<button
							className="search-clear-button"
							type="button"
							onClick={() => {
								onQueryChange("");
								searchInputRef.current?.focus();
							}}
							aria-label="clear session filter"
							title="Clear Filter Sessions"
						>
							<X size={13} />
						</button>
					) : null}
				</label>
			) : null}
		</div>
	);
}
