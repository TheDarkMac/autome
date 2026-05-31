import { useCallback, useEffect, useMemo, useState } from "react";
import { apiBaseUrl, fetchOpenApi, type OpenApiSpec } from "./api";
import { createApiClient } from "./apiClient";
import { fetchCatalog, STATIC_CATALOG, type CatalogItem } from "./catalog";
import { ConnectorsPanel } from "./ConnectorsPanel";
import type { CanvasNode, WireGraph } from "./graph";
import { LoginPage } from "./LoginPage";
import { NodeCanvas, type NodeCanvasController } from "./NodeCanvas";
import { NodePanel } from "./NodePanel";
import { Palette } from "./Palette";
import type { ParamValues } from "./paramSchema";
import { RunsList } from "./RunsList";
import { RunExecutionPanel } from "./RunExecutionPanel";
import {
  nodeExecutionStatus,
  type NodeExecStatus,
  type RunCheckpoint,
} from "./runs";
import { useRunPolling } from "./useRunPolling";
import { useSession, type UseSessionOptions } from "./useSession";

type Status =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; spec: OpenApiSpec }
  | { kind: "error"; message: string };

type View = "edit" | "runs" | "connectors";

export interface AppProps {
  /** Options de session (store/impls injectables pour les tests). */
  sessionOptions?: UseSessionOptions;
}

// Workflow courant : tant que F8.7 (persistence backend) n'est pas en place,
// on opère sur un brouillon local ; la liste des runs interroge ce même
// `workflowId` côté backend.
const DRAFT_WORKFLOW_ID = "wf-draft";

export function App({ sessionOptions }: AppProps = {}): JSX.Element {
  const { session, loading: sessionLoading, login, signup, logout, authFetch } =
    useSession(sessionOptions);

  // Client API typé adossé à l'`authFetch` (F8.7) — identité stable car
  // `authFetch` l'est ; les vues de runs y branchent leurs lectures.
  const api = useMemo(() => createApiClient(authFetch), [authFetch]);
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [catalog, setCatalog] = useState<readonly CatalogItem[]>(STATIC_CATALOG);
  const [catalogLoading, setCatalogLoading] = useState(true);
  const [selectedNode, setSelectedNode] = useState<CanvasNode | null>(null);
  const [controller, setController] = useState<NodeCanvasController | null>(null);
  const [view, setView] = useState<View>("edit");
  const [graph, setGraph] = useState<WireGraph | null>(null);
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

  // Polling du run sélectionné (F8.5.d). Inactif quand on est en mode édition.
  const { checkpoint, error: pollError } = useRunPolling(
    view === "runs" ? selectedRunId : null,
    { fetchImpl: api.getRun },
  );

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

  const handlePanelChange = useCallback(
    (nodeId: string, params: ParamValues) => {
      controller?.patchNodeParams(nodeId, params);
      setSelectedNode((prev) =>
        prev && prev.id === nodeId
          ? { ...prev, data: { ...prev.data, params } }
          : prev,
      );
    },
    [controller],
  );

  const executionStatuses = useMemo(
    () =>
      view === "runs" && checkpoint
        ? computeStatuses(checkpoint, graph)
        : undefined,
    [view, checkpoint, graph],
  );

  // Garde d'authentification (F8.6.b) — on attend le chargement du store
  // avant de décider, pour éviter un flash de la page de login.
  if (sessionLoading) {
    return (
      <main className="flex h-screen items-center justify-center bg-neutral-50">
        <p role="status" className="text-sm text-neutral-500">
          Chargement…
        </p>
      </main>
    );
  }
  if (!session) {
    return <LoginPage login={login} signup={signup} />;
  }

  return (
    <main className="flex h-screen flex-col bg-neutral-50">
      <header className="flex items-start justify-between border-b border-neutral-200 bg-white px-6 py-4 sm:px-10">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Autome</h1>
          <p className="mt-1 text-sm text-neutral-500">
            Éditeur visuel de workflows — F8.6 authentification & connecteurs.
          </p>
        </div>
        <div className="flex items-center gap-3 text-sm">
          <span className="text-neutral-500" data-testid="current-user">
            {session.userId}
          </span>
          <button
            type="button"
            onClick={logout}
            className="rounded-md border border-neutral-200 px-3 py-1.5 text-neutral-700 hover:bg-neutral-50"
          >
            Se déconnecter
          </button>
        </div>
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

      <ViewTabs view={view} onChange={setView} />

      {view === "connectors" ? (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <ConnectorsPanel accessToken={session.accessToken} />
        </div>
      ) : (
        <div className="flex min-h-0 flex-1">
          {view === "edit" ? (
            <Palette items={catalog} loading={catalogLoading} />
          ) : (
            <RunsList
              workflowId={DRAFT_WORKFLOW_ID}
              selectedRunId={selectedRunId}
              onSelectRun={setSelectedRunId}
              fetchImpl={api.listRuns}
            />
          )}
          <section className="flex min-h-0 flex-1">
            <NodeCanvas
              onChange={setGraph}
              onSelectionChange={setSelectedNode}
              onReady={setController}
              executionStatuses={executionStatuses}
              workflowId={DRAFT_WORKFLOW_ID}
            />
          </section>
          {view === "edit" ? (
            <NodePanel
              node={selectedNode}
              onChange={handlePanelChange}
              onClose={() => setSelectedNode(null)}
            />
          ) : (
            <RunExecutionPanel
              checkpoint={pollError ? null : checkpoint}
              node={selectedNode}
              edges={graph?.edges ?? []}
              onClose={() => setSelectedNode(null)}
            />
          )}
        </div>
      )}
    </main>
  );
}

function computeStatuses(
  ck: RunCheckpoint,
  graph: WireGraph | null,
): Readonly<Record<string, NodeExecStatus>> {
  const ids = new Set<string>(graph?.nodes.map((n) => n.id) ?? []);
  for (const id of Object.keys(ck.outputs)) ids.add(id);
  if (ck.next_node) ids.add(ck.next_node);
  const out: Record<string, NodeExecStatus> = {};
  for (const id of ids) out[id] = nodeExecutionStatus(ck, id);
  return out;
}

function ViewTabs({
  view,
  onChange,
}: {
  view: View;
  onChange: (v: View) => void;
}): JSX.Element {
  return (
    <nav
      role="tablist"
      aria-label="Vues"
      className="flex gap-1 border-b border-neutral-200 bg-white px-6 sm:px-10"
    >
      <ViewTab
        active={view === "edit"}
        label="Éditeur"
        onClick={() => onChange("edit")}
      />
      <ViewTab
        active={view === "runs"}
        label="Exécutions"
        onClick={() => onChange("runs")}
      />
      <ViewTab
        active={view === "connectors"}
        label="Connecteurs"
        onClick={() => onChange("connectors")}
      />
    </nav>
  );
}

function ViewTab({
  active,
  label,
  onClick,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className={`-mb-px border-b-2 px-3 py-2 text-sm transition ${
        active
          ? "border-sky-500 text-sky-700"
          : "border-transparent text-neutral-500 hover:text-neutral-700"
      }`}
    >
      {label}
    </button>
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
