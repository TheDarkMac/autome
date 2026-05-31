// Liste paginée des runs d'un workflow (F8.5.a).
//
// Affiche un tableau (run_id court, statut, déclencheur, durée, date) et
// laisse le parent gérer la sélection. La pagination est offset/limit pure
// (pas de "next page token" backend) ; on cache la barre de pagination si
// la page tient en une seule fournée.

import { useCallback, useEffect, useState } from "react";

import {
  fetchRunsForWorkflow,
  formatDuration,
  formatTime,
  type RunCheckpoint,
  type RunListPage,
  type RunStatus,
} from "./runs";

export interface RunsListProps {
  workflowId: string;
  selectedRunId: string | null;
  onSelectRun: (runId: string | null) => void;
  pageSize?: number;
  /** Override pour tests. */
  fetchImpl?: typeof fetchRunsForWorkflow;
}

const DEFAULT_PAGE_SIZE = 20;

type Status =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; page: RunListPage }
  | { kind: "error"; message: string };

export function RunsList({
  workflowId,
  selectedRunId,
  onSelectRun,
  pageSize = DEFAULT_PAGE_SIZE,
  fetchImpl = fetchRunsForWorkflow,
}: RunsListProps): JSX.Element {
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [offset, setOffset] = useState(0);
  const [reloadTick, setReloadTick] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setStatus({ kind: "loading" });
    fetchImpl(workflowId, { offset, limit: pageSize })
      .then((page) => {
        if (!cancelled) setStatus({ kind: "ok", page });
      })
      .catch((e: Error) => {
        if (!cancelled) setStatus({ kind: "error", message: e.message });
      });
    return () => {
      cancelled = true;
    };
  }, [workflowId, offset, pageSize, reloadTick, fetchImpl]);

  // Reset de la pagination si le workflow change sous nos pieds.
  useEffect(() => {
    setOffset(0);
  }, [workflowId]);

  const refresh = useCallback(() => setReloadTick((t) => t + 1), []);

  return (
    <aside
      aria-label="Liste des runs"
      data-testid="runs-list"
      className="flex h-full w-80 shrink-0 flex-col border-r border-neutral-200 bg-white"
    >
      <header className="flex items-center justify-between border-b border-neutral-200 px-4 py-3">
        <div>
          <h2 className="text-sm font-semibold text-neutral-800">Exécutions</h2>
          <p className="mt-0.5 truncate text-xs text-neutral-500">
            workflow <code>{workflowId}</code>
          </p>
        </div>
        <button
          type="button"
          onClick={refresh}
          aria-label="Rafraîchir la liste"
          className="rounded-md border border-neutral-200 px-2 py-0.5 text-xs text-neutral-600 hover:bg-neutral-50"
        >
          ↻
        </button>
      </header>

      <RunsListBody
        status={status}
        selectedRunId={selectedRunId}
        onSelectRun={onSelectRun}
      />

      <Pagination
        status={status}
        offset={offset}
        pageSize={pageSize}
        onOffsetChange={setOffset}
      />
    </aside>
  );
}

function RunsListBody({
  status,
  selectedRunId,
  onSelectRun,
}: {
  status: Status;
  selectedRunId: string | null;
  onSelectRun: (runId: string | null) => void;
}): JSX.Element {
  if (status.kind === "idle" || status.kind === "loading") {
    return (
      <p className="px-4 py-3 text-sm text-neutral-500" role="status">
        Chargement des runs…
      </p>
    );
  }
  if (status.kind === "error") {
    return (
      <p className="px-4 py-3 text-sm text-rose-700" role="alert">
        Erreur : {status.message}
      </p>
    );
  }
  if (status.page.items.length === 0) {
    return (
      <p className="px-4 py-3 text-sm text-neutral-500" role="status">
        Aucun run pour ce workflow.
      </p>
    );
  }
  return (
    <ul className="flex-1 divide-y divide-neutral-100 overflow-y-auto">
      {status.page.items.map((run) => (
        <RunRow
          key={run.run_id}
          run={run}
          selected={run.run_id === selectedRunId}
          onSelect={() => onSelectRun(run.run_id)}
        />
      ))}
    </ul>
  );
}

function RunRow({
  run,
  selected,
  onSelect,
}: {
  run: RunCheckpoint;
  selected: boolean;
  onSelect: () => void;
}): JSX.Element {
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        data-testid={`run-row-${run.run_id}`}
        data-selected={selected ? "true" : undefined}
        aria-current={selected ? "true" : undefined}
        className={`flex w-full flex-col gap-1 px-4 py-3 text-left text-sm transition ${
          selected ? "bg-sky-50" : "hover:bg-neutral-50"
        }`}
      >
        <div className="flex items-center justify-between gap-2">
          <span
            className="truncate font-mono text-xs text-neutral-700"
            title={run.run_id}
          >
            {shortRunId(run.run_id)}
          </span>
          <StatusBadge status={run.status} />
        </div>
        <div className="flex items-center gap-2 text-xs text-neutral-500">
          <span>{run.trigger_type || "—"}</span>
          <span>·</span>
          <span>{formatDuration(run.started_at_ms, run.finished_at_ms)}</span>
          <span>·</span>
          <span>{formatTime(run.created_at_ms)}</span>
        </div>
      </button>
    </li>
  );
}

function StatusBadge({ status }: { status: RunStatus }): JSX.Element {
  const cls: Record<RunStatus, string> = {
    Queued: "bg-neutral-100 text-neutral-700",
    Running: "bg-sky-100 text-sky-800",
    Succeeded: "bg-emerald-100 text-emerald-800",
    Failed: "bg-rose-100 text-rose-800",
  };
  const label: Record<RunStatus, string> = {
    Queued: "queued",
    Running: "running",
    Succeeded: "succeeded",
    Failed: "failed",
  };
  return (
    <span
      data-testid={`badge-${status}`}
      className={`rounded-full px-2 py-0.5 text-[10px] font-medium ${cls[status]}`}
    >
      {label[status]}
    </span>
  );
}

function Pagination({
  status,
  offset,
  pageSize,
  onOffsetChange,
}: {
  status: Status;
  offset: number;
  pageSize: number;
  onOffsetChange: (o: number) => void;
}): JSX.Element | null {
  if (status.kind !== "ok") return null;
  const total = status.page.total;
  if (total <= pageSize) return null;
  const page = Math.floor(offset / pageSize) + 1;
  const lastOffset = Math.max(0, Math.floor((total - 1) / pageSize) * pageSize);
  const canPrev = offset > 0;
  const canNext = offset + pageSize < total;
  return (
    <nav
      aria-label="Pagination des runs"
      className="flex items-center justify-between border-t border-neutral-200 px-4 py-2 text-xs text-neutral-500"
    >
      <span data-testid="pagination-info">
        page {page} · {total} run{total > 1 ? "s" : ""}
      </span>
      <span className="flex items-center gap-1">
        <button
          type="button"
          disabled={!canPrev}
          onClick={() => onOffsetChange(Math.max(0, offset - pageSize))}
          aria-label="Page précédente"
          className="rounded border border-neutral-200 px-2 py-0.5 disabled:opacity-40"
        >
          ‹
        </button>
        <button
          type="button"
          disabled={!canNext}
          onClick={() => onOffsetChange(Math.min(lastOffset, offset + pageSize))}
          aria-label="Page suivante"
          className="rounded border border-neutral-200 px-2 py-0.5 disabled:opacity-40"
        >
          ›
        </button>
      </span>
    </nav>
  );
}

function shortRunId(id: string): string {
  return id.length > 14 ? `${id.slice(0, 6)}…${id.slice(-6)}` : id;
}
