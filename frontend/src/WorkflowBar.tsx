// Barre d'outils du workflow ouvert (#225) : choix du workflow, nom,
// enregistrement, lancement, suppression. Toute la logique vit dans
// `useWorkflowEditor` ; ce composant ne fait qu'afficher et relayer.

import type { WorkflowEditor } from "./useWorkflowEditor";

export interface WorkflowBarProps {
  editor: WorkflowEditor;
  /** `workflows.write` (le serveur refuserait de toute façon). */
  canWrite: boolean;
  /** `runs.execute`. */
  canRun: boolean;
  /** Appelé avec l'id du run créé par « Lancer ». */
  onRunStarted: (runId: string) => void;
}

const BUTTON =
  "rounded-md border px-3 py-1.5 text-sm disabled:cursor-not-allowed disabled:opacity-50";

export function WorkflowBar({
  editor,
  canWrite,
  canRun,
  onRunStarted,
}: WorkflowBarProps): JSX.Element {
  const { current, workflows, dirty, busy, error, graph } = editor;
  const idle = busy === null;
  const hasNodes = (graph?.nodes.length ?? 0) > 0;
  const needsSave = !current.persisted || dirty;

  const state = !current.persisted
    ? "Jamais enregistré"
    : dirty
      ? "Modifications non enregistrées"
      : "Enregistré";

  const run = async (): Promise<void> => {
    const runId = await editor.run();
    if (runId) onRunStarted(runId);
  };

  return (
    <div
      role="toolbar"
      aria-label="Workflow"
      data-testid="workflow-bar"
      className="flex flex-wrap items-center gap-2 border-b border-neutral-200 bg-white px-6 py-2 sm:px-10"
    >
      <select
        aria-label="Workflow ouvert"
        value={current.id}
        disabled={!idle}
        onChange={(e) => void editor.open(e.target.value)}
        className="max-w-56 rounded-md border border-neutral-200 bg-white px-2 py-1.5 text-sm"
      >
        {!current.persisted && (
          <option value={current.id}>Nouveau workflow</option>
        )}
        {workflows.map((w) => (
          <option key={w.id} value={w.id}>
            {w.name || w.id}
          </option>
        ))}
      </select>
      <button
        type="button"
        onClick={editor.newWorkflow}
        disabled={!idle}
        className={`${BUTTON} border-neutral-200 text-neutral-700 hover:bg-neutral-50`}
      >
        Nouveau
      </button>
      <input
        aria-label="Nom du workflow"
        value={current.name}
        onChange={(e) => editor.setName(e.target.value)}
        maxLength={200}
        className="w-56 rounded-md border border-neutral-200 px-2 py-1.5 text-sm"
      />
      <span
        data-testid="workflow-state"
        className={`text-xs ${dirty ? "text-amber-700" : "text-neutral-500"}`}
      >
        {state}
      </span>
      <div className="ml-auto flex items-center gap-2">
        <button
          type="button"
          onClick={() => void editor.save()}
          disabled={!idle || !canWrite || !needsSave}
          title={canWrite ? undefined : "Droit workflows.write requis"}
          className={`${BUTTON} border-neutral-200 text-neutral-700 hover:bg-neutral-50`}
        >
          {busy === "save" ? "Enregistrement…" : "Enregistrer"}
        </button>
        <button
          type="button"
          onClick={() => void run()}
          disabled={!idle || !canRun || !hasNodes || (needsSave && !canWrite)}
          title={
            !canRun
              ? "Droit runs.execute requis"
              : !hasNodes
                ? "Ajoutez au moins un node"
                : needsSave
                  ? "Enregistre puis lance le workflow"
                  : undefined
          }
          className={`${BUTTON} border-sky-600 bg-sky-600 text-white hover:bg-sky-700`}
        >
          {busy === "run" ? "Lancement…" : "Lancer"}
        </button>
        <button
          type="button"
          onClick={() => void editor.remove()}
          disabled={!idle || !current.persisted || !canWrite}
          className={`${BUTTON} border-rose-200 text-rose-700 hover:bg-rose-50`}
        >
          Supprimer
        </button>
      </div>
      {error && (
        <p role="alert" className="basis-full text-sm text-rose-700">
          {error}
        </p>
      )}
    </div>
  );
}
