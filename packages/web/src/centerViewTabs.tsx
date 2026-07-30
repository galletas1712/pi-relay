import type { CenterView } from "./workspaceRoute.ts";

const CENTER_VIEWS: { id: CenterView; label: string; disabled?: boolean }[] = [
	{ id: "chat", label: "Chat" },
	{ id: "git", label: "Git" },
	{ id: "files", label: "Files", disabled: true },
];

export function CenterViewTabs({
	activeView,
	onChange,
}: {
	activeView: CenterView;
	onChange: (view: CenterView) => void;
}) {
	return (
		<div className="center-view-tabs" role="tablist" aria-label="Center pane views">
			{CENTER_VIEWS.map((view) => (
				<button
					key={view.id}
					className={`center-view-tab ${activeView === view.id ? "active" : ""}`}
					type="button"
					role="tab"
					id={`center-view-tab-${view.id}`}
					aria-selected={activeView === view.id}
					aria-controls={`center-view-panel-${view.id}`}
					disabled={view.disabled}
					title={view.disabled ? "Coming soon" : undefined}
					onClick={() => onChange(view.id)}
				>
					{view.label}
				</button>
			))}
		</div>
	);
}
