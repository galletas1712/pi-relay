import type { ContentBlock } from "./types.ts";

export const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
export const MAX_IMAGES_PER_CONTENT = 4;
export const MAX_AGGREGATE_IMAGE_BYTES = 10 * 1024 * 1024;

const ALLOWED_MIME_TYPES = [
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
] as const;
const ALLOWED_MIME_SET = new Set<string>(ALLOWED_MIME_TYPES);

export type ImageContentBlock = Extract<ContentBlock, { type: "image" }>;
export interface PreparedImageUpload {
  mimeType: string;
  data: string;
  byteLength: number;
}

export function normalizeMimeType(mimeType: string): string {
  if (!mimeType.trim()) throw new Error("image MIME type is empty");
  const lowered = mimeType.trim().toLowerCase();
  const normalized = lowered === "image/jpg" ? "image/jpeg" : lowered;
  if (!ALLOWED_MIME_SET.has(normalized)) {
    throw new Error(
      `unsupported image MIME type \`${mimeType}\`; allowed: ${ALLOWED_MIME_TYPES.join(", ")}`,
    );
  }
  return normalized;
}

export function validateImageFiles(
  files: File[],
  existing: Array<{ byteLength: number }>,
): void {
  const selected = files.map((file) => {
    normalizeMimeType(file.type);
    if (file.size === 0) throw new Error("image data is empty");
    if (file.size > MAX_IMAGE_BYTES) {
      throw new Error(`image exceeds ${MAX_IMAGE_BYTES} decoded bytes`);
    }
    return { byteLength: file.size };
  });
  assertDraftImageLimits([...existing, ...selected]);
}

export async function prepareImageUpload(
  file: File,
): Promise<PreparedImageUpload> {
  const mimeType = normalizeMimeType(file.type);
  if (file.size === 0) throw new Error("image data is empty");
  if (file.size > MAX_IMAGE_BYTES) {
    throw new Error(`image exceeds ${MAX_IMAGE_BYTES} decoded bytes`);
  }
  const bytes = new Uint8Array(await file.arrayBuffer());
  const sniffedMimeType = sniffImageMimeType(bytes);
  if (!sniffedMimeType)
    throw new Error("image signature is not PNG, JPEG, GIF, or WebP");
  if (sniffedMimeType !== mimeType) {
    throw new Error(
      `declared image MIME type ${mimeType} does not match ${sniffedMimeType} data`,
    );
  }
  const dataUrl = await readFileAsDataUrl(file);
  const comma = dataUrl.indexOf(",");
  const data = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
  if (!data) throw new Error("image data is empty");
  return { mimeType, data, byteLength: bytes.byteLength };
}

/** Optional trimmed text, then attachments. Rejects empty content. */
export function buildUserContent(
  text: string,
  attachments: ContentBlock[],
): ContentBlock[] {
  const content: ContentBlock[] = [];
  const trimmed = text.trim();
  if (trimmed) content.push({ type: "text", text: trimmed });
  for (const block of attachments) {
    if (block.type !== "image") {
      throw new Error("attachments must be image content blocks");
    }
    content.push(block);
  }
  if (content.length === 0) {
    throw new Error("message content is empty");
  }
  assertAttachmentLimits(
    content.filter(
      (block): block is ImageContentBlock => block.type === "image",
    ),
  );
  return content;
}

export function imageBlocksOf(content: ContentBlock[]): ImageContentBlock[] {
  return content.filter(
    (block): block is ImageContentBlock => block.type === "image",
  );
}

export function textBlocksToEditString(content: ContentBlock[]): string {
  return content
    .filter(
      (block): block is Extract<ContentBlock, { type: "text" }> =>
        block.type === "text",
    )
    .map((block) => block.text)
    .join("\n");
}

/** Replace text blocks with `text` while keeping image blocks in their relative order. */
export function replaceTextPreserveImages(
  content: ContentBlock[],
  text: string,
): ContentBlock[] {
  return buildUserContent(text, imageBlocksOf(content));
}

function assertAttachmentLimits(images: ImageContentBlock[]): void {
  if (images.length > MAX_IMAGES_PER_CONTENT) {
    throw new Error(`at most ${MAX_IMAGES_PER_CONTENT} images are allowed`);
  }
}

export function assertDraftImageLimits(
  images: Array<{ byteLength: number }>,
): void {
  if (images.length > MAX_IMAGES_PER_CONTENT) {
    throw new Error(`at most ${MAX_IMAGES_PER_CONTENT} images are allowed`);
  }
  const aggregate = images.reduce(
    (total, image) => total + image.byteLength,
    0,
  );
  if (aggregate > MAX_AGGREGATE_IMAGE_BYTES) {
    throw new Error(
      `aggregate image bytes exceed ${MAX_AGGREGATE_IMAGE_BYTES}`,
    );
  }
}

function sniffImageMimeType(bytes: Uint8Array): string | null {
  if (startsWith(bytes, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) {
    return "image/png";
  }
  if (startsWith(bytes, [0xff, 0xd8, 0xff])) return "image/jpeg";
  if (
    startsWith(bytes, [0x47, 0x49, 0x46, 0x38, 0x37, 0x61]) ||
    startsWith(bytes, [0x47, 0x49, 0x46, 0x38, 0x39, 0x61])
  ) {
    return "image/gif";
  }
  if (
    startsWith(bytes, [0x52, 0x49, 0x46, 0x46]) &&
    bytes.length >= 12 &&
    startsWith(bytes.subarray(8), [0x57, 0x45, 0x42, 0x50])
  ) {
    return "image/webp";
  }
  return null;
}

function startsWith(bytes: Uint8Array, signature: number[]): boolean {
  return signature.every((value, index) => bytes[index] === value);
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("failed to read image file"));
    reader.onload = () => {
      if (typeof reader.result !== "string") {
        reject(new Error("failed to read image file"));
        return;
      }
      resolve(reader.result);
    };
    reader.readAsDataURL(file);
  });
}
