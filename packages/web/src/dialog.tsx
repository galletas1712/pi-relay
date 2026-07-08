import * as AlertDialogPrimitive from "@radix-ui/react-alert-dialog";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import {
	createContext,
	useContext,
	useRef,
	type ComponentPropsWithoutRef,
	type ComponentPropsWithRef,
	type ReactNode,
	type RefObject,
} from "react";

type DialogKind = "dialog" | "alertdialog";

const DialogKindContext = createContext<DialogKind>("dialog");

interface AppDialogProps {
	children: ReactNode;
	className?: string;
	busy?: boolean;
	initialFocusRef?: RefObject<HTMLElement | null>;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
	onDismiss: () => void;
}

/**
 * Return focus only to connected, explicitly available elements. Attribute
 * checks are intentional: layout APIs are not reliable in jsdom and a
 * responsive off-canvas container already exposes its state with `inert`.
 */
export function isValidFocusReturnTarget(
	target: HTMLElement | null | undefined,
): target is HTMLElement {
	if (!target?.isConnected || target === document.body || target === document.documentElement) {
		return false;
	}
	for (let element: HTMLElement | null = target; element; element = element.parentElement) {
		if (
			element.hasAttribute("inert") ||
			element.hasAttribute("hidden") ||
			element.hasAttribute("disabled") ||
			element.getAttribute("aria-disabled")?.toLowerCase() === "true" ||
			element.getAttribute("aria-hidden")?.toLowerCase() === "true"
		) {
			return false;
		}
	}
	return true;
}

/**
 * Shared dismissal policy: idle dialogs close via Escape, their close controls,
 * or an outside pointer interaction. Busy dialogs reject all three.
 */
export function AppDialog(props: AppDialogProps) {
	return <DialogRoot kind="dialog" {...props} />;
}

/**
 * Alerts require an explicit Cancel/close choice (Radix blocks outside
 * interactions). Escape remains available while idle and is blocked while busy.
 */
export function AppAlertDialog(props: Omit<AppDialogProps, "initialFocusRef">) {
	return <DialogRoot kind="alertdialog" {...props} />;
}

function DialogRoot({
	kind,
	children,
	className,
	busy = false,
	initialFocusRef,
	returnFocusFallbackRef,
	onDismiss,
}: AppDialogProps & { kind: DialogKind }) {
	const returnFocusRef = useRef<HTMLElement | null | undefined>(undefined);
	if (returnFocusRef.current === undefined) {
		returnFocusRef.current =
			typeof document !== "undefined" && document.activeElement instanceof HTMLElement
				? document.activeElement
				: null;
	}
	const handleOpenChange = (open: boolean) => {
		if (!open && !busy) onDismiss();
	};
	const handleCloseAutoFocus = (event: Event) => {
		event.preventDefault();
		// Wait until Radix removes the modal's temporary aria-hidden markers
		// before validating application-owned return targets.
		queueMicrotask(() => {
			for (const target of [returnFocusRef.current, returnFocusFallbackRef?.current]) {
				if (!isValidFocusReturnTarget(target)) continue;
				target.focus();
				if (document.activeElement === target) return;
			}
		});
	};
	const handleEscapeKeyDown = (event: KeyboardEvent) => {
		event.stopPropagation();
		if (busy) event.preventDefault();
	};
	const handleOpenAutoFocus = initialFocusRef
		? (event: Event) => {
				event.preventDefault();
				initialFocusRef.current?.focus();
		  }
		: undefined;
	const contentClassName = ["dialog-content", className].filter(Boolean).join(" ");

	if (kind === "alertdialog") {
		return (
			<DialogKindContext.Provider value={kind}>
				<AlertDialogPrimitive.Root open onOpenChange={handleOpenChange}>
					<AlertDialogPrimitive.Portal>
						<AlertDialogPrimitive.Overlay className="dialog-overlay" />
						<AlertDialogPrimitive.Content
							className={contentClassName}
							onCloseAutoFocus={handleCloseAutoFocus}
							onEscapeKeyDown={handleEscapeKeyDown}
						>
							{children}
						</AlertDialogPrimitive.Content>
					</AlertDialogPrimitive.Portal>
				</AlertDialogPrimitive.Root>
			</DialogKindContext.Provider>
		);
	}

	return (
		<DialogKindContext.Provider value={kind}>
			<DialogPrimitive.Root open onOpenChange={handleOpenChange}>
				<DialogPrimitive.Portal>
					<DialogPrimitive.Overlay className="dialog-overlay" />
					<DialogPrimitive.Content
						className={contentClassName}
						onCloseAutoFocus={handleCloseAutoFocus}
						onEscapeKeyDown={handleEscapeKeyDown}
						onOpenAutoFocus={handleOpenAutoFocus}
						onPointerDownOutside={(event) => {
							if (busy) event.preventDefault();
						}}
						onInteractOutside={(event) => {
							if (busy) event.preventDefault();
						}}
					>
						{children}
					</DialogPrimitive.Content>
				</DialogPrimitive.Portal>
			</DialogPrimitive.Root>
		</DialogKindContext.Provider>
	);
}

export function DialogHeader({ children }: { children: ReactNode }) {
	return <div className="dialog-header">{children}</div>;
}

export function DialogHeading({ children }: { children: ReactNode }) {
	return <div className="dialog-heading">{children}</div>;
}

export function DialogTitle({
	children,
	...props
}: ComponentPropsWithRef<typeof DialogPrimitive.Title>) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogPrimitive.Title {...props}>{children}</AlertDialogPrimitive.Title>
	) : (
		<DialogPrimitive.Title {...props}>{children}</DialogPrimitive.Title>
	);
}

export function DialogDescription({
	children,
	...props
}: ComponentPropsWithoutRef<typeof DialogPrimitive.Description>) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogPrimitive.Description {...props}>{children}</AlertDialogPrimitive.Description>
	) : (
		<DialogPrimitive.Description {...props}>{children}</DialogPrimitive.Description>
	);
}

export function DialogBody({
	children,
	className,
}: {
	children: ReactNode;
	className?: string;
}) {
	return <div className={["dialog-body", className].filter(Boolean).join(" ")}>{children}</div>;
}

export function DialogFooter({ children }: { children: ReactNode }) {
	return <div className="dialog-footer">{children}</div>;
}

export function DialogClose({
	children,
	...props
}: ComponentPropsWithoutRef<"button">) {
	const kind = useContext(DialogKindContext);
	const button = <button type="button" {...props}>{children}</button>;
	return kind === "alertdialog" ? (
		<AlertDialogPrimitive.Cancel asChild>{button}</AlertDialogPrimitive.Cancel>
	) : (
		<DialogPrimitive.Close asChild>{button}</DialogPrimitive.Close>
	);
}

export function DialogCloseButton({
	label,
	disabled = false,
}: {
	label: string;
	disabled?: boolean;
}) {
	return (
		<DialogClose className="plain-close-button" aria-label={label} disabled={disabled}>
			<X size={16} aria-hidden />
		</DialogClose>
	);
}
