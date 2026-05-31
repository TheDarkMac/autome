// Lecture des exécutions de workflow (F8.5).
//
// Endpoints backend (F7.2) :
//   - GET /workflows/{id}/runs?offset=&limit=  → ListRunsResponse
//   - GET /runs/{run_id}                       → RunCheckpoint
//
// Pas d'auth ici (à brancher en F8.6). Les helpers `nodeExecutionStatus`,
// `formatDuration`, `inputForNode` et `isTerminalStatus` sont **purs** et
// directement testables — c'est la matière première de la vue exécution.

import { ApiError, apiBaseUrl } from "./api";
import type { CanvasNode, WireEdge } from "./graph";

/** Statuts wire — capitalisation conservée du backend (`RunStatus`). */
export type RunStatus = "Queued" | "Running" | "Succeeded" | "Failed";

/** Statut d'exécution dérivé pour un node particulier. */
export type NodeExecStatus = "succeeded" | "running" | "failed" | "pending";

/** Snapshot d'un run tel que renvoyé par le backend. */
export interface RunCheckpoint {
  readonly run_id: string;
  readonly workflow_id: string;
  readonly version: number;
  readonly trigger_type: string;
  readonly status: RunStatus;
  readonly started_at_ms: number | null;
  readonly finished_at_ms: number | null;
  readonly created_at_ms: number;
  readonly outputs: Readonly<Record<string, unknown>>;
  readonly next_node: string | null;
  readonly error: string | null;
}

/** Page renvoyée par `GET /workflows/{id}/runs`. */
export interface RunListPage {
  readonly items: readonly RunCheckpoint[];
  readonly total: number;
  readonly offset: number;
  readonly limit: number;
}

export interface FetchRunsParams {
  offset?: number;
  limit?: number;
}

const DEFAULT_LIMIT = 50;

export async function fetchRunsForWorkflow(
  workflowId: string,
  params: FetchRunsParams = {},
  fetchImpl: typeof fetch = fetch,
): Promise<RunListPage> {
  const offset = params.offset ?? 0;
  const limit = params.limit ?? DEFAULT_LIMIT;
  const url =
    `${apiBaseUrl()}/workflows/${encodeURIComponent(workflowId)}/runs` +
    `?offset=${offset}&limit=${limit}`;
  let res: Response;
  try {
    res = await fetchImpl(url, { headers: { Accept: "application/json" } });
  } catch (e) {
    throw new ApiError(`runs injoignables : ${(e as Error).message}`);
  }
  if (!res.ok) {
    throw new ApiError(
      `GET /workflows/${workflowId}/runs a échoué (${res.status})`,
      res.status,
    );
  }
  return parseRunListPage(await res.json());
}

export async function fetchRun(
  runId: string,
  fetchImpl: typeof fetch = fetch,
): Promise<RunCheckpoint> {
  const url = `${apiBaseUrl()}/runs/${encodeURIComponent(runId)}`;
  let res: Response;
  try {
    res = await fetchImpl(url, { headers: { Accept: "application/json" } });
  } catch (e) {
    throw new ApiError(`run injoignable : ${(e as Error).message}`);
  }
  if (!res.ok) {
    throw new ApiError(`GET /runs/${runId} a échoué (${res.status})`, res.status);
  }
  return parseRunCheckpoint(await res.json());
}

// ─────────────────────────── parsing tolérant ───────────────────────────

function parseRunListPage(raw: unknown): RunListPage {
  const r = (raw ?? {}) as Record<string, unknown>;
  const itemsRaw = Array.isArray(r["items"]) ? (r["items"] as unknown[]) : [];
  const items: RunCheckpoint[] = [];
  for (const it of itemsRaw) {
    const ck = tryParseRunCheckpoint(it);
    if (ck) items.push(ck);
  }
  return {
    items,
    total: numberOr(r["total"], items.length),
    offset: numberOr(r["offset"], 0),
    limit: numberOr(r["limit"], DEFAULT_LIMIT),
  };
}

function parseRunCheckpoint(raw: unknown): RunCheckpoint {
  const ck = tryParseRunCheckpoint(raw);
  if (!ck) throw new ApiError("payload /runs/{id} mal formé");
  return ck;
}

function tryParseRunCheckpoint(raw: unknown): RunCheckpoint | null {
  if (typeof raw !== "object" || raw === null) return null;
  const v = raw as Record<string, unknown>;
  const run_id = typeof v["run_id"] === "string" ? v["run_id"] : null;
  const workflow_id =
    typeof v["workflow_id"] === "string" ? v["workflow_id"] : null;
  const statusRaw = typeof v["status"] === "string" ? v["status"] : null;
  if (!run_id || !workflow_id || !statusRaw || !isRunStatus(statusRaw)) {
    return null;
  }
  const outputs =
    typeof v["outputs"] === "object" && v["outputs"] !== null
      ? (v["outputs"] as Record<string, unknown>)
      : {};
  return {
    run_id,
    workflow_id,
    version: numberOr(v["version"], 1),
    trigger_type:
      typeof v["trigger_type"] === "string" ? v["trigger_type"] : "",
    status: statusRaw,
    started_at_ms: numberOrNull(v["started_at_ms"]),
    finished_at_ms: numberOrNull(v["finished_at_ms"]),
    created_at_ms: numberOr(v["created_at_ms"], 0),
    outputs,
    next_node: typeof v["next_node"] === "string" ? v["next_node"] : null,
    error: typeof v["error"] === "string" ? v["error"] : null,
  };
}

function isRunStatus(s: string): s is RunStatus {
  return (
    s === "Queued" || s === "Running" || s === "Succeeded" || s === "Failed"
  );
}

function numberOr(v: unknown, def: number): number {
  return typeof v === "number" && Number.isFinite(v) ? v : def;
}

function numberOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

// ─────────────────────────── dérivations pures ───────────────────────────

export function isTerminalStatus(s: RunStatus): boolean {
  return s === "Succeeded" || s === "Failed";
}

/**
 * Statut d'exécution d'un node dérivé du checkpoint :
 *   - succeeded : node ∈ outputs ;
 *   - running   : node == next_node && status == Running ;
 *   - failed    : node == next_node && status == Failed ;
 *   - pending   : sinon (queued, ou non encore atteint).
 */
export function nodeExecutionStatus(
  checkpoint: RunCheckpoint,
  nodeId: string,
): NodeExecStatus {
  if (Object.prototype.hasOwnProperty.call(checkpoint.outputs, nodeId)) {
    return "succeeded";
  }
  if (checkpoint.next_node === nodeId) {
    if (checkpoint.status === "Failed") return "failed";
    if (checkpoint.status === "Running") return "running";
  }
  return "pending";
}

/** Formate la durée d'un run (ou « en cours » / « — »). */
export function formatDuration(
  startedAtMs: number | null,
  finishedAtMs: number | null,
  nowMs: number = Date.now(),
): string {
  if (startedAtMs == null) return "—";
  const end = finishedAtMs ?? nowMs;
  const d = Math.max(0, end - startedAtMs);
  if (d < 1000) return `${d} ms`;
  if (d < 60_000) return `${(d / 1000).toFixed(1)} s`;
  const m = Math.floor(d / 60_000);
  const s = Math.floor((d % 60_000) / 1000);
  return `${m} min ${s.toString().padStart(2, "0")} s`;
}

/** Formate un timestamp ms en heure locale courte (HH:MM). */
export function formatTime(ms: number): string {
  const d = new Date(ms);
  const hh = d.getHours().toString().padStart(2, "0");
  const mm = d.getMinutes().toString().padStart(2, "0");
  return `${hh}:${mm}`;
}

/**
 * Calcule l'« input » d'un node pour une exécution donnée.
 *
 *   - root (aucun prédécesseur) → `{ kind: "trigger" }` (le payload initial
 *     n'est pas exposé par le checkpoint courant — on indique seulement
 *     que l'input vient du trigger).
 *   - sinon → `{ kind: "predecessors", inputs }` où `inputs[pred_id]` est
 *     l'output du prédécesseur si déjà produit. Les prédécesseurs non
 *     encore exécutés sont **omis** de l'objet (la clé est absente).
 */
export type NodeInputView =
  | { readonly kind: "trigger" }
  | {
      readonly kind: "predecessors";
      readonly inputs: Readonly<Record<string, unknown>>;
    };

export function inputForNode(
  checkpoint: RunCheckpoint,
  edges: readonly WireEdge[],
  nodeId: string,
): NodeInputView {
  const preds = edges.filter((e) => e.to === nodeId).map((e) => e.from);
  if (preds.length === 0) return { kind: "trigger" };
  const inputs: Record<string, unknown> = {};
  for (const p of preds) {
    if (Object.prototype.hasOwnProperty.call(checkpoint.outputs, p)) {
      inputs[p] = checkpoint.outputs[p];
    }
  }
  return { kind: "predecessors", inputs };
}

// ─────────────────────── overlay canvas (F8.5.b) ────────────────────────

const EXEC_STATUS_COLORS: Readonly<Record<NodeExecStatus, string>> = {
  succeeded: "#10b981", // emerald-500
  running: "#0ea5e9", // sky-500
  failed: "#ef4444", // rose-500
  pending: "#94a3b8", // slate-400
};

/** Couleur recommandée pour afficher un statut d'exécution dans l'UI. */
export function execStatusColor(status: NodeExecStatus): string {
  return EXEC_STATUS_COLORS[status];
}

/** Applique une bordure colorée à chaque node selon son statut d'exécution. */
export function decorateNodesWithExecStatus(
  nodes: readonly CanvasNode[],
  statuses: Readonly<Record<string, NodeExecStatus>>,
): CanvasNode[] {
  return nodes.map((n) => {
    const st = statuses[n.id];
    if (!st) return n;
    return {
      ...n,
      style: {
        ...(n.style ?? {}),
        borderColor: EXEC_STATUS_COLORS[st],
        borderWidth: 2,
      },
      data: { ...n.data, execStatus: st },
    };
  });
}
