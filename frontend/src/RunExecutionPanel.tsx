// Panneau read-only d'exécution d'un node (F8.5.c).
//
// Quand un run est sélectionné dans la `RunsList` et qu'un node est cliqué
// sur le canvas, ce panneau remplace le `NodePanel` éditable et affiche :
//   - le statut d'exécution du node (succeeded / running / failed / pending),
//   - l'**input** dérivé du checkpoint (payload de trigger pour les nodes
//     racines, ou outputs des prédécesseurs immédiats sinon — cf.
//     `inputForNode`),
//   - l'**output** brut (`outputs[node_id]`) si déjà produit,
//   - le message d'erreur du run si le node correspond à `next_node` et
//     que le run est `Failed`.
//
// Aucun calcul d'expression côté frontend : on rend des JSON bruts. La
// lecture est non destructive (pas de bouton modifier — l'édition se fait
// via le `NodePanel` dans le mode édition).

import type { CanvasNode } from "./graph";
import type { WireEdge } from "./graph";
import {
  inputForNode,
  nodeExecutionStatus,
  type NodeExecStatus,
  type RunCheckpoint,
} from "./runs";

export interface RunExecutionPanelProps {
  /** Checkpoint courant (issu de `useRunPolling`). */
  checkpoint: RunCheckpoint | null;
  /** Node sélectionné sur le canvas. */
  node: CanvasNode | null;
  /** Edges du workflow courant (pour dériver l'input). */
  edges: readonly WireEdge[];
  onClose: () => void;
}

export function RunExecutionPanel({
  checkpoint,
  node,
  edges,
  onClose,
}: RunExecutionPanelProps): JSX.Element {
  if (!checkpoint) {
    return (
      <EmptyPanel
        title="Exécution"
        message="Sélectionnez un run dans la liste pour visualiser son exécution."
      />
    );
  }
  if (!node) {
    return (
      <EmptyPanel
        title="Exécution"
        message={
          checkpoint.next_node
            ? `Run ${checkpoint.status.toLowerCase()} · prochain node : ${checkpoint.next_node}`
            : `Run ${checkpoint.status.toLowerCase()}. Sélectionnez un node sur le canvas.`
        }
      />
    );
  }

  const status = nodeExecutionStatus(checkpoint, node.id);
  const input = inputForNode(checkpoint, edges, node.id);
  const output = Object.prototype.hasOwnProperty.call(
    checkpoint.outputs,
    node.id,
  )
    ? checkpoint.outputs[node.id]
    : undefined;
  const errorMessage =
    status === "failed" && checkpoint.error ? checkpoint.error : null;

  return (
    <aside
      aria-label="Panneau d'exécution"
      data-testid="run-execution-panel"
      data-node-id={node.id}
      data-status={status}
      className="flex h-full w-80 shrink-0 flex-col border-l border-neutral-200 bg-white"
    >
      <header className="flex items-start justify-between gap-2 border-b border-neutral-200 px-4 py-3">
        <div className="min-w-0">
          <h2 className="truncate text-sm font-semibold text-neutral-800">
            {node.data.label}
          </h2>
          <p className="mt-0.5 truncate text-xs text-neutral-500">
            {node.data.kind}
            {node.data.subtype ? ` · ${node.data.subtype}` : ""}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Fermer le panneau"
          className="rounded-md p-1 text-neutral-400 hover:bg-neutral-100 hover:text-neutral-700"
        >
          ×
        </button>
      </header>

      <div className="border-b border-neutral-200 px-4 py-2">
        <ExecStatusBadge status={status} />
      </div>

      {errorMessage ? (
        <section
          role="alert"
          data-testid="exec-error"
          className="border-b border-rose-100 bg-rose-50 px-4 py-2 text-xs text-rose-800"
        >
          <strong className="block">Erreur</strong>
          <span>{errorMessage}</span>
        </section>
      ) : null}

      <div className="flex-1 space-y-3 overflow-y-auto px-4 py-4">
        <InputSection input={input} />
        <OutputSection output={output} status={status} />
      </div>
    </aside>
  );
}

function EmptyPanel({
  title,
  message,
}: {
  title: string;
  message: string;
}): JSX.Element {
  return (
    <aside
      aria-label="Panneau d'exécution"
      data-testid="run-execution-panel"
      data-empty="true"
      className="flex h-full w-80 shrink-0 flex-col border-l border-neutral-200 bg-white"
    >
      <header className="border-b border-neutral-200 px-4 py-3">
        <h2 className="text-sm font-semibold text-neutral-800">{title}</h2>
        <p className="mt-0.5 text-xs text-neutral-500">{message}</p>
      </header>
    </aside>
  );
}

function ExecStatusBadge({ status }: { status: NodeExecStatus }): JSX.Element {
  const cls: Record<NodeExecStatus, string> = {
    succeeded: "bg-emerald-100 text-emerald-800",
    running: "bg-sky-100 text-sky-800",
    failed: "bg-rose-100 text-rose-800",
    pending: "bg-neutral-100 text-neutral-700",
  };
  const label: Record<NodeExecStatus, string> = {
    succeeded: "succeeded",
    running: "running",
    failed: "failed",
    pending: "pending",
  };
  return (
    <span
      data-testid="exec-status-badge"
      className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${cls[status]}`}
    >
      {label[status]}
    </span>
  );
}

function InputSection({
  input,
}: {
  input:
    | { kind: "trigger" }
    | { kind: "predecessors"; inputs: Readonly<Record<string, unknown>> };
}): JSX.Element {
  return (
    <section data-testid="input-section">
      <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-neutral-500">
        Input
      </h3>
      {input.kind === "trigger" ? (
        <p className="rounded-md bg-neutral-50 px-3 py-2 text-xs text-neutral-600">
          Payload du trigger (non exposé par le checkpoint actuel).
        </p>
      ) : Object.keys(input.inputs).length === 0 ? (
        <p
          data-testid="input-empty"
          className="rounded-md bg-neutral-50 px-3 py-2 text-xs text-neutral-500"
        >
          Aucune sortie de prédécesseur disponible.
        </p>
      ) : (
        <JsonBlock value={input.inputs} testId="input-json" />
      )}
    </section>
  );
}

function OutputSection({
  output,
  status,
}: {
  output: unknown;
  status: NodeExecStatus;
}): JSX.Element {
  return (
    <section data-testid="output-section">
      <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-neutral-500">
        Output
      </h3>
      {output === undefined ? (
        <p
          data-testid="output-empty"
          className="rounded-md bg-neutral-50 px-3 py-2 text-xs text-neutral-500"
        >
          {status === "pending"
            ? "Node non exécuté."
            : status === "running"
              ? "Exécution en cours…"
              : "Aucune sortie produite."}
        </p>
      ) : (
        <JsonBlock value={output} testId="output-json" />
      )}
    </section>
  );
}

function JsonBlock({
  value,
  testId,
}: {
  value: unknown;
  testId: string;
}): JSX.Element {
  return (
    <pre
      data-testid={testId}
      className="max-h-64 overflow-auto rounded-md border border-neutral-200 bg-neutral-50 px-3 py-2 text-[11px] leading-relaxed text-neutral-800"
    >
      {safeStringify(value)}
    </pre>
  );
}

function safeStringify(v: unknown): string {
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}
