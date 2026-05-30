import { useEffect, useState } from "react";
import { apiBaseUrl, fetchOpenApi, type OpenApiSpec } from "./api";

type Status =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; spec: OpenApiSpec }
  | { kind: "error"; message: string };

/** Page d'accueil F8.1 — skeleton qui valide l'intégration backend. */
export function App(): JSX.Element {
  const [status, setStatus] = useState<Status>({ kind: "idle" });

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

  return (
    <main className="min-h-screen px-6 py-12 sm:px-12">
      <header className="mx-auto max-w-3xl">
        <h1 className="text-4xl font-semibold tracking-tight">Autome</h1>
        <p className="mt-2 text-neutral-600">
          Éditeur visuel de workflows — frontend en cours d'amorçage (F8.1).
        </p>
      </header>

      <section
        aria-labelledby="backend-status"
        className="mx-auto mt-10 max-w-3xl rounded-lg border border-neutral-200 bg-white p-6 shadow-sm"
      >
        <h2 id="backend-status" className="text-lg font-medium">
          Connexion backend
        </h2>
        <p className="mt-1 text-sm text-neutral-500">
          Cible : <code className="font-mono">{apiBaseUrl()}/openapi.json</code>
        </p>
        <BackendStatus status={status} />
      </section>
    </main>
  );
}

function BackendStatus({ status }: { status: Status }): JSX.Element {
  switch (status.kind) {
    case "idle":
    case "loading":
      return (
        <p className="mt-4 text-neutral-700" role="status">
          Chargement…
        </p>
      );
    case "ok": {
      const paths = Object.keys(status.spec.paths).length;
      return (
        <p className="mt-4 text-emerald-700" role="status">
          OK — <strong>{status.spec.info.title}</strong> v
          {status.spec.info.version} expose {paths} endpoint
          {paths > 1 ? "s" : ""}.
        </p>
      );
    }
    case "error":
      return (
        <p className="mt-4 text-rose-700" role="alert">
          Erreur : {status.message}
        </p>
      );
  }
}
