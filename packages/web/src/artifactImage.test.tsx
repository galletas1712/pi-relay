// @vitest-environment jsdom

import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ImageArtifact } from "./agentApi.ts";
import {
  ArtifactImage,
  ArtifactImageProvider,
  createArtifactImageLoader,
} from "./artifactImage.tsx";

afterEach(cleanup);

describe("artifact image loading", () => {
  it("deduplicates in-flight reads and evicts least-recently-used decoded data", async () => {
    const pending = deferred<ImageArtifact>();
    const load = vi.fn((artifactId: string) => {
      if (artifactId === "a") return pending.promise;
      return Promise.resolve(artifact(artifactId));
    });
    const loader = createArtifactImageLoader(load, 2);

    const first = loader.load("a");
    const second = loader.load("a");
    expect(load).toHaveBeenCalledTimes(1);
    pending.resolve(artifact("a"));
    await Promise.all([first, second]);
    await loader.load("b");
    await loader.load("c");
    await loader.load("a");
    expect(load).toHaveBeenCalledTimes(4);
  });

  it("evicts failures and exposes a visible retry that renders data and download", async () => {
    const load = vi
      .fn()
      .mockRejectedValueOnce(new Error("temporary read failure"))
      .mockResolvedValueOnce(artifact("a"));
    render(
      <ArtifactImageProvider load={load}>
        <ArtifactImage artifactId="sha256:aaaaaaaaaaaa" />
      </ArtifactImageProvider>,
    );

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Image unavailable",
    );
    fireEvent.click(screen.getByRole("button", { name: "Retry image" }));
    const image = await screen.findByRole("img", {
      name: "attached image (image/png)",
    });
    expect(image.getAttribute("src")).toBe("data:image/png;base64,encoded-a");
    const download = screen.getByRole("link", { name: "Download image" });
    expect(download.getAttribute("href")).toBe(
      "data:image/png;base64,encoded-a",
    );
    expect(download.getAttribute("download")).toBe("image-aaaaaaaaaaaa.png");
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("deduplicates two mounted renderers for the same artifact", async () => {
    const load = vi.fn(async () => artifact("a"));
    const view = render(
      <ArtifactImageProvider load={load}>
        <ArtifactImage artifactId="a" />
        <ArtifactImage artifactId="a" />
      </ArtifactImageProvider>,
    );

    await waitFor(() => expect(screen.getAllByRole("img")).toHaveLength(2));
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("coalesces concurrent retries from two renderers through the ordinary loader", async () => {
    const first = deferred<ImageArtifact>();
    const retry = deferred<ImageArtifact>();
    const load = vi
      .fn()
      .mockImplementationOnce(() => first.promise)
      .mockImplementationOnce(() => retry.promise);
    const view = render(
      <ArtifactImageProvider load={load}>
        <ArtifactImage artifactId="a" />
        <ArtifactImage artifactId="a" />
      </ArtifactImageProvider>,
    );

    first.reject(new Error("temporary read failure"));
    expect(await screen.findAllByRole("alert")).toHaveLength(2);
    const buttons = screen.getAllByRole("button", { name: "Retry image" });
    fireEvent.click(buttons[0]!);
    fireEvent.click(buttons[1]!);
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));

    retry.resolve(artifact("fresh"));
    await waitFor(() => expect(screen.getAllByRole("img")).toHaveLength(2));
    expect(
      screen
        .getAllByRole("img")
        .map((image) => image.getAttribute("src")),
    ).toEqual([
      "data:image/png;base64,encoded-fresh",
      "data:image/png;base64,encoded-fresh",
    ]);
    view.rerender(
      <ArtifactImageProvider load={load}>
        <ArtifactImage artifactId="a" />
        <ArtifactImage artifactId="a" />
        <ArtifactImage artifactId="a" />
      </ArtifactImageProvider>,
    );
    await waitFor(() => expect(screen.getAllByRole("img")).toHaveLength(3));
    expect(load).toHaveBeenCalledTimes(2);
  });
});

function artifact(id: string): ImageArtifact {
  return {
    artifact_id: id,
    mime_type: "image/png",
    byte_length: 9,
    data: `encoded-${id}`,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, reject, resolve };
}
