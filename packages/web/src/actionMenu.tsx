import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { MoreHorizontal } from "lucide-react";
import { Fragment, useRef, type ReactNode } from "react";

export interface ActionMenuItem {
	id: string;
	label: string;
	onSelect: () => void;
	icon?: ReactNode;
	disabled?: boolean;
	disabledReason?: string;
	destructive?: boolean;
	separatorBefore?: boolean;
	focusDestination?: "dialog";
}

export function ActionMenu({
	triggerLabel,
	items,
	open,
	defaultOpen,
	onOpenChange,
}: {
	triggerLabel: string;
	items: ActionMenuItem[];
	open?: boolean;
	defaultOpen?: boolean;
	onOpenChange?: (open: boolean) => void;
}) {
	const pendingDialogAction = useRef<(() => void) | null>(null);
	const triggerRef = useRef<HTMLButtonElement>(null);
	return (
		<DropdownMenu.Root open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange}>
			<DropdownMenu.Trigger asChild>
				<button ref={triggerRef} className="action-menu-trigger" type="button" aria-label={triggerLabel}>
					<MoreHorizontal size={17} aria-hidden />
				</button>
			</DropdownMenu.Trigger>
			<DropdownMenu.Portal>
				<DropdownMenu.Content
					className="action-menu-content"
					align="end"
					sideOffset={4}
					collisionPadding={8}
					onEscapeKeyDown={(event) => event.stopPropagation()}
					onCloseAutoFocus={(event) => {
						const action = pendingDialogAction.current;
						if (!action) return;
						pendingDialogAction.current = null;
						event.preventDefault();
						// Radix removes the closing menu focus scope after this handler.
						// Re-focus the opener before mounting so the dialog can restore it.
						queueMicrotask(() => {
							triggerRef.current?.focus();
							action();
						});
					}}
				>
					{items.map((item) => (
						<Fragment key={item.id}>
							{item.separatorBefore ? <DropdownMenu.Separator className="action-menu-separator" /> : null}
							<DropdownMenu.Item
								className={`action-menu-item ${item.destructive ? "destructive" : ""}`}
								disabled={item.disabled}
								onSelect={() => {
									if (item.focusDestination === "dialog") {
										pendingDialogAction.current ??= item.onSelect;
										return;
									}
									item.onSelect();
								}}
							>
								{item.icon ? <span className="action-menu-item-icon">{item.icon}</span> : null}
								<span className="action-menu-item-copy">
									<span>{item.label}</span>
									{item.disabled && item.disabledReason ? (
										<span className="action-menu-item-reason">{item.disabledReason}</span>
									) : null}
								</span>
							</DropdownMenu.Item>
						</Fragment>
					))}
				</DropdownMenu.Content>
			</DropdownMenu.Portal>
		</DropdownMenu.Root>
	);
}
