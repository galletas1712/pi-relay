import { XIcon } from "lucide-react";
import {
	createContext,
	useContext,
	useRef,
	type ComponentPropsWithoutRef,
	type ComponentPropsWithRef,
	type ReactNode,
	type RefObject,
} from "react";
import {
	AlertDialog,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
	Dialog,
	DialogClose as ShadDialogClose,
	DialogContent,
	DialogDescription as ShadDialogDescription,
	DialogFooter as ShadDialogFooter,
	DialogHeader as ShadDialogHeader,
	DialogTitle as ShadDialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

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
export function AppAlertDialog(props: AppDialogProps) {
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
	const contentClassName = cn("dialog-content max-w-[min(760px,calc(100vw-2rem))]", className);

	if (kind === "alertdialog") {
		return (
			<DialogKindContext.Provider value={kind}>
				<AlertDialog open onOpenChange={handleOpenChange}>
					<AlertDialogContent
						className={contentClassName}
						size="default"
						onCloseAutoFocus={handleCloseAutoFocus}
						onEscapeKeyDown={handleEscapeKeyDown}
						onOpenAutoFocus={handleOpenAutoFocus}
					>
						{children}
					</AlertDialogContent>
				</AlertDialog>
			</DialogKindContext.Provider>
		);
	}

	return (
		<DialogKindContext.Provider value={kind}>
			<Dialog open onOpenChange={handleOpenChange}>
				<DialogContent
					showCloseButton={false}
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
				</DialogContent>
			</Dialog>
		</DialogKindContext.Provider>
	);
}

export function DialogHeader({ children, className }: { children: ReactNode; className?: string }) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogHeader className={cn("dialog-header", className)}>{children}</AlertDialogHeader>
	) : (
		<ShadDialogHeader className={cn("dialog-header", className)}>{children}</ShadDialogHeader>
	);
}

export function DialogHeading({ children }: { children: ReactNode }) {
	return <div className="dialog-heading">{children}</div>;
}

export function DialogTitle({
	children,
	className,
	...props
}: ComponentPropsWithRef<typeof ShadDialogTitle>) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogTitle className={className} {...props}>
			{children}
		</AlertDialogTitle>
	) : (
		<ShadDialogTitle className={className} {...props}>
			{children}
		</ShadDialogTitle>
	);
}

export function DialogDescription({
	children,
	className,
	...props
}: ComponentPropsWithoutRef<typeof ShadDialogDescription>) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogDescription className={className} {...props}>
			{children}
		</AlertDialogDescription>
	) : (
		<ShadDialogDescription className={className} {...props}>
			{children}
		</ShadDialogDescription>
	);
}

export function DialogBody({
	children,
	className,
}: {
	children: ReactNode;
	className?: string;
}) {
	return <div className={cn("dialog-body", className)}>{children}</div>;
}

export function DialogFooter({ children, className }: { children: ReactNode; className?: string }) {
	const kind = useContext(DialogKindContext);
	return kind === "alertdialog" ? (
		<AlertDialogFooter className={cn("dialog-footer", className)}>{children}</AlertDialogFooter>
	) : (
		<ShadDialogFooter className={cn("dialog-footer", className)}>{children}</ShadDialogFooter>
	);
}

export function DialogClose({
	children,
	className,
	...props
}: ComponentPropsWithRef<"button">) {
	const kind = useContext(DialogKindContext);
	if (kind === "alertdialog") {
		return (
			<AlertDialogCancel className={className} {...props}>
				{children}
			</AlertDialogCancel>
		);
	}
	return (
		<ShadDialogClose asChild>
			<button type="button" className={className} {...props}>
				{children}
			</button>
		</ShadDialogClose>
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
			<XIcon aria-hidden />
		</DialogClose>
	);
}
