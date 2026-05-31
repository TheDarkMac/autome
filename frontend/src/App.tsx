import { useEffect, useState } from "react";
import { apiBaseUrl, fetchOpenApi, type OpenApiSpec } from "./api";
import { fetchCatalog, STATIC_CATALOG, type CatalogItem } from "./catalog";
import { NodeCanvas } from "./NodeCanvas";
import { Palette } from "./Palette";

type Status =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; spec: OpenApiSpec }
  | { kind: "error"; message: string };

export function App(): JSX.Element {
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [catalog, setCatalog] = useState<readonly CatalogItem[]>(STATIC_CATALOG);
  const [catalogLoading, setCatalogLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setStatus({ kind: "loading" });
    fetchOpenApi()
      .then((spec) => {
        if (!cancelled) setStatus({ kind: "ok", spec });
      })
      .catch((e: Error) => {
        if (!cancelled) setStatus({ kind: "error", message: e.message });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setCatalogLoading(true);
    fetchCatalog().then((items) => {
      if (cancelled) return;
      setCatalog(items);
      setCatalogLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <main className="flex h-screen flex-col bg-neutral-50">
      <header className="border-b border-neutral-200 bg-white px-6 py-4 sm:px-10">
        <h1 className="text-2xl font-semibold tracking-tight">Autome</h1>
        <p className="mt-1 text-sm text-neutral-500">
          Éditeur visuel de workflows — F8.3 palette + canvas drag-and-drop.
        </p>
      </header>

      <section
        aria-labelledby="backend-status"
        className="border-b border-neutral-200 bg-white px-6 py-3 text-sm sm:px-10"
      >
        <span id="backend-status" className="font-medium">
          Backend
        </span>{" "}
        <span className="text-neutral-500">
          ({apiBaseUrl()}/openapi.json)
        </span>{" "}
        <BackendStatus status={status} />
      </section>

      <div className="flex min-h-0 flex-1">
        <Palette items={catalog} loading={catalogLoading} />
        <section className="flex min-h-0 flex-1">
          <NodeCanvas />
        </section>
      </div>
    </main>
  );
}

function BackendStatus({ status }: { status: Status }): JSX.Element {
  switch (status.kind) {
    case "idle":
    case "loading":
      return (
        <span className="text-neutral-700" role="status">
          chargement…
        </span>
      );
    case "ok": {
      const paths = Object.keys(status.spec.paths).length;
      return (
        <span className="text-emerald-700" role="status">
          OK · {status.spec.info.title} v{status.spec.info.version} ({paths}{" "}
          endpoint{paths > 1 ? "s" : ""})
        </span>
      );
    }
    case "error":
      return (
        <span className="text-rose-700" role="alert">
          erreur : {status.message}
        </span>
      );
  }
}
