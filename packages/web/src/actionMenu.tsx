import { MoreHorizontal } from "lucide-react";
import { Fragment, useRef, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuGroup,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

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
		<DropdownMenu open={open} defaultOpen={defaultOpen} onOpenChange={onOpenChange}>
			<DropdownMenuTrigger asChild>
				<Button
					ref={triggerRef}
					type="button"
					variant="ghost"
					size="icon-sm"
					className="action-menu-trigger"
					aria-label={triggerLabel}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent
				className="action-menu-content min-w-40"
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
				<DropdownMenuGroup>
					{items.map((item) => (
						<Fragment key={item.id}>
							{item.separatorBefore ? <DropdownMenuSeparator /> : null}
							<DropdownMenuItem
								variant={item.destructive ? "destructive" : "default"}
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
							</DropdownMenuItem>
						</Fragment>
					))}
				</DropdownMenuGroup>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}
