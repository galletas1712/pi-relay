import {
	memo,
	useCallback,
	useEffect,
	useLayoutEffect,
	useMemo,
	useRef,
	useState,
	useSyncExternalStore,
	type ClipboardEvent,
	type DragEvent,
	type KeyboardEvent,
	type RefObject,
} from "react";
import {
	Check,
	Edit3,
	GripVertical,
	ImagePlus,
	Loader2,
	RefreshCw,
	Send,
	ShipWheel,
	Square,
	Trash2,
	X,
} from "lucide-react";
import {
	Attachment,
	AttachmentAction,
	AttachmentActions,
	AttachmentContent,
	AttachmentDescription,
	AttachmentGroup,
	AttachmentMedia,
	AttachmentTitle,
} from "@/components/ui/attachment";
import type { ComposerSubmission } from "./composerRouting.ts";
import {
	composerTextNeedsConnection,
	ConnectionBlockedReason,
} from "./connectionRecovery.tsx";
import {
	buildUserContent,
	normalizeMimeType,
	prepareImageUpload,
	textBlocksToEditString,
	validateImageFiles,
} from "./imageContent.ts";
import type { ImageArtifactMetadata, ImageUploadInput } from "./agentApi.ts";
import { randomId } from "./ids.ts";
import {
	COMMANDS,
	filterCommands,
	matchSlashPrefix,
	type SlashCommandInfo,
} from "./slash.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { ContentBlock, QueuedInput } from "./types.ts";

const NEW_SESSION_DRAFT_ID = "__new_session__";
const COMPOSER_DRAFTS_STORAGE_KEY = "piRelayComposerDrafts:v1";
const COMPOSER_DRAFT_STORAGE_PREFIX = "piRelayComposerDraft:v2:";
const COMPOSER_MIN_HEIGHT_PX = 44;
const COMPOSER_MAX_HEIGHT_PX = 180;
const IMAGE_ACCEPT = "image/png,image/jpeg,image/gif,image/webp";

type ComposerSubmitShortcutEvent = Pick<
	KeyboardEvent<HTMLTextAreaElement>,
	"ctrlKey" | "key" | "metaKey"
>;
type DraftImage = {
	localId: string;
	file: File;
	previewUrl: string;
	mimeType: string;
	byteLength: number;
	status: "uploading" | "ready" | "failed";
	artifactId?: string;
	error?: string;
	generation: number;
};
type NewDraftImage = Omit<DraftImage, "generation">;

export type PendingSubmittedDraft = {
	value: string;
	attachments: ContentBlock[];
	images: DraftImage[];
	revision: number;
	newSessionSetupGeneration: number;
	clientControlId: string;
	newSessionId: string;
};

function revokeDraftImagesNotIn(
	images: DraftImage[],
	retained: DraftImage[],
): void {
	const retainedUrls = new Set(retained.map((image) => image.previewUrl));
	revokeDraftImages(
		images.filter((image) => !retainedUrls.has(image.previewUrl)),
	);
}

export class ComposerDraftStore {
	private readonly drafts: Map<string, string>;
	private readonly attachmentsBySession = new Map<string, DraftImage[]>();
	private readonly intentRevisions = new Map<string, number>();
	private readonly pendingSubmittedDrafts = new Map<
		string,
		PendingSubmittedDraft
	>();
	private readonly listeners = new Set<() => void>();
	private revision = 0;
	private uploadGeneration = 0;
	private disposed = false;

	constructor(private readonly storage: ComposerDraftStorage | null = browserStorage()) {
		this.drafts = loadComposerDrafts(storage);
	}

	subscribe = (listener: () => void): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	getSnapshot = (): number => this.revision;

	draft(sessionId: string | null): string {
		return this.drafts.get(composerDraftKey(sessionId)) ?? "";
	}

	attachments(sessionId: string | null): DraftImage[] {
		return this.attachmentsBySession.get(composerDraftKey(sessionId)) ?? [];
	}

	addAttachment(sessionId: string | null, image: NewDraftImage): void {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		this.advanceIntent(key);
		this.attachmentsBySession.set(key, [
			...(this.attachmentsBySession.get(key) ?? []),
			{ ...image, generation: 0 },
		]);
		this.persistCurrentDraft(key);
		this.emit();
	}

	removeAttachment(sessionId: string | null, localId: string): void {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		const current = this.attachmentsBySession.get(key) ?? [];
		const removed = current.find((image) => image.localId === localId);
		if (!removed) return;
		this.advanceIntent(key);
		const next = current.filter((image) => image.localId !== localId);
		if (next.length) this.attachmentsBySession.set(key, next);
		else this.attachmentsBySession.delete(key);
		revokeDraftImages([removed]);
		this.persistCurrentDraft(key);
		this.emit();
	}

	async uploadAttachment(
		sessionId: string | null,
		localId: string,
		uploadImage?: (input: ImageUploadInput) => Promise<ImageArtifactMetadata>,
	): Promise<void> {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		const image = this.attachmentsBySession
			.get(key)
			?.find((candidate) => candidate.localId === localId);
		if (!image) return;
		const generation = ++this.uploadGeneration;
		this.updateUpload(key, localId, image.generation, (current) => ({
			...current,
			generation,
			status: "uploading",
			error: undefined,
		}));
		try {
			const prepared = await prepareImageUpload(image.file);
			if (!uploadImage) throw new Error("image upload is unavailable");
			const uploaded = await uploadImage({
				mimeType: prepared.mimeType,
				data: prepared.data,
			});
			this.updateUpload(key, localId, generation, (current) => ({
				...current,
				status: "ready",
				artifactId: uploaded.artifact_id,
				mimeType: uploaded.mime_type,
				byteLength: uploaded.byte_length,
				error: undefined,
			}));
		} catch (error) {
			this.updateUpload(key, localId, generation, (current) => ({
				...current,
				status: "failed",
				error: error instanceof Error ? error.message : String(error),
			}));
		}
	}

	setDraft(sessionId: string | null, value: string): void {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		this.advanceIntent(key);
		this.setDraftContents(key, value);
		this.emit();
	}

	beginSubmission(
		sessionId: string | null,
		rawDraft: string,
		attachments: ContentBlock[],
		newSessionSetupGeneration: number,
		createId: (prefix: string) => string = randomId,
	): PendingSubmittedDraft | null {
		if (this.disposed) return null;
		const key = composerDraftKey(sessionId);
		const previous = this.pendingSubmittedDrafts.get(key);
		if (
			previous &&
			!this.drafts.has(key) &&
			!(this.attachmentsBySession.get(key)?.length)
		) {
			return null;
		}
		const ids = submissionIdsForDraft(
			previous,
			rawDraft,
			newSessionSetupGeneration,
			createId,
			attachments,
		);
		const pending: PendingSubmittedDraft = {
			value: rawDraft,
			attachments,
			images: [...(this.attachmentsBySession.get(key) ?? [])],
			revision: this.intentRevisions.get(key) ?? 0,
			newSessionSetupGeneration,
			...ids,
		};
		this.drafts.delete(key);
		saveComposerDraft(key, rawDraft, this.storage);
		this.attachmentsBySession.delete(key);
		this.pendingSubmittedDrafts.set(key, pending);
		this.emit();
		return pending;
	}

	settleSubmission(
		sessionId: string | null,
		submission: PendingSubmittedDraft,
		accepted: boolean,
	): void {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		const pending = this.pendingSubmittedDrafts.get(key);
		if (pending !== submission) return;
		const currentImages = this.attachmentsBySession.get(key) ?? [];
		const canRestore =
			(this.intentRevisions.get(key) ?? 0) === pending.revision &&
			!this.drafts.has(key) &&
			currentImages.length === 0;
		if (!accepted && canRestore) {
			this.setDraftContents(key, pending.value);
			if (pending.images.length) {
				this.attachmentsBySession.set(key, pending.images);
			}
			this.emit();
			return;
		}
		this.pendingSubmittedDrafts.delete(key);
		this.persistCurrentDraft(key);
		revokeDraftImagesNotIn(pending.images, currentImages);
		this.emit();
	}

	clear(sessionId: string | null): void {
		if (this.disposed) return;
		const key = composerDraftKey(sessionId);
		const pending = this.pendingSubmittedDrafts.get(key);
		this.pendingSubmittedDrafts.delete(key);
		this.intentRevisions.set(key, (this.intentRevisions.get(key) ?? 0) + 1);
		this.setDraftContents(key, "");
		revokeDraftImages([
			...(this.attachmentsBySession.get(key) ?? []),
			...(pending?.images ?? []),
		]);
		this.attachmentsBySession.delete(key);
		this.emit();
	}

	dispose(): void {
		if (this.disposed) return;
		this.disposed = true;
		revokeDraftImages([
			...[...this.attachmentsBySession.values()].flat(),
			...[...this.pendingSubmittedDrafts.values()].flatMap(
				(pending) => pending.images,
			),
		]);
		this.attachmentsBySession.clear();
		this.pendingSubmittedDrafts.clear();
		this.listeners.clear();
	}

	private advanceIntent(key: string): void {
		const pending = this.pendingSubmittedDrafts.get(key);
		this.pendingSubmittedDrafts.delete(key);
		this.intentRevisions.set(key, (this.intentRevisions.get(key) ?? 0) + 1);
		if (pending) {
			revokeDraftImagesNotIn(
				pending.images,
				this.attachmentsBySession.get(key) ?? [],
			);
		}
	}

	private setDraftContents(key: string, value: string): void {
		if (value) this.drafts.set(key, value);
		else this.drafts.delete(key);
		saveComposerDraft(key, value, this.storage);
	}

	private persistCurrentDraft(key: string): void {
		saveComposerDraft(key, this.drafts.get(key) ?? "", this.storage);
	}

	private updateUpload(
		key: string,
		localId: string,
		generation: number,
		change: (image: DraftImage) => DraftImage,
	): void {
		if (this.disposed) return;
		const current = this.attachmentsBySession.get(key) ?? [];
		const index = current.findIndex(
			(image) =>
				image.localId === localId && image.generation === generation,
		);
		if (index < 0) return;
		const next = [...current];
		next[index] = change(next[index]!);
		this.attachmentsBySession.set(key, next);
		this.emit();
	}

	private emit(): void {
		this.revision += 1;
		for (const listener of this.listeners) listener();
	}
}

export function isComposerSubmitShortcut(
	event: ComposerSubmitShortcutEvent,
): boolean {
	return event.key === "Enter" && (event.metaKey || event.ctrlKey);
}

/**
 * Return the queue order produced by dropping `draggedId` on `targetId`.
 * A drop on a row inserts before that row, matching the row's top insertion marker.
 */
export function reorderQueuedInputsBefore(inputIds: string[], draggedId: string, targetId: string): string[] {
	const fromIndex = inputIds.indexOf(draggedId);
	const targetIndex = inputIds.indexOf(targetId);
	if (fromIndex < 0 || targetIndex < 0 || fromIndex === targetIndex) return inputIds;

	const nextOrder = [...inputIds];
	nextOrder.splice(fromIndex, 1);
	const insertionIndex = targetIndex > fromIndex ? targetIndex - 1 : targetIndex;
	nextOrder.splice(insertionIndex, 0, draggedId);
	return nextOrder;
}

export function submissionIdsForDraft(
	pending: PendingSubmittedDraft | undefined,
	text: string,
	newSessionSetupGeneration: number,
	createId: (prefix: string) => string = randomId,
	attachments: ContentBlock[] = [],
): Pick<PendingSubmittedDraft, "clientControlId" | "newSessionId"> {
	if (
		pending?.value === text &&
		JSON.stringify(pending.attachments) === JSON.stringify(attachments) &&
		pending.newSessionSetupGeneration === newSessionSetupGeneration
	) {
		return {
			clientControlId: pending.clientControlId,
			newSessionId: pending.newSessionId,
		};
	}
	return {
		clientControlId: createId("web_control"),
		newSessionId: createId("session"),
	};
}

export interface ComposerHandle {
	focus(): void;
	focusTarget(): HTMLElement | null;
	getValue(): string;
	setValue(value: string): void;
	setSessionDraft(sessionId: string | null, value: string): void;
	clearSession(sessionId: string | null): void;
}

export type ComposerDraftStorage = Storage;

export const Composer = memo(function Composer({
	selectedId,
	selectedIsSubagent,
	composerHandleRef,
	sending,
	canStop,
	stopping,
	queuedInputs,
	mutationBlockedReason,
	newSessionSetupGeneration = 0,
	uploadImage,
	onSubmit,
	onStop,
	onPromoteQueued,
	onUpdateQueued,
	onCancelQueued,
	onReorderQueued,
	storage,
	draftStore: externalDraftStore,
}: {
	selectedId: string | null;
	selectedIsSubagent: boolean;
	composerHandleRef: RefObject<ComposerHandle | null>;
	sending: boolean;
	canStop: boolean;
	stopping: boolean;
	queuedInputs: QueuedInput[];
	mutationBlockedReason?: string | null;
	newSessionSetupGeneration?: number;
	uploadImage?: (input: ImageUploadInput) => Promise<ImageArtifactMetadata>;
	onSubmit: (submission: ComposerSubmission) => Promise<boolean> | boolean;
	onStop: () => void;
	onPromoteQueued: (inputId: string) => void;
	onUpdateQueued: (inputId: string, text: string) => void;
	onCancelQueued: (inputId: string) => void;
	onReorderQueued: (inputIds: string[]) => void;
	storage?: ComposerDraftStorage;
	draftStore?: ComposerDraftStore;
}) {
	const textAreaRef = useRef<HTMLTextAreaElement | null>(null);
	const fileInputRef = useRef<HTMLInputElement | null>(null);
	const selectedIdRef = useRef<string | null>(selectedId);
	const internalDraftStore = useMemo(
		() => new ComposerDraftStore(storage),
		[storage],
	);
	const draftStore = externalDraftStore ?? internalDraftStore;
	const draftStoreRevision = useSyncExternalStore(
		draftStore.subscribe,
		draftStore.getSnapshot,
		draftStore.getSnapshot,
	);
	const [draft, setDraft] = useState(() => draftStore.draft(selectedId));
	const draftRef = useRef(draft);
	draftRef.current = draft;
	const [attachments, setAttachments] = useState<DraftImage[]>(() =>
		draftStore.attachments(selectedId),
	);
	const attachmentsRef = useRef(attachments);
	const [slashIndex, setSlashIndex] = useState(0);
	const [attachmentError, setAttachmentError] = useState<string | null>(null);

	const resizeComposer = useCallback(() => {
		const textArea = textAreaRef.current;
		if (!textArea) return;
		textArea.style.height = "auto";
		const nextHeight = Math.min(
			Math.max(textArea.scrollHeight, COMPOSER_MIN_HEIGHT_PX),
			COMPOSER_MAX_HEIGHT_PX,
		);
		textArea.style.height = `${nextHeight}px`;
		textArea.style.overflowY =
			textArea.scrollHeight > COMPOSER_MAX_HEIGHT_PX ? "auto" : "hidden";
	}, []);

	const setSessionDraft = useCallback(
		(sessionId: string | null, value: string) => {
			if (selectedIdRef.current === sessionId) {
				draftRef.current = value;
				setDraft(value);
			}
			draftStore.setDraft(sessionId, value);
		},
		[draftStore],
	);

	const setDraftValue = useCallback(
		(value: string) => {
			draftRef.current = value;
			setDraft(value);
			draftStore.setDraft(selectedIdRef.current, value);
		},
		[draftStore],
	);

	useLayoutEffect(() => {
		selectedIdRef.current = selectedId;
		const nextDraft = draftStore.draft(selectedId);
		draftRef.current = nextDraft;
		setDraft(nextDraft);
		const nextAttachments = draftStore.attachments(selectedId);
		attachmentsRef.current = nextAttachments;
		setAttachments(nextAttachments);
		setAttachmentError(null);
	}, [draftStore, selectedId]);

	useLayoutEffect(() => {
		const nextDraft = draftStore.draft(selectedIdRef.current);
		draftRef.current = nextDraft;
		setDraft(nextDraft);
		const nextAttachments = draftStore.attachments(selectedIdRef.current);
		attachmentsRef.current = nextAttachments;
		setAttachments(nextAttachments);
	}, [draftStore, draftStoreRevision]);

	useEffect(() => {
		attachmentsRef.current = attachments;
	}, [attachments]);

	useEffect(() => {
		return () => {
			if (!externalDraftStore) internalDraftStore.dispose();
		};
	}, [externalDraftStore, internalDraftStore]);

	useEffect(() => {
		composerHandleRef.current = {
			focus: () => textAreaRef.current?.focus(),
			focusTarget: () => textAreaRef.current,
			getValue: () => draftRef.current,
			setValue: (value) => setDraftValue(value),
			setSessionDraft: (sessionId, value) => setSessionDraft(sessionId, value),
			clearSession: (sessionId) => {
				draftStore.clear(sessionId);
			},
		};
		return () => {
			if (composerHandleRef.current?.getValue() === draftRef.current) {
				composerHandleRef.current = null;
			}
		};
	}, [
		composerHandleRef,
		draftStore,
		setDraftValue,
		setSessionDraft,
	]);

	const slashState = useMemo<{
		visible: boolean;
		commands: typeof COMMANDS;
	}>(() => {
		const prefix = matchSlashPrefix(draft);
		if (prefix === null) return { visible: false, commands: [] };
		return { visible: true, commands: filterCommands(prefix) };
	}, [draft]);

	useEffect(() => {
		setSlashIndex(0);
	}, [slashState.commands, slashState.visible]);

	useLayoutEffect(() => {
		resizeComposer();
	}, [draft, resizeComposer]);

	useEffect(() => {
		const textArea = textAreaRef.current;
		const target = textArea?.parentElement;
		if (!target || typeof ResizeObserver === "undefined") return;
		const observer = new ResizeObserver(() => resizeComposer());
		observer.observe(target);
		return () => observer.disconnect();
	}, [resizeComposer]);

	const uploadDraftImage = useCallback(
		async (targetSessionId: string | null, localId: string) => {
			await draftStore.uploadAttachment(
				targetSessionId,
				localId,
				uploadImage,
			);
		},
		[draftStore, uploadImage],
	);

	const addImageBlocks = useCallback(
		async (files: File[]) => {
			if (!files.length) return;
			const targetSessionId = selectedIdRef.current;
			setAttachmentError(null);
			try {
				validateImageFiles(
					files,
					draftStore.attachments(targetSessionId),
				);
				for (const file of files) {
					const mimeType = normalizeMimeType(file.type);
					const image: NewDraftImage = {
						localId: randomId("image"),
						file,
						previewUrl: URL.createObjectURL(file),
						mimeType,
						byteLength: file.size,
						status: "uploading",
					};
					draftStore.addAttachment(targetSessionId, image);
					void uploadDraftImage(targetSessionId, image.localId);
				}
			} catch (error) {
				setAttachmentError(
					error instanceof Error ? error.message : String(error),
				);
			}
		},
		[draftStore, uploadDraftImage],
	);

	const removeAttachment = useCallback(
		(localId: string) => {
			draftStore.removeAttachment(selectedIdRef.current, localId);
			setAttachmentError(null);
		},
		[draftStore],
	);

	const retryAttachment = useCallback(
		(image: DraftImage) => {
			setAttachmentError(null);
			void uploadDraftImage(selectedIdRef.current, image.localId);
		},
		[uploadDraftImage],
	);

	const onPaste = useCallback(
		(event: ClipboardEvent<HTMLTextAreaElement>) => {
			const files = [...(event.clipboardData?.files ?? [])];
			if (!files.length) return;
			event.preventDefault();
			void addImageBlocks(files);
		},
		[addImageBlocks],
	);

	const onDrop = useCallback(
		(event: DragEvent<HTMLDivElement>) => {
			event.preventDefault();
			const files = [...(event.dataTransfer?.files ?? [])];
			if (!files.length) return;
			void addImageBlocks(files);
		},
		[addImageBlocks],
	);

	const sendDraft = useCallback(async () => {
		const rawDraft = draftRef.current;
		const text = rawDraft.trim();
		const submittedImages = [...attachmentsRef.current];
		if (
			(!text && !submittedImages.length) ||
			sending ||
			(mutationBlockedReason && composerTextNeedsConnection(text))
		) {
			return;
		}
		const incomplete = submittedImages.find(
			(image) => image.status !== "ready" || !image.artifactId,
		);
		if (incomplete) {
			setAttachmentError(
				incomplete.status === "uploading"
					? "wait for image uploads to finish"
					: (incomplete.error ?? "retry or remove the failed image upload"),
			);
			return;
		}
		const submittedAttachments: ContentBlock[] = submittedImages.map(
			(image) => ({
				type: "image",
				artifact_id: image.artifactId!,
			}),
		);
		let content: ContentBlock[];
		try {
			content = buildUserContent(text, submittedAttachments);
		} catch (error) {
			setAttachmentError(
				error instanceof Error ? error.message : String(error),
			);
			return;
		}
		const submittedSessionId = selectedIdRef.current;
		const submittedSetupGeneration =
			submittedSessionId === null ? newSessionSetupGeneration : 0;
		const submission = draftStore.beginSubmission(
			submittedSessionId,
			rawDraft,
			submittedAttachments,
			submittedSetupGeneration,
			randomId,
		);
		if (!submission) return;
		setAttachmentError(null);
		requestAnimationFrame(() => {
			// A slash command can mount a modal before this callback runs.
			// Never pull focus back behind that modal.
			if (!document.querySelector('[role="dialog"], [role="alertdialog"]')) {
				textAreaRef.current?.focus();
			}
		});
		let accepted = false;
		try {
			accepted = await onSubmit({
				sessionId: submittedSessionId,
				text,
				content,
				clientControlId: submission.clientControlId,
				newSessionId: submission.newSessionId,
			});
		} finally {
			draftStore.settleSubmission(submittedSessionId, submission, accepted);
		}
	}, [
		draftStore,
		mutationBlockedReason,
		newSessionSetupGeneration,
		onSubmit,
		sending,
	]);

	const onKeyDown = useCallback(
		(event: KeyboardEvent<HTMLTextAreaElement>) => {
			if (slashState.visible && slashState.commands.length > 0) {
				if (event.key === "ArrowDown") {
					event.preventDefault();
					setSlashIndex((index) => (index + 1) % slashState.commands.length);
					return;
				}
				if (event.key === "ArrowUp") {
					event.preventDefault();
					setSlashIndex(
						(index) =>
							(index - 1 + slashState.commands.length) %
							slashState.commands.length,
					);
					return;
				}
				if (event.key === "Tab") {
					event.preventDefault();
					const command =
						slashState.commands[
							Math.min(slashIndex, slashState.commands.length - 1)
						];
					setDraftValue(`/${command.name} `);
					return;
				}
			}
			if (isComposerSubmitShortcut(event)) {
				event.preventDefault();
				if (slashState.visible && slashState.commands.length > 0) {
					const command =
						slashState.commands[
							Math.min(slashIndex, slashState.commands.length - 1)
						];
					const typedCommand = matchSlashPrefix(draftRef.current) ?? "";
					if (command.name !== typedCommand) {
						setDraftValue(`/${command.name} `);
						return;
					}
				}
				void sendDraft();
			}
		},
		[
			sendDraft,
			setDraftValue,
			slashIndex,
			slashState.commands,
			slashState.visible,
		],
	);

	const canSend =
		(!!draft.trim() || attachments.length > 0) &&
		attachments.every((image) => image.status === "ready");

	return (
		<div
			className="composer-wrap"
			onDragOver={(event) => {
				if ([...(event.dataTransfer?.types ?? [])].includes("Files"))
					event.preventDefault();
			}}
			onDrop={onDrop}
		>
			<SlashMenu
				commands={slashState.commands}
				visible={slashState.visible}
				selectedIndex={slashIndex}
				mutationBlockedReason={mutationBlockedReason}
				onSetIndex={setSlashIndex}
				onSelect={(command) => setDraftValue(`/${command.name} `)}
			/>
			<QueuedInputPane
				inputs={queuedInputs}
				visible={queuedInputs.length > 0 && !slashState.visible}
				mutationBlockedReason={mutationBlockedReason}
				onPromote={onPromoteQueued}
				onUpdate={onUpdateQueued}
				onCancel={onCancelQueued}
				onReorder={onReorderQueued}
			/>
			{attachments.length > 0 ? (
				<AttachmentGroup className="composer-attachments">
					{attachments.map((image) => (
						<ComposerAttachmentChip
							key={image.localId}
							image={image}
							disabled={false}
							onRetry={() => retryAttachment(image)}
							onRemove={() => removeAttachment(image.localId)}
						/>
					))}
				</AttachmentGroup>
			) : null}
			{attachmentError ? (
				<div className="composer-attachment-error" role="alert">
					{attachmentError}
				</div>
			) : null}
			<textarea
				ref={textAreaRef}
				value={draft}
				onChange={(event) => setDraftValue(event.target.value)}
				onKeyDown={onKeyDown}
				onPaste={onPaste}
				placeholder={
					selectedIsSubagent
						? "Steer this subagent or type /"
						: selectedId
							? "Follow up with this session or type /"
							: "Create or select a session"
				}
				className="composer"
				rows={1}
				enterKeyHint="enter"
				title="Enter for newline. Cmd+Enter to send."
			/>
			<input
				ref={fileInputRef}
				type="file"
				accept={IMAGE_ACCEPT}
				multiple
				hidden
				onChange={(event) => {
					const files = [...(event.target.files ?? [])];
					event.target.value = "";
					void addImageBlocks(files);
				}}
			/>
			<button
				className="composer-attach-button"
				type="button"
				onClick={() => fileInputRef.current?.click()}
				title="attach image"
				aria-label="attach image"
			>
				<ImagePlus size={15} />
			</button>
			<button
				className="stop-button"
				type="button"
				onClick={onStop}
				disabled={!canStop || stopping || !!mutationBlockedReason}
				aria-busy={stopping}
				title="stop active turn"
				aria-label="stop active turn"
			>
				{stopping ? (
					<Loader2 className="spin" size={15} />
				) : (
					<Square size={14} />
				)}
			</button>
			<button
				className="send-button"
				type="button"
				onClick={() => void sendDraft()}
				disabled={
					sending ||
					!canSend ||
					(!!mutationBlockedReason && composerTextNeedsConnection(draft))
				}
				aria-busy={sending}
				title="send (Cmd+Enter)"
				aria-label="send message"
			>
				{sending ? <Loader2 className="spin" size={16} /> : <Send size={16} />}
			</button>
			<ConnectionBlockedReason
				reason={mutationBlockedReason}
				className="composer-blocked-reason"
			/>
		</div>
	);
});

function ComposerAttachmentChip({
	image,
	disabled,
	onRetry,
	onRemove,
}: {
	image: DraftImage;
	disabled: boolean;
	onRetry: () => void;
	onRemove: () => void;
}) {
	const label = image.mimeType.replace(/^image\//, "").toUpperCase();
	return (
		<Attachment size="xs" orientation="horizontal">
			<AttachmentMedia variant="image">
				<img src={image.previewUrl} alt={`${label} preview`} />
			</AttachmentMedia>
			<AttachmentContent>
				<AttachmentTitle>{label}</AttachmentTitle>
				<AttachmentDescription
					title={image.error}
					role="status"
					aria-live="polite"
				>
					{image.status === "uploading"
						? "uploading…"
						: image.status === "failed"
							? (image.error ?? "upload failed")
							: "ready"}
				</AttachmentDescription>
			</AttachmentContent>
			<AttachmentActions>
				{image.status === "failed" ? (
					<AttachmentAction
						type="button"
						onClick={onRetry}
						disabled={disabled}
						aria-label="retry image upload"
					>
						<RefreshCw />
					</AttachmentAction>
				) : null}
				<AttachmentAction
					type="button"
					onClick={onRemove}
					disabled={disabled}
					aria-label="remove attachment"
				>
					<X />
				</AttachmentAction>
			</AttachmentActions>
		</Attachment>
	);
}

export function QueuedInputPane({
	inputs,
	visible,
	mutationBlockedReason,
	onPromote,
	onUpdate,
	onCancel,
	onReorder,
}: {
	inputs: QueuedInput[];
	visible: boolean;
	mutationBlockedReason?: string | null;
	onPromote: (inputId: string) => void;
	onUpdate: (inputId: string, text: string) => void;
	onCancel: (inputId: string) => void;
	onReorder: (inputIds: string[]) => void;
}) {
	const [editingId, setEditingId] = useState<string | null>(null);
	const [editingText, setEditingText] = useState("");
	const [draggedId, setDraggedId] = useState<string | null>(null);
	const [dragOverId, setDragOverId] = useState<string | null>(null);
	const followUpIds = useMemo(
		() =>
			inputs
				.filter(
					(input) =>
						input.priority === "follow_up" && input.status === "queued",
				)
				.map((input) => input.input_id),
		[inputs],
	);
	const canReorder = followUpIds.length > 1;
	const submitReorder = useCallback(
		(nextOrder: string[]) => {
			if (nextOrder.every((inputId, index) => inputId === followUpIds[index])) return;
			onReorder(nextOrder);
		},
		[followUpIds, onReorder],
	);
	useEffect(() => {
		if (editingId && !inputs.some((input) => input.input_id === editingId)) {
			setEditingId(null);
			setEditingText("");
		}
	}, [editingId, inputs]);
	const reorderByKeyboard = useCallback(
		(inputId: string, direction: "up" | "down") => {
			const currentIndex = followUpIds.indexOf(inputId);
			const targetIndex = direction === "up" ? currentIndex - 1 : currentIndex + 1;
			if (currentIndex < 0 || targetIndex < 0 || targetIndex >= followUpIds.length) return;
			const nextOrder = [...followUpIds];
			[nextOrder[currentIndex], nextOrder[targetIndex]] = [nextOrder[targetIndex], nextOrder[currentIndex]];
			submitReorder(nextOrder);
		},
		[followUpIds, submitReorder],
	);
	const finishDrag = useCallback(() => {
		setDraggedId(null);
		setDragOverId(null);
	}, []);
	const handleDrop = useCallback(
		(inputId: string) => {
			if (!draggedId || draggedId === inputId) {
				finishDrag();
				return;
			}
			const nextOrder = reorderQueuedInputsBefore(followUpIds, draggedId, inputId);
			if (nextOrder === followUpIds) {
				finishDrag();
				return;
			}
			submitReorder(nextOrder);
			finishDrag();
		},
		[draggedId, finishDrag, followUpIds, submitReorder],
	);
	if (!visible) return null;
	return (
		<div className="queue-pane">
			<div className="queue-pane-head">
				<span>Queued messages</span>
				<code>{inputs.length}</code>
			</div>
			<ConnectionBlockedReason
				reason={mutationBlockedReason}
				className="queue-blocked-reason"
			/>
			<div className="queue-list">
				{inputs.map((input) => {
					const canPromote =
						input.priority === "follow_up" && input.status === "queued";
					const canMutate = canPromote;
					const isEditing = editingId === input.input_id;
					const preview = contentBlocksToText(input.content);
					const hasImages = input.content.some(
						(block) => block.type === "image",
					);
					return (
						<div
							className={`queue-row${canReorder ? " has-reorder-handle" : ""}${draggedId === input.input_id ? " is-dragging" : ""}${dragOverId === input.input_id ? " is-drag-over" : ""}`}
							key={input.input_id}
							onDragOver={(event) => {
								if (!draggedId || !canMutate || draggedId === input.input_id) return;
								event.preventDefault();
								event.dataTransfer.dropEffect = "move";
								setDragOverId(input.input_id);
							}}
							onDrop={(event) => {
								event.preventDefault();
								handleDrop(input.input_id);
							}}
						>
							{canReorder && canMutate ? (
								<button
									className="queue-drag-handle"
									type="button"
									draggable={!mutationBlockedReason}
									onDragStart={(event) => {
										if (mutationBlockedReason) {
											event.preventDefault();
											return;
										}
										event.dataTransfer.effectAllowed = "move";
										event.dataTransfer.setData("text/plain", input.input_id);
										setDraggedId(input.input_id);
										setDragOverId(null);
									}}
									onDragEnd={finishDrag}
									onKeyDown={(event) => {
										if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return;
										event.preventDefault();
										if (!mutationBlockedReason) {
											reorderByKeyboard(input.input_id, event.key === "ArrowUp" ? "up" : "down");
										}
									}}
									disabled={!!mutationBlockedReason}
									title="Drag to reorder queued follow-up"
									aria-label={`Reorder queued follow-up: ${truncate(firstLine(preview) || "(empty)", 48)}`}
								>
									<GripVertical size={15} aria-hidden />
								</button>
							) : canReorder ? (
								<span className="queue-drag-handle-slot" aria-hidden />
							) : null}
							{isEditing ? (
								<textarea
									className="queue-edit"
									value={editingText}
									onChange={(event) => setEditingText(event.target.value)}
									rows={Math.min(
										4,
										Math.max(2, editingText.split("\n").length),
									)}
									autoFocus
								/>
							) : (
								<span className="queue-preview">
									{truncate(firstLine(preview) || "(empty)", 96)}
								</span>
							)}
							{isEditing ? (
								<div className="queue-actions">
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											const nextText = editingText.trim();
											if (!nextText && !hasImages) return;
											onUpdate(input.input_id, nextText);
											setEditingId(null);
										}}
										disabled={
											(!editingText.trim() && !hasImages) ||
											!!mutationBlockedReason
										}
										title="save queued message"
										aria-label="save queued message"
									>
										<Check size={13} />
									</button>
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											setEditingId(null);
											setEditingText("");
										}}
										title="cancel edit"
										aria-label="cancel edit"
									>
										<X size={13} />
									</button>
								</div>
							) : (
								<div className="queue-actions">
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											setEditingId(input.input_id);
											setEditingText(textBlocksToEditString(input.content));
										}}
										disabled={!canMutate}
										title={
											canMutate
												? "edit queued follow-up"
												: "steering messages cannot be edited"
										}
										aria-label="edit queued follow-up"
									>
										<Edit3 size={13} />
									</button>
									<button
										className="queue-icon-button destructive"
										type="button"
										onClick={() => onCancel(input.input_id)}
										disabled={!canMutate || !!mutationBlockedReason}
										title={
											canMutate
												? "delete queued follow-up"
												: "steering messages cannot be deleted here"
										}
										aria-label="delete queued follow-up"
									>
										<Trash2 size={13} />
									</button>
								</div>
							)}
							<button
								className="queue-steer-button"
								type="button"
								onClick={() => onPromote(input.input_id)}
								disabled={!canPromote || !!mutationBlockedReason}
								title={canPromote ? "promote to steer" : "already steering"}
								aria-label={
									canPromote ? "promote to steer" : "already steering"
								}
							>
								<ShipWheel size={15} />
							</button>
						</div>
					);
				})}
			</div>
		</div>
	);
}

function revokeDraftImages(images: DraftImage[]): void {
	for (const previewUrl of new Set(images.map((image) => image.previewUrl))) {
		URL.revokeObjectURL(previewUrl);
	}
}

export function composerDraftKey(sessionId: string | null): string {
	return sessionId ?? NEW_SESSION_DRAFT_ID;
}

export function loadComposerDrafts(
	storage = browserStorage(),
): Map<string, string> {
	const drafts = new Map<string, string>();
	if (!storage) return drafts;
	try {
		for (let index = 0; index < storage.length; index += 1) {
			const storageKey = storage.key(index);
			if (!storageKey?.startsWith(COMPOSER_DRAFT_STORAGE_PREFIX)) continue;
			const value = storage.getItem(storageKey);
			if (value?.trim()) {
				drafts.set(
					storageKey.slice(COMPOSER_DRAFT_STORAGE_PREFIX.length),
					value,
				);
			}
		}
	} catch {
		return new Map();
	}
	return drafts;
}

export function saveComposerDrafts(
	drafts: Map<string, string>,
	storage = browserStorage(),
): void {
	if (!storage) return;
	for (const [key, value] of drafts) saveComposerDraft(key, value, storage);
}

function saveComposerDraft(
	key: string,
	value: string,
	storage = browserStorage(),
): void {
	if (!storage) return;
	const storageKey = `${COMPOSER_DRAFT_STORAGE_PREFIX}${key}`;
	try {
		if (value.trim()) storage.setItem(storageKey, value);
		else storage.removeItem(storageKey);
	} catch {
		// localStorage can be unavailable or full; draft persistence is best-effort.
	}
}

function browserStorage(): Storage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage ?? null;
	} catch {
		return null;
	}
}

export { COMPOSER_DRAFTS_STORAGE_KEY, COMPOSER_DRAFT_STORAGE_PREFIX };

export function SlashMenu({
	commands,
	visible,
	selectedIndex,
	mutationBlockedReason,
	onSetIndex,
	onSelect,
}: {
	commands: typeof COMMANDS;
	visible: boolean;
	selectedIndex: number;
	mutationBlockedReason?: string | null;
	onSetIndex: (index: number) => void;
	onSelect: (command: SlashCommandInfo) => void;
}) {
	if (!visible || commands.length === 0) return null;
	return (
		<div className="slash-menu" role="listbox" aria-label="slash commands">
			{commands.map((command, index) => {
				const connectionRequired = composerTextNeedsConnection(
					`/${command.name}`,
				);
				return (
					<button
						type="button"
						key={command.name}
						className={`slash-row ${index === selectedIndex ? "selected" : ""}`}
						role="option"
						aria-selected={index === selectedIndex}
						disabled={!!mutationBlockedReason && connectionRequired}
						onMouseEnter={() => onSetIndex(index)}
						onMouseDown={(event) => {
							event.preventDefault();
							onSelect(command);
						}}
					>
						<span className="slash-name">
							/{command.name}
							{command.argumentHint ? (
								<small>{command.argumentHint}</small>
							) : null}
						</span>
						<span className="slash-description">{command.description}</span>
						{mutationBlockedReason && connectionRequired ? (
							<span className="slash-disabled-reason">
								{mutationBlockedReason}
							</span>
						) : null}
					</button>
				);
			})}
		</div>
	);
}
