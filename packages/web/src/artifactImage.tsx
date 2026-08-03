import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import type { ImageArtifact } from "./agentApi.ts";

type ArtifactLoader = (artifactId: string) => Promise<ImageArtifact>;
type ArtifactImageLoader = {
  load: ArtifactLoader;
};

const ArtifactLoaderContext = createContext<ArtifactImageLoader | null>(null);

export function createArtifactImageLoader(
  load: ArtifactLoader,
  maxEntries = 4,
): ArtifactImageLoader {
  const cache = new Map<string, ImageArtifact>();
  const inFlight = new Map<string, Promise<ImageArtifact>>();
  const read = (artifactId: string) => {
    const cached = cache.get(artifactId);
    if (cached) {
      cache.delete(artifactId);
      cache.set(artifactId, cached);
      return Promise.resolve(cached);
    }
    const pending = inFlight.get(artifactId);
    if (pending) return pending;
    const request = load(artifactId)
      .then((artifact) => {
        cache.set(artifactId, artifact);
        while (cache.size > maxEntries) {
          const oldest = cache.keys().next().value;
          if (oldest === undefined) break;
          cache.delete(oldest);
        }
        return artifact;
      })
      .finally(() => {
        if (inFlight.get(artifactId) === request) {
          inFlight.delete(artifactId);
        }
      });
    inFlight.set(artifactId, request);
    return request;
  };
  return { load: read };
}

export function ArtifactImageProvider({
  load,
  children,
}: {
  load: ArtifactLoader;
  children: ReactNode;
}) {
  const loader = useMemo(() => createArtifactImageLoader(load), [load]);

  return (
    <ArtifactLoaderContext.Provider value={loader}>
      {children}
    </ArtifactLoaderContext.Provider>
  );
}

export function ArtifactImage({
  artifactId,
  className = "content-block-image",
}: {
  artifactId: string;
  className?: string;
}) {
  const load = useContext(ArtifactLoaderContext);
  const [artifact, setArtifact] = useState<ImageArtifact | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let live = true;
    setArtifact(null);
    setError(null);
    if (!load) {
      setError("image loader unavailable");
      return () => {
        live = false;
      };
    }
    void load.load(artifactId).then(
      (value) => {
        if (live) setArtifact(value);
      },
      (reason) => {
        if (live)
          setError(reason instanceof Error ? reason.message : String(reason));
      },
    );
    return () => {
      live = false;
    };
  }, [artifactId, attempt, load]);

  if (error) {
    return (
      <span className="content-block-image-error" role="alert" title={error}>
        Image unavailable ({artifactId}){" "}
        <button type="button" onClick={() => setAttempt((value) => value + 1)}>
          Retry image
        </button>
      </span>
    );
  }
  if (!artifact) {
    return (
      <span className="content-block-image-loading" role="status">
        Loading image…
      </span>
    );
  }

  const dataUrl = `data:${artifact.mime_type};base64,${artifact.data}`;
  const extension =
    artifact.mime_type === "image/jpeg"
      ? "jpg"
      : artifact.mime_type.replace("image/", "");
  const digest = artifactId.replace(/^sha256:/, "").slice(0, 12);
  return (
    <span className="content-block-image-wrap">
      <img
        className={className}
        src={dataUrl}
        alt={`attached image (${artifact.mime_type})`}
      />
      <a
        className="content-block-image-download"
        href={dataUrl}
        download={`image-${digest}.${extension}`}
      >
        Download image
      </a>
    </span>
  );
}
