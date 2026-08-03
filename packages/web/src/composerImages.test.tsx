// @vitest-environment jsdom

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { ImageArtifactMetadata } from "./agentApi.ts";
import {
  Composer,
  ComposerDraftStore,
  type ComposerHandle,
} from "./composer.tsx";
import {
  MAX_AGGREGATE_IMAGE_BYTES,
  MAX_IMAGE_BYTES,
  MAX_IMAGES_PER_CONTENT,
  prepareImageUpload,
  validateImageFiles,
} from "./imageContent.ts";

beforeAll(() => {
  class ResizeObserver {
    observe() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", ResizeObserver);
  vi.stubGlobal("URL", {
    ...URL,
    createObjectURL: vi.fn(() => "blob:preview"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  document.body.replaceChildren();
  window.localStorage.clear();
  vi.clearAllMocks();
});

describe("Composer image uploads", () => {
  it("keeps a late upload completion on the captured session draft", async () => {
    const upload = deferred<ImageArtifactMetadata>();
    const props = composerProps(vi.fn(() => upload.promise));
    const view = render(<Composer {...props} selectedId="session-a" />);

    addFile(view.container, pngFile());
    await screen.findByText("uploading…");
    view.rerender(<Composer {...props} selectedId="session-b" />);
    expect(screen.queryByText("uploading…")).toBeNull();

    upload.resolve(imageMetadata("a"));
    await waitFor(() => expect(screen.queryByText("ready")).toBeNull());
    view.rerender(<Composer {...props} selectedId="session-a" />);
    await screen.findByText("ready");
  });

  it("optimistically clears an image submission and restores it on failure", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const props = composerProps(
      async () => imageMetadata("b"),
      () => submit.promise,
    );
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("ready");
    const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
    await user.type(textbox, "inspect this");
    await user.keyboard("{Control>}{Enter}{/Control}");

    expect(textbox.readOnly).toBe(false);
    expect(textbox.value).toBe("");
    expect(screen.queryByText("ready")).toBeNull();
    submit.resolve(false);
    await waitFor(() => expect(textbox.value).toBe("inspect this"));
    expect(screen.getByText("ready")).toBeTruthy();
  });

  it("preserves exact raw whitespace while normalizing only the wire text", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => submit.promise);
    const props = composerProps(async () => imageMetadata("w"), onSubmit);
    render(<Composer {...props} selectedId="session-a" />);
    const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
    await user.type(textbox, "  inspect this  ");
    await user.keyboard("{Control>}{Enter}{/Control}");

    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        text: "inspect this",
        content: [{ type: "text", text: "inspect this" }],
      }),
    );
    expect(textbox.value).toBe("");
    submit.resolve(false);
    await waitFor(() => expect(textbox.value).toBe("  inspect this  "));
  });

  it("keeps a successful text-only send optimistically clear and editable", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const onSubmit = vi.fn(() => submit.promise);
    const props = composerProps(async () => imageMetadata("s"), onSubmit);
    render(<Composer {...props} selectedId="session-a" />);
    const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
    await user.type(textbox, "  submitted exactly  ");
    await user.keyboard("{Control>}{Enter}{/Control}");

    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        text: "submitted exactly",
        content: [{ type: "text", text: "submitted exactly" }],
      }),
    );
    expect(textbox.value).toBe("");
    expect(textbox.readOnly).toBe(false);
    await user.type(textbox, "next draft");
    submit.resolve(true);
    await waitFor(() => expect(textbox.value).toBe("next draft"));
  });

  it("does not overwrite a newer draft when an image submission fails", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const props = composerProps(
      async () => imageMetadata("c"),
      () => submit.promise,
    );
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("ready");
    const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
    await user.type(textbox, "submitted");
    await user.keyboard("{Control>}{Enter}{/Control}");
    await user.type(textbox, "newer draft");

    submit.resolve(false);
    await waitFor(() => expect(textbox.value).toBe("newer draft"));
    expect(screen.queryByText("ready")).toBeNull();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:preview");
  });

  it.each([
    ["accepted", true],
    ["failed", false],
  ])(
    "does not resurrect submitted text when an attachment-only next draft is %s",
    async (_outcome, accepted) => {
      const submit = deferred<boolean>();
      const user = userEvent.setup();
      const store = new ComposerDraftStore(window.localStorage);
      const props = {
        ...composerProps(
          async () => imageMetadata("n"),
          () => submit.promise,
        ),
        draftStore: store,
      };
      const first = render(<Composer {...props} selectedId="session-a" />);
      const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
      await user.type(textbox, "old submitted text");
      await user.keyboard("{Control>}{Enter}{/Control}");
      expect(
        window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
      ).toBe("old submitted text");

      addFile(first.container, pngFile());
      await screen.findByText("ready");
      expect(
        window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
      ).toBeNull();
      submit.resolve(accepted);
      await waitFor(() =>
        expect(
          window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
        ).toBeNull(),
      );

      first.unmount();
      render(<Composer {...props} selectedId="session-a" />);
      expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
      expect(screen.getByText("ready")).toBeTruthy();
      store.dispose();
    },
  );

  it("does not restore an obsolete submission after attachment replacement and removal", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(
        vi
          .fn()
          .mockResolvedValueOnce(imageMetadata("o"))
          .mockResolvedValueOnce(imageMetadata("p")),
        () => submit.promise,
      ),
      draftStore: store,
    };
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("ready");
    await user.type(screen.getByRole("textbox"), "obsolete submission");
    await user.keyboard("{Control>}{Enter}{/Control}");

    addFile(view.container, pngFile());
    await screen.findByText("ready");
    await user.click(screen.getByRole("button", { name: "remove attachment" }));
    submit.resolve(false);
    await waitFor(() =>
      expect(
        window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
      ).toBeNull(),
    );

    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
    expect(screen.queryByText("ready")).toBeNull();
    view.unmount();
    render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
    expect(screen.queryByText("ready")).toBeNull();
    store.dispose();
  });

  it("does not route a late image failure into a navigated session", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const props = composerProps(
      async () => imageMetadata("d"),
      () => submit.promise,
    );
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("ready");
    const textbox = screen.getByRole("textbox") as HTMLTextAreaElement;
    await user.type(textbox, "session a");
    await user.keyboard("{Control>}{Enter}{/Control}");
    view.rerender(<Composer {...props} selectedId="session-b" />);
    await user.type(textbox, "session b");

    submit.resolve(false);
    await waitFor(() => expect(textbox.value).toBe("session b"));
    view.rerender(<Composer {...props} selectedId="session-a" />);
    await waitFor(() => expect(textbox.value).toBe("session a"));
    expect(screen.getByText("ready")).toBeTruthy();
  });

  it("retains submission identity when the same image draft is retried", async () => {
    const user = userEvent.setup();
    const onSubmit = vi
      .fn()
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true);
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(async () => imageMetadata("r"), onSubmit),
      draftStore: store,
    };
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("ready");
    const localId = store.attachments("session-a")[0]!.localId;
    await user.type(screen.getByRole("textbox"), "  retry image  ");
    await user.keyboard("{Control>}{Enter}{/Control}");
    await screen.findByText("ready");
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe(
      "  retry image  ",
    );
    expect(store.attachments("session-a")[0]!.localId).toBe(localId);
    await user.keyboard("{Control>}{Enter}{/Control}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(2));

    const first = onSubmit.mock.calls[0]![0];
    const second = onSubmit.mock.calls[1]![0];
    expect(second.clientControlId).toBe(first.clientControlId);
    expect(second.newSessionId).toBe(first.newSessionId);
    expect(second.content).toEqual(first.content);
    await waitFor(() =>
      expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:preview"),
    );
    store.dispose();
  });

  it.each([
    ["drop", ""],
    ["drop", "text/plain"],
    ["paste", ""],
    ["paste", "text/plain"],
  ])(
    "cancels %s files with MIME %j and reports the validation error",
    async (source, type) => {
      const props = composerProps(async () => imageMetadata("e"));
      const view = render(<Composer {...props} selectedId="session-a" />);
      const file = new File(["not an image"], "bad.bin", { type });
      if (source === "drop") {
        const event = new Event("drop", { bubbles: true, cancelable: true });
        Object.defineProperty(event, "dataTransfer", {
          value: { files: [file], types: ["Files"] },
        });
        view.container.querySelector(".composer-wrap")!.dispatchEvent(event);
        expect(event.defaultPrevented).toBe(true);
      } else {
        const event = new Event("paste", { bubbles: true, cancelable: true });
        Object.defineProperty(event, "clipboardData", {
          value: { files: [file] },
        });
        screen.getByRole("textbox").dispatchEvent(event);
        expect(event.defaultPrevented).toBe(true);
      }
      await screen.findByRole("alert");
      expect(screen.getByRole("alert").textContent).toMatch(
        type ? /unsupported image MIME type/ : /MIME type is empty/,
      );
      expect(URL.createObjectURL).not.toHaveBeenCalled();
    },
  );

  it("revokes previews on unmount and ignores late upload completion", async () => {
    const upload = deferred<ImageArtifactMetadata>();
    const props = composerProps(() => upload.promise);
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await screen.findByText("uploading…");

    view.unmount();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:preview");
    upload.resolve(imageMetadata("f"));
    await Promise.resolve();
    expect(screen.queryByText("ready")).toBeNull();
  });

  it("settles upload success in an external store across Composer remount", async () => {
    const upload = deferred<ImageArtifactMetadata>();
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(() => upload.promise),
      draftStore: store,
    };
    const first = render(<Composer {...props} selectedId="session-a" />);
    addFile(first.container, pngFile());
    await screen.findByText("uploading…");
    first.unmount();

    upload.resolve(imageMetadata("u"));
    await waitFor(() =>
      expect(store.attachments("session-a")[0]?.status).toBe("ready"),
    );
    render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByText("ready")).toBeTruthy();
    store.dispose();
  });

  it("settles upload failure in an external store across Composer remount and allows retry", async () => {
    const upload = deferred<ImageArtifactMetadata>();
    const user = userEvent.setup();
    const uploadImage = vi
      .fn()
      .mockReturnValueOnce(upload.promise)
      .mockResolvedValueOnce(imageMetadata("v"));
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(uploadImage),
      draftStore: store,
    };
    const first = render(<Composer {...props} selectedId="session-a" />);
    addFile(first.container, pngFile());
    await screen.findByText("uploading…");
    first.unmount();

    upload.reject(new Error("late upload failed"));
    await waitFor(() =>
      expect(store.attachments("session-a")[0]?.status).toBe("failed"),
    );
    render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByText("late upload failed")).toBeTruthy();
    await user.click(
      screen.getByRole("button", { name: "retry image upload" }),
    );
    await screen.findByText("ready");
    store.dispose();
  });

  it("ignores stale upload generations after a newer retry and removal", async () => {
    const staleUpload = deferred<ImageArtifactMetadata>();
    const removedUpload = deferred<ImageArtifactMetadata>();
    const uploadImage = vi.fn(() => staleUpload.promise);
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(uploadImage),
      draftStore: store,
    };
    const view = render(<Composer {...props} selectedId="session-a" />);
    addFile(view.container, pngFile());
    await waitFor(() => expect(uploadImage).toHaveBeenCalledTimes(1));
    const localId = store.attachments("session-a")[0]!.localId;

    await store.uploadAttachment(
      "session-a",
      localId,
      async () => imageMetadata("w"),
    );
    expect(store.attachments("session-a")[0]?.artifactId).toBe(
      imageMetadata("w").artifact_id,
    );
    staleUpload.resolve(imageMetadata("x"));
    await staleUpload.promise;
    await Promise.resolve();
    expect(store.attachments("session-a")[0]?.artifactId).toBe(
      imageMetadata("w").artifact_id,
    );

    const removedSettlement = store.uploadAttachment(
      "session-a",
      localId,
      () => removedUpload.promise,
    );
    await waitFor(() =>
      expect(store.attachments("session-a")[0]?.status).toBe("uploading"),
    );
    store.removeAttachment("session-a", localId);
    removedUpload.resolve(imageMetadata("y"));
    await removedSettlement;

    expect(store.attachments("session-a")).toEqual([]);
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:preview");
    store.dispose();
  });

  it("settles a late success after unmount and remount without resurrecting persisted text", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(async () => imageMetadata("g"), () => submit.promise),
      draftStore: store,
    };
    const first = render(<Composer {...props} selectedId="session-a" />);
    await user.type(screen.getByRole("textbox"), "accepted once");
    await user.keyboard("{Control>}{Enter}{/Control}");
    expect(
      window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
    ).toBe("accepted once");
    first.unmount();

    const second = render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
    submit.resolve(true);
    await waitFor(() =>
      expect(
        window.localStorage.getItem("piRelayComposerDraft:v2:session-a"),
      ).toBeNull(),
    );
    second.unmount();
    render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
    store.dispose();
  });

  it("restores a late failure into the remounted originating session", async () => {
    const submit = deferred<boolean>();
    const user = userEvent.setup();
    const store = new ComposerDraftStore(window.localStorage);
    const props = {
      ...composerProps(async () => imageMetadata("h"), () => submit.promise),
      draftStore: store,
    };
    const first = render(<Composer {...props} selectedId="session-a" />);
    await user.type(screen.getByRole("textbox"), "restore after remount");
    await user.keyboard("{Control>}{Enter}{/Control}");
    first.unmount();

    render(<Composer {...props} selectedId="session-a" />);
    expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe("");
    submit.resolve(false);
    await waitFor(() =>
      expect(screen.getByRole<HTMLTextAreaElement>("textbox").value).toBe(
        "restore after remount",
      ),
    );
    store.dispose();
  });
});

describe("browser image validation", () => {
  it("rejects a declared MIME and signature mismatch", async () => {
    const file = pngFile("image/jpeg");
    await expect(prepareImageUpload(file)).rejects.toThrow(
      "declared image MIME type image/jpeg does not match image/png data",
    );
  });

  it("rejects the per-file byte limit before reading the file", () => {
    expect(() =>
      validateImageFiles([sizedFile(MAX_IMAGE_BYTES + 1)], []),
    ).toThrow(`image exceeds ${MAX_IMAGE_BYTES} decoded bytes`);
  });

  it("rejects the image count limit with generated metadata-only files", () => {
    expect(() =>
      validateImageFiles(
        Array.from(
          { length: MAX_IMAGES_PER_CONTENT + 1 },
          () => sizedFile(1),
        ),
        [],
      ),
    ).toThrow(`at most ${MAX_IMAGES_PER_CONTENT} images are allowed`);
  });

  it("rejects the aggregate byte limit with generated metadata-only files", () => {
    const each = Math.floor(MAX_AGGREGATE_IMAGE_BYTES / 3) + 1;
    expect(() =>
      validateImageFiles(
        [sizedFile(each), sizedFile(each), sizedFile(each)],
        [],
      ),
    ).toThrow(
      `aggregate image bytes exceed ${MAX_AGGREGATE_IMAGE_BYTES}`,
    );
  });
});

function composerProps(
  uploadImage: (input: {
    mimeType: string;
    data: string;
  }) => Promise<ImageArtifactMetadata>,
  onSubmit: () => Promise<boolean> | boolean = () => true,
) {
  return {
    selectedIsSubagent: false,
    composerHandleRef: createRef<ComposerHandle>(),
    sending: false,
    canStop: false,
    stopping: false,
    queuedInputs: [],
    uploadImage,
    onSubmit,
    onStop: () => undefined,
    onPromoteQueued: () => undefined,
    onUpdateQueued: () => undefined,
    onCancelQueued: () => undefined,
    onReorderQueued: () => undefined,
  };
}

function addFile(container: HTMLElement, file: File) {
  const input = container.querySelector<HTMLInputElement>('input[type="file"]');
  if (!input) throw new Error("file input missing");
  fireEvent.change(input, { target: { files: [file] } });
}

function pngFile(type = "image/png"): File {
  const bytes = Uint8Array.from([
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    0x49, 0x48, 0x44, 0x52,
  ]);
  const file = new File([bytes], "image.png", { type });
  Object.defineProperty(file, "arrayBuffer", {
    value: async () => bytes.buffer,
  });
  return file;
}

function sizedFile(size: number): File {
  return {
    size,
    type: "image/png",
  } as File;
}

function imageMetadata(digest: string): ImageArtifactMetadata {
  return {
    artifact_id: `sha256:${digest.repeat(64)}`,
    mime_type: "image/png",
    byte_length: 16,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}
