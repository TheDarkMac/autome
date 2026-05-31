// Canvas drag-and-drop (F8.2 + F8.3.b).
//
// Le composant gère un state local (nodes + edges) au-dessus de
// `@xyflow/react`. Les nouveaux nodes arrivent via **drop** depuis la
// `Palette` (F8.3) : le `dataTransfer` du MIME `application/x-autome-node`
// porte un `CatalogItem` JSON, qu'on convertit en `CanvasNode` à la
// position du curseur (via `screenToFlowPosition`).
//
// La sérialisation est propagée à `onChange` à chaque mutation — F8.6
// branchera la persistance backend par-dessus.

import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  ReactFlow,
  ReactFlowProvider,
  addEdge,
  applyEdgeChanges,
  applyNodeChanges,
  useReactFlow,
  type Connection,
  type EdgeChange,
  type NodeChange,
  type OnSelectionChangeParams,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useCallback, useEffect, useMemo, useState } from "react";

import { DND_MIME, type CatalogItem } from "./catalog";
import {
  buildCanvasNode,
  generateNodeId,
  serializeGraph,
  type CanvasEdge,
  type CanvasNode,
  type WireGraph,
} from "./graph";
import {
  defaultParams,
  findNodeSchema,
  type ParamValues,
} from "./paramSchema";

export interface NodeCanvasProps {
  /** Notifié à chaque mutation du graphe. */
  onChange?: (wire: WireGraph) => void;
  workflowId?: string;
  workflowName?: string;
  initialNodes?: CanvasNode[];
  initialEdges?: CanvasEdge[];
  /** Notifié quand la sélection change (F8.4). */
  onSelectionChange?: (node: CanvasNode | null) => void;
  /**
   * Exposé une fois au mount : un patcher que le parent peut appeler pour
   * mettre à jour les `params` d'un node (depuis le NodePanel, F8.4).
   */
  onReady?: (controller: NodeCanvasController) => void;
}

export interface NodeCanvasController {
  patchNodeParams(nodeId: string, params: ParamValues): void;
}

/** Wrapper qui injecte le `ReactFlowProvider` requis par `useReactFlow`. */
export function NodeCanvas(props: NodeCanvasProps): JSX.Element {
  return (
    <ReactFlowProvider>
      <NodeCanvasInner {...props} />
    </ReactFlowProvider>
  );
}

function NodeCanvasInner({
  onChange,
  workflowId = "wf-draft",
  workflowName = "Brouillon",
  initialNodes = [],
  initialEdges = [],
  onSelectionChange,
  onReady,
}: NodeCanvasProps): JSX.Element {
  const [nodes, setNodes] = useState<CanvasNode[]>(initialNodes);
  const [edges, setEdges] = useState<CanvasEdge[]>(initialEdges);
  const { screenToFlowPosition } = useReactFlow();

  // Expose un patcher imperatif aux parents (App → NodePanel).
  useEffect(() => {
    if (!onReady) return;
    onReady({
      patchNodeParams: (nodeId, params) => {
        setNodes((prev) =>
          prev.map((n) =>
            n.id === nodeId
              ? { ...n, data: { ...n.data, params } }
              : n,
          ),
        );
      },
    });
  }, [onReady]);

  const onSelectionChangeRF = useCallback(
    (params: OnSelectionChangeParams): void => {
      if (!onSelectionChange) return;
      const selected = params.nodes[0];
      onSelectionChange(selected ? (selected as CanvasNode) : null);
    },
    [onSelectionChange],
  );

  const existingIds = useMemo(() => new Set(nodes.map((n) => n.id)), [nodes]);

  // ─── React Flow callbacks ───

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

  const onConnect = useCallback(
    (connection: Connection) =>
      setEdges((es) => addEdge(connection, es) as CanvasEdge[]),
    [],
  );

  // ─── F8.3.b — drop depuis la palette ───

  const onDragOver = useCallback((event: React.DragEvent): void => {
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
  }, []);

  const onDrop = useCallback(
    (event: React.DragEvent): void => {
      event.preventDefault();
      const raw = event.dataTransfer.getData(DND_MIME);
      if (!raw) return;
      let item: CatalogItem;
      try {
        item = JSON.parse(raw) as CatalogItem;
      } catch {
        return;
      }
      const position = screenToFlowPosition({
        x: event.clientX,
        y: event.clientY,
      });
      setNodes((prev) => {
        const ids = new Set(prev.map((n) => n.id));
        const id = generateNodeId(item.kind, ids);
        const schema = findNodeSchema(item.kind, item.subtype);
        const params = schema ? defaultParams(schema) : undefined;
        const node = buildCanvasNode({
          id,
          kind: item.kind,
          ...(item.subtype !== undefined ? { subtype: item.subtype } : {}),
          label: item.label,
          position,
          ...(params !== undefined ? { params } : {}),
        });
        return [...prev, node];
      });
    },
    [screenToFlowPosition],
  );

  // ─── Sérialisation propagée ───

  useEffect(() => {
    if (!onChange) return;
    onChange(serializeGraph(workflowId, workflowName, nodes, edges));
  }, [nodes, edges, workflowId, workflowName, onChange]);

  return (
    <div
      className="flex h-full w-full flex-col"
      data-testid="node-canvas"
      onDragOver={onDragOver}
      onDrop={onDrop}
    >
      <div className="relative flex-1" style={{ minHeight: 480 }}>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          onConnect={onConnect}
          onSelectionChange={onSelectionChangeRF}
          fitView
          deleteKeyCode={["Backspace", "Delete"]}
        >
          <Controls />
          <MiniMap pannable zoomable />
          <Background variant={BackgroundVariant.Dots} gap={16} />
        </ReactFlow>
      </div>
      <Counters nodes={nodes} edges={edges} existing={existingIds} />
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
