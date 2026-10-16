// Workflow ouvert dans l'éditeur (#225).
//
// Remplace le brouillon local `wf-draft` : le workflow courant a un id
// généré côté client (les ids sont propres à chaque propriétaire depuis
// #188), il est créé au premier enregistrement (`POST /workflows`) puis
// remplacé (`PUT /workflows/{id}`). « Lancer » enregistre d'abord si besoin,
// puis crée un run (`POST /workflows/{id}/runs`).
//
// Les modifications non enregistrées sont détectées en comparant le graphe
// émis par le canvas à l'instantané du dernier enregistrement (même
// `serializeGraph` des deux côtés, donc même ordre de clés). Le dernier
// workflow ouvert est retenu par utilisateur dans `localStorage` (simple
// confort : absent ou illisible ⇒ nouveau workflow).

import { useCallback, useEffect, useRef, useState } from "react";

import { ApiError } from "./api";
import type { ApiClient } from "./apiClient";
import {
  deserializeGraph,
  serializeGraph,
  type CanvasEdge,
  type CanvasNode,
  type WireGraph,
} from "./graph";

/** Nom proposé pour un nouveau workflow. */
export const DEFAULT_WORKFLOW_NAME = "Sans titre";

/** Clé `localStorage` du dernier workflow ouvert par un utilisateur. */
export function lastWorkflowKey(userId: string): string {
  return `autome.lastWorkflow.${userId}`;
}

/** Id aléatoire (`wf-` + 12 hex) ; `getRandomValues` marche hors contexte sécurisé. */
export function newWorkflowId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(6));
  return `wf-${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

export interface WorkflowSummary {
  readonly id: string;
  readonly name: string;
}

export interface CurrentWorkflow {
  readonly id: string;
  readonly name: string;
  /** `true` dès que le serveur connaît ce workflow. */
  readonly persisted: boolean;
}

/** État initial du canvas ; `key` change à chaque ouverture (remontage). */
export interface CanvasSeed {
  readonly key: number;
  readonly nodes: CanvasNode[];
  readonly edges: CanvasEdge[];
}

export type EditorBusy = "list" | "open" | "save" | "run" | "delete" | null;

export interface UseWorkflowEditorOptions {
  /** Confirmation des actions qui perdent des modifications (tests). */
  confirmImpl?: (message: string) => boolean;
  /** Stockage du dernier workflow ouvert (tests) ; `null` = désactivé. */
  storage?: Storage | null;
}

export interface WorkflowEditor {
  current: CurrentWorkflow;
  canvas: CanvasSeed;
  /** Dernier graphe émis par le canvas. */
  graph: WireGraph | null;
  workflows: readonly WorkflowSummary[];
  dirty: boolean;
  busy: EditorBusy;
  error: string | null;
  /** Incrémenté à chaque enregistrement (rafraîchit les URL webhook). */
  revision: number;
  setName(name: string): void;
  onGraphChange(graph: WireGraph): void;
  newWorkflow(): void;
  open(id: string): Promise<void>;
  /** Id enregistré, ou `null` en cas d'échec (erreur exposée). */
  save(): Promise<string | null>;
  /** Id du run créé, ou `null` en cas d'échec. */
  run(): Promise<string | null>;
  remove(): Promise<void>;
}

const DISCARD_MESSAGE =
  "Le workflow a des modifications non enregistrées. Les abandonner ?";

function emptySeed(key: number): CanvasSeed {
  return { key, nodes: [], edges: [] };
}

function snapshot(g: WireGraph): string {
  return JSON.stringify(g);
}

function defaultStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/** Message lisible pour un échec d'appel, sans détail technique superflu. */
export function describeEditorError(action: string, e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 403) return `${action} : droits insuffisants.`;
    if (e.status === 404) return `${action} : workflow introuvable.`;
  }
  return `${action} : ${(e as Error).message}`;
}

export function useWorkflowEditor(
  api: ApiClient,
  userId: string | null,
  { confirmImpl = (m) => window.confirm(m), storage }: UseWorkflowEditorOptions = {},
): WorkflowEditor {
  const store = storage === undefined ? defaultStorage() : storage;
  const [current, setCurrent] = useState<CurrentWorkflow>(() => ({
    id: newWorkflowId(),
    name: DEFAULT_WORKFLOW_NAME,
    persisted: false,
  }));
  const [canvas, setCanvas] = useState<CanvasSeed>(() => emptySeed(0));
  const [graph, setGraph] = useState<WireGraph | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [workflows, setWorkflows] = useState<readonly WorkflowSummary[]>([]);
  const [busy, setBusy] = useState<EditorBusy>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);

  // Pour l'instantané d'un nouveau workflow vide : tant que le canvas n'a
  // rien émis, on n'est pas « modifié ».
  const dirty = graph !== null && saved !== null && snapshot(graph) !== saved;
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;

  const remember = useCallback(
    (id: string | null) => {
      if (!store || !userId) return;
      try {
        if (id) store.setItem(lastWorkflowKey(userId), id);
        else store.removeItem(lastWorkflowKey(userId));
      } catch {
        // Stockage plein ou bloqué : simple confort perdu.
      }
    },
    [store, userId],
  );

  const load = useCallback((wf: WireGraph, persisted: boolean) => {
    const { nodes, edges } = deserializeGraph(wf);
    const normalized = serializeGraph(wf.id, wf.name, nodes, edges);
    setCanvas((c) => ({ key: c.key + 1, nodes, edges }));
    setCurrent({ id: wf.id, name: wf.name, persisted });
    setGraph(normalized);
    setSaved(snapshot(normalized));
    setError(null);
  }, []);

  const startNew = useCallback(() => {
    load({ id: newWorkflowId(), name: DEFAULT_WORKFLOW_NAME, nodes: [], edges: [] }, false);
  }, [load]);

  const confirmDiscard = useCallback(
    () => !dirtyRef.current || confirmImpl(DISCARD_MESSAGE),
    [confirmImpl],
  );

  // Liste des workflows de l'utilisateur, puis réouverture du dernier ouvert.
  useEffect(() => {
    setWorkflows([]);
    if (!userId) return;
    let cancelled = false;
    setBusy("list");
    (async () => {
      let items: WorkflowSummary[] = [];
      try {
        // Pages de 100 : la liste sert de sélecteur, pas de vue paginée.
        for (let offset = 0; ; offset += 100) {
          const page = await api.listWorkflows({ offset, limit: 100 });
          items = items.concat(page.items.map(({ id, name }) => ({ id, name })));
          if (page.items.length === 0 || items.length >= page.total) break;
        }
      } catch (e) {
        if (!cancelled) setError(describeEditorError("Chargement des workflows", e));
      }
      if (cancelled) return;
      setWorkflows(items);
      let last: string | null = null;
      try {
        last = store?.getItem(lastWorkflowKey(userId)) ?? null;
      } catch {
        last = null;
      }
      if (last && items.some((w) => w.id === last) && !dirtyRef.current) {
        try {
          const wf = await api.getWorkflow(last);
          if (!cancelled && !dirtyRef.current) load(wf, true);
        } catch (e) {
          if (!cancelled) setError(describeEditorError("Ouverture du workflow", e));
        }
      }
      if (!cancelled) setBusy(null);
    })();
    return () => {
      cancelled = true;
    };
  }, [api, userId, store, load]);

  // Fermeture de l'onglet avec des modifications : alerte du navigateur.
  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent): void => {
      e.preventDefault();
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);

  const onGraphChange = useCallback((g: WireGraph) => {
    setGraph(g);
    // Premier rendu d'un nouveau workflow : l'instantané de référence est
    // le graphe vide émis par le canvas.
    setSaved((s) => s ?? snapshot(g));
  }, []);

  const setName = useCallback((name: string) => {
    setCurrent((c) => ({ ...c, name }));
  }, []);

  const newWorkflow = useCallback(() => {
    if (!confirmDiscard()) return;
    startNew();
    remember(null);
  }, [confirmDiscard, startNew, remember]);

  const open = useCallback(
    async (id: string) => {
      if (id === current.id || !confirmDiscard()) return;
      setBusy("open");
      setError(null);
      try {
        load(await api.getWorkflow(id), true);
        remember(id);
      } catch (e) {
        setError(describeEditorError("Ouverture du workflow", e));
      } finally {
        setBusy(null);
      }
    },
    [api, current.id, confirmDiscard, load, remember],
  );

  const save = useCallback(async (): Promise<string | null> => {
    if (!graph) return null;
    if (current.name.trim().length === 0) {
      setError("Donnez un nom au workflow avant de l'enregistrer.");
      return null;
    }
    setBusy("save");
    setError(null);
    let body: WireGraph = { ...graph, id: current.id, name: current.name.trim() };
    try {
      if (current.persisted) {
        await api.updateWorkflow(body.id, body);
      } else {
        try {
          await api.createWorkflow(body);
        } catch (e) {
          // Id déjà pris dans mon espace (collision aléatoire) : un autre.
          if (!(e instanceof ApiError && e.status === 409)) throw e;
          body = { ...body, id: newWorkflowId() };
          await api.createWorkflow(body);
        }
      }
      setCurrent({ id: body.id, name: body.name, persisted: true });
      setSaved(snapshot(body));
      setGraph((g) => (g && g.id !== body.id ? { ...g, id: body.id } : g));
      setWorkflows((ws) =>
        ws.some((w) => w.id === body.id)
          ? ws.map((w) => (w.id === body.id ? { id: body.id, name: body.name } : w))
          : [...ws, { id: body.id, name: body.name }],
      );
      setRevision((r) => r + 1);
      remember(body.id);
      return body.id;
    } catch (e) {
      setError(describeEditorError("Enregistrement impossible", e));
      return null;
    } finally {
      setBusy(null);
    }
  }, [api, graph, current, remember]);

  const run = useCallback(async (): Promise<string | null> => {
    let id: string | null = current.id;
    if (!current.persisted || dirty) {
      id = await save();
      if (!id) return null;
    }
    setBusy("run");
    setError(null);
    try {
      const created = await api.createRun(id);
      return created.run_id;
    } catch (e) {
      setError(describeEditorError("Lancement impossible", e));
      return null;
    } finally {
      setBusy(null);
    }
  }, [api, current, dirty, save]);

  const remove = useCallback(async () => {
    if (!current.persisted) {
      newWorkflow();
      return;
    }
    if (!confirmImpl(`Supprimer le workflow « ${current.name} » ?`)) return;
    setBusy("delete");
    setError(null);
    try {
      await api.deleteWorkflow(current.id);
      setWorkflows((ws) => ws.filter((w) => w.id !== current.id));
      remember(null);
      startNew();
    } catch (e) {
      setError(describeEditorError("Suppression impossible", e));
    } finally {
      setBusy(null);
    }
  }, [api, current, confirmImpl, newWorkflow, remember, startNew]);

  return {
    current,
    canvas,
    graph,
    workflows,
    dirty,
    busy,
    error,
    revision,
    setName,
    onGraphChange,
    newWorkflow,
    open,
    save,
    run,
    remove,
  };
}
