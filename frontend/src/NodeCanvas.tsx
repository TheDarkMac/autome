// Canvas drag-and-drop (F8.2.b + F8.2.c + F8.2.d).
//
// Le composant gère un état local (nodes + edges) au-dessus de `@xyflow/react`.
// Les ajouts passent par une mini-palette temporaire (4 boutons) ; la
// palette riche est livrée en F8.3. La sérialisation est exposée via
// `onChange(serialized)` afin que F8.2.e soit consommable par le reste de
// l'app sans tirer dans React Flow.

import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  ReactFlow,
  addEdge,
  applyEdgeChanges,
  applyNodeChanges,
  type Connection,
  type EdgeChange,
  type NodeChange,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  buildCanvasNode,
  generateNodeId,
  serializeGraph,
  type CanvasEdge,
  type CanvasNode,
  type WireGraph,
  type WireKind,
} from "./graph";

/** Description d'un bouton de la mini-palette (F8.2.b). */
interface PaletteItem {
  kind: WireKind;
  subtype?: string;
  label: string;
}

const PALETTE: readonly PaletteItem[] = [
  { kind: "trigger", subtype: "webhook", label: "Trigger · webhook" },
  { kind: "action", subtype: "slack.send", label: "Action · slack.send" },
  { kind: "if", label: "If" },
  { kind: "loop", label: "Loop" },
];

export interface NodeCanvasProps {
  /** Notifié à chaque mutation du graphe (utile pour persistance / tests). */
  onChange?: (wire: WireGraph) => void;
  /** ID + nom utilisés lors de la sérialisation. */
  workflowId?: string;
  workflowName?: string;
  /** État initial du graphe (utile pour les tests / la réhydratation). */
  initialNodes?: CanvasNode[];
  initialEdges?: CanvasEdge[];
}

export function NodeCanvas({
  onChange,
  workflowId = "wf-draft",
  workflowName = "Brouillon",
  initialNodes = [],
  initialEdges = [],
}: NodeCanvasProps): JSX.Element {
  const [nodes, setNodes] = useState<CanvasNode[]>(initialNodes);
  const [edges, setEdges] = useState<CanvasEdge[]>(initialEdges);

  const existingIds = useMemo(() => new Set(nodes.map((n) => n.id)), [nodes]);

  // F8.2.b — Ajout d'un node depuis la mini-palette.
  const addNode = useCallback(
    (item: PaletteItem) => {
      setNodes((prev) => {
        const ids = new Set(prev.map((n) => n.id));
        const id = generateNodeId(item.kind, ids);
        const node = buildCanvasNode({
          id,
          kind: item.kind,
          ...(item.subtype !== undefined ? { subtype: item.subtype } : {}),
          label: item.label,
          // Position auto : décalée à chaque ajout pour éviter le chevauchement.
          position: { x: 120 + (prev.length % 5) * 60, y: 120 + (prev.length % 5) * 60 },
        });
        return [...prev, node];
      });
    },
    [],
  );

  // F8.2.b/c — Mises à jour par React Flow (drag, sélection, suppression).
  const onNodesChange = useCallback(
    (changes: NodeChange[]) =>
      setNodes((ns) => applyNodeChanges(changes, ns) as CanvasNode[]),
    [],
  );

  const onEdgesChange = useCallback(
    (changes: EdgeChange[]) =>
      setEdges((es) => applyEdgeChanges(changes, es) as CanvasEdge[]),
    [],
  );

  // F8.2.c — Nouvelle connexion (drag entre handles).
  const onConnect = useCallback(
    (connection: Connection) =>
      setEdges((es) => addEdge(connection, es) as CanvasEdge[]),
    [],
  );

  // Propage la sérialisation à chaque mutation (F8.2.e).
  useEffect(() => {
    if (!onChange) return;
    onChange(serializeGraph(workflowId, workflowName, nodes, edges));
  }, [nodes, edges, workflowId, workflowName, onChange]);

  return (
    <div className="flex h-full w-full flex-col" data-testid="node-canvas">
      <Palette items={PALETTE} onAdd={addNode} />
      <div className="relative flex-1" style={{ minHeight: 480 }}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          fitView
          deleteKeyCode={["Backspace", "Delete"]}
        >
          {/* F8.2.d — Zoom / pan / mini-map / fond. */}
          <Controls />
          <MiniMap pannable zoomable />
          <Background variant={BackgroundVariant.Dots} gap={16} />
        </ReactFlow>
      </div>
      <Counters nodes={nodes} edges={edges} existing={existingIds} />
    </div>
  );
}

function Palette({
  items,
  onAdd,
}: {
  items: readonly PaletteItem[];
  onAdd: (item: PaletteItem) => void;
}): JSX.Element {
  return (
    <div
      role="toolbar"
      aria-label="Mini-palette de nodes (F8.2)"
      className="flex flex-wrap gap-2 border-b border-neutral-200 bg-white px-4 py-3"
    >
      <span className="self-center text-sm font-medium text-neutral-600">
        Ajouter :
      </span>
      {items.map((item) => (
        <button
          key={`${item.kind}:${item.subtype ?? ""}`}
          type="button"
          onClick={() => onAdd(item)}
          className="rounded-md border border-neutral-200 bg-neutral-50 px-3 py-1 text-sm font-medium text-neutral-800 transition hover:bg-neutral-100"
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}

function Counters({
  nodes,
  edges,
  existing,
}: {
  nodes: readonly CanvasNode[];
  edges: readonly CanvasEdge[];
  existing: ReadonlySet<string>;
}): JSX.Element {
  return (
    <div
      data-testid="canvas-counters"
      className="border-t border-neutral-200 bg-white px-4 py-2 text-xs text-neutral-500"
    >
      <span data-testid="nodes-count">{nodes.length} node(s)</span>
      <span className="px-2">·</span>
      <span data-testid="edges-count">{edges.length} edge(s)</span>
      <span className="sr-only">{existing.size} ids enregistrés</span>
    </div>
  );
}
