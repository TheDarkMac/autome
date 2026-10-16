// Sérialisation graphe React Flow ↔ `WorkflowGraph` backend (F8.2.e).
//
// Le backend (autome-core) modélise un graphe ainsi :
//
//   #[serde(tag = "kind", rename_all = "snake_case")]
//   pub enum NodeKind {
//     Trigger { trigger_type: String },
//     Action  { action_type: String },
//     If, Switch, Loop, While,
//   }
//
//   pub struct Node {
//     pub id: NodeId,            // string nue
//     pub name: String,
//     #[serde(flatten)] pub kind: NodeKind,
//     pub params: serde_json::Value,
//     pub position: Option<NodePosition>,  // { x, y } — disposition (#225)
//   }
//
//   pub struct Edge {
//     pub from: NodeId,
//     pub to:   NodeId,
//     #[serde(default, skip_serializing_if = "Option::is_none")]
//     pub from_port: Option<String>,
//   }
//
// Côté React Flow on travaille avec `RFNode` (position + data) ; le mapping
// est explicite des deux côtés et symétrique (round-trip vérifié par tests).

import type { Edge as RFEdge, Node as RFNode } from "@xyflow/react";

// ─────────────────────────── modèle backend ───────────────────────────

/** Discriminant `kind` côté wire (snake_case via serde). */
export type WireKind = "trigger" | "action" | "if" | "switch" | "loop" | "while";

/** Forme d'un Node telle que sérialisée pour le backend. */
export interface WireNode {
  id: string;
  name: string;
  kind: WireKind;
  params: unknown;
  /** Position sur le canvas (#225) ; absente des graphes écrits hors éditeur. */
  position?: { x: number; y: number };
  // Champs spécifiques à certains kinds, aplatis par `#[serde(flatten)]`.
  trigger_type?: string;
  action_type?: string;
}

/** Forme d'une Edge telle que sérialisée pour le backend. */
export interface WireEdge {
  from: string;
  to: string;
  from_port?: string;
}

/** Graphe complet. */
export interface WireGraph {
  id: string;
  name: string;
  nodes: WireNode[];
  edges: WireEdge[];
}

// ─────────────────────────── modèle UI ───────────────────────────

/** Données portées par un node React Flow — toujours synchronisées avec WireNode. */
export interface CanvasNodeData extends Record<string, unknown> {
  kind: WireKind;
  label: string;
  /** Discriminant secondaire pour Trigger/Action (verbatim côté wire). */
  subtype?: string;
  /** Paramètres opaques (édités via F8.4). */
  params?: unknown;
}

export type CanvasNode = RFNode<CanvasNodeData>;
export type CanvasEdge = RFEdge;

// ─────────────────────── helpers de construction ───────────────────────

/** Position par défaut quand on crée un node sans coord (centre du canvas). */
export const DEFAULT_POSITION = { x: 240, y: 160 } as const;

/** Génère un identifiant lisible et raisonnablement unique pour un nouveau node. */
export function generateNodeId(kind: WireKind, existing: ReadonlySet<string>): string {
  for (let i = 1; i < 1_000; i += 1) {
    const id = `${kind}-${i}`;
    if (!existing.has(id)) return id;
  }
  // Fallback déterministe : timestamp + suffixe court — collision improbable
  // (et le caller peut retenter).
  return `${kind}-${Date.now().toString(36)}`;
}

/** Construit un `CanvasNode` neuf prêt à être inséré. */
export function buildCanvasNode(args: {
  id: string;
  kind: WireKind;
  subtype?: string;
  label?: string;
  position?: { x: number; y: number };
  params?: unknown;
}): CanvasNode {
  const data: CanvasNodeData = {
    kind: args.kind,
    label: args.label ?? defaultLabel(args.kind, args.subtype),
    ...(args.subtype !== undefined ? { subtype: args.subtype } : {}),
    ...(args.params !== undefined ? { params: args.params } : {}),
  };
  return {
    id: args.id,
    type: "default",
    position: args.position ?? { ...DEFAULT_POSITION },
    data,
  };
}

function defaultLabel(kind: WireKind, subtype?: string): string {
  if (kind === "trigger" || kind === "action") {
    return subtype ? `${capitalize(kind)} · ${subtype}` : capitalize(kind);
  }
  return capitalize(kind);
}

function capitalize(s: string): string {
  return s.length === 0 ? s : `${s[0]!.toUpperCase()}${s.slice(1)}`;
}

// ─────────────────────────── serialize ───────────────────────────

/** Convertit l'état canvas vers la forme wire envoyée au backend. */
export function serializeGraph(
  id: string,
  name: string,
  nodes: readonly CanvasNode[],
  edges: readonly CanvasEdge[],
): WireGraph {
  return {
    id,
    name,
    nodes: nodes.map(serializeNode),
    edges: edges.map(serializeEdge),
  };
}

function serializeNode(n: CanvasNode): WireNode {
  const base: WireNode = {
    id: n.id,
    name: n.data.label ?? "",
    kind: n.data.kind,
    params: n.data.params ?? {},
  };
  // Une coordonnée non finie partirait en `null` dans le JSON, que le
  // backend refuse (f64) : mieux vaut laisser le node se replacer.
  if (isPosition(n.position)) base.position = { x: n.position.x, y: n.position.y };
  if (n.data.kind === "trigger") {
    base.trigger_type = n.data.subtype ?? "";
  } else if (n.data.kind === "action") {
    base.action_type = n.data.subtype ?? "";
  }
  return base;
}

function serializeEdge(e: CanvasEdge): WireEdge {
  const wire: WireEdge = { from: e.source, to: e.target };
  if (typeof e.sourceHandle === "string" && e.sourceHandle.length > 0) {
    wire.from_port = e.sourceHandle;
  }
  return wire;
}

// ─────────────────────────── deserialize ───────────────────────────

/**
 * Convertit un graphe reçu du backend en état canvas. Les nodes sans
 * position enregistrée (graphe écrit hors éditeur) sont placés par `layout`.
 */
export function deserializeGraph(
  graph: WireGraph,
  layout: (id: string, idx: number) => { x: number; y: number } = defaultLayout,
): { nodes: CanvasNode[]; edges: CanvasEdge[] } {
  const nodes = graph.nodes.map((n, idx) =>
    deserializeNode(n, isPosition(n.position) ? n.position : layout(n.id, idx)),
  );
  const edges = graph.edges.map(deserializeEdge);
  return { nodes, edges };
}

function isPosition(p: unknown): p is { x: number; y: number } {
  if (typeof p !== "object" || p === null) return false;
  const { x, y } = p as Record<string, unknown>;
  return typeof x === "number" && Number.isFinite(x) && typeof y === "number" && Number.isFinite(y);
}

function deserializeNode(n: WireNode, position: { x: number; y: number }): CanvasNode {
  const subtype =
    n.kind === "trigger" ? n.trigger_type : n.kind === "action" ? n.action_type : undefined;
  const data: CanvasNodeData = {
    kind: n.kind,
    label: n.name.length > 0 ? n.name : defaultLabel(n.kind, subtype),
    ...(subtype !== undefined && subtype.length > 0 ? { subtype } : {}),
    ...(n.params !== undefined ? { params: n.params } : {}),
  };
  return { id: n.id, type: "default", position: { x: position.x, y: position.y }, data };
}

function deserializeEdge(e: WireEdge): CanvasEdge {
  const edge: CanvasEdge = {
    id: edgeIdFromWire(e),
    source: e.from,
    target: e.to,
  };
  if (e.from_port !== undefined) {
    edge.sourceHandle = e.from_port;
  }
  return edge;
}

function edgeIdFromWire(e: WireEdge): string {
  return e.from_port ? `${e.from}-[${e.from_port}]->${e.to}` : `${e.from}->${e.to}`;
}

/** Layout grille par défaut quand le backend ne fournit pas de coords. */
function defaultLayout(_id: string, idx: number): { x: number; y: number } {
  const cols = 3;
  const colWidth = 240;
  const rowHeight = 140;
  return {
    x: 80 + (idx % cols) * colWidth,
    y: 80 + Math.floor(idx / cols) * rowHeight,
  };
}
