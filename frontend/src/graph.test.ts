import { describe, expect, it } from "vitest";
import {
  buildCanvasNode,
  deserializeGraph,
  generateNodeId,
  serializeGraph,
  type CanvasEdge,
  type CanvasNode,
  type WireGraph,
} from "./graph";

function asSet(values: string[]): ReadonlySet<string> {
  return new Set(values);
}

describe("serializeGraph", () => {
  it("emits trigger nodes with snake_case kind and trigger_type", () => {
    const node = buildCanvasNode({
      id: "trigger-1",
      kind: "trigger",
      subtype: "webhook",
      label: "Webhook",
      position: { x: 10, y: 20 },
    });
    const wire = serializeGraph("wf-1", "demo", [node], []);
    expect(wire.id).toBe("wf-1");
    expect(wire.nodes).toHaveLength(1);
    expect(wire.nodes[0]).toEqual({
      id: "trigger-1",
      name: "Webhook",
      kind: "trigger",
      trigger_type: "webhook",
      params: {},
      position: { x: 10, y: 20 },
    });
  });

  it("emits action nodes with action_type and ignores subtype on if/loop", () => {
    const action = buildCanvasNode({
      id: "act-1",
      kind: "action",
      subtype: "slack.send",
    });
    const branch = buildCanvasNode({ id: "if-1", kind: "if" });
    const wire = serializeGraph("wf-1", "demo", [action, branch], []);
    expect(wire.nodes[0]?.kind).toBe("action");
    expect(wire.nodes[0]?.action_type).toBe("slack.send");
    expect(wire.nodes[1]?.kind).toBe("if");
    expect(wire.nodes[1]).not.toHaveProperty("trigger_type");
    expect(wire.nodes[1]).not.toHaveProperty("action_type");
  });

  it("emits edges with from/to and from_port when sourceHandle is set", () => {
    const edges: CanvasEdge[] = [
      { id: "a->b", source: "a", target: "b" },
      { id: "if->true", source: "if-1", target: "b", sourceHandle: "true" },
    ];
    const wire = serializeGraph("wf-1", "demo", [], edges);
    expect(wire.edges).toEqual([
      { from: "a", to: "b" },
      { from: "if-1", to: "b", from_port: "true" },
    ]);
  });
});

describe("serializeGraph — positions (#225)", () => {
  it("omits a non-finite position instead of sending null", () => {
    const node = buildCanvasNode({ id: "a", kind: "if", position: { x: Number.NaN, y: 3 } });
    const wire = serializeGraph("wf-1", "demo", [node], []);
    expect(wire.nodes[0]).not.toHaveProperty("position");
  });
});

describe("deserializeGraph", () => {
  it("rebuilds CanvasNodes with subtype on trigger/action", () => {
    const wire: WireGraph = {
      id: "wf-1",
      name: "demo",
      nodes: [
        {
          id: "t1",
          name: "Webhook",
          kind: "trigger",
          trigger_type: "webhook",
          params: {},
        },
        { id: "a1", name: "Send", kind: "action", action_type: "slack.send", params: {} },
        { id: "i1", name: "", kind: "if", params: {} },
      ],
      edges: [],
    };
    const { nodes } = deserializeGraph(wire);
    expect(nodes[0]?.data.subtype).toBe("webhook");
    expect(nodes[1]?.data.subtype).toBe("slack.send");
    // if node sans subtype, label par défaut
    expect(nodes[2]?.data.subtype).toBeUndefined();
    expect(nodes[2]?.data.label).toBe("If");
  });

  it("restores saved positions and lays out nodes without one (#225)", () => {
    const wire: WireGraph = {
      id: "wf-1",
      name: "demo",
      nodes: [
        { id: "t1", name: "", kind: "trigger", trigger_type: "manual", params: {}, position: { x: 412.5, y: -30 } },
        { id: "a1", name: "", kind: "action", action_type: "http_request", params: {} },
        // Position mal formée (graphe écrit à la main) ⇒ grille.
        { id: "a2", name: "", kind: "action", action_type: "http_request", params: {}, position: { x: "1", y: 2 } as never },
      ],
      edges: [],
    };
    const { nodes } = deserializeGraph(wire, (_id, idx) => ({ x: idx * 1000, y: 7 }));
    expect(nodes.map((n) => n.position)).toEqual([
      { x: 412.5, y: -30 },
      { x: 1000, y: 7 },
      { x: 2000, y: 7 },
    ]);
  });

  it("converts from_port back to sourceHandle", () => {
    const wire: WireGraph = {
      id: "wf-1",
      name: "demo",
      nodes: [],
      edges: [
        { from: "if-1", to: "b", from_port: "false" },
        { from: "a", to: "b" },
      ],
    };
    const { edges } = deserializeGraph(wire);
    expect(edges[0]?.sourceHandle).toBe("false");
    expect(edges[1]?.sourceHandle).toBeUndefined();
  });
});

describe("round-trip serialize ↔ deserialize", () => {
  it("preserves the graph through one full cycle", () => {
    const nodes: CanvasNode[] = [
      buildCanvasNode({
        id: "t1",
        kind: "trigger",
        subtype: "webhook",
        label: "Entry",
        position: { x: 0, y: 0 },
      }),
      buildCanvasNode({
        id: "i1",
        kind: "if",
        label: "Branch",
        position: { x: 100, y: 0 },
      }),
      buildCanvasNode({
        id: "a1",
        kind: "action",
        subtype: "slack.send",
        label: "Slack",
        position: { x: 200, y: 0 },
      }),
    ];
    const edges: CanvasEdge[] = [
      { id: "t1->i1", source: "t1", target: "i1" },
      { id: "i1[true]->a1", source: "i1", target: "a1", sourceHandle: "true" },
    ];
    const wire = serializeGraph("wf-1", "demo", nodes, edges);
    const rebuilt = deserializeGraph(wire);
    // Labels et kinds préservés
    expect(rebuilt.nodes.map((n) => n.data.kind)).toEqual([
      "trigger",
      "if",
      "action",
    ]);
    expect(rebuilt.nodes.map((n) => n.data.label)).toEqual([
      "Entry",
      "Branch",
      "Slack",
    ]);
    // Edges préservées (sourceHandle compris)
    expect(rebuilt.edges.map((e) => e.source)).toEqual(["t1", "i1"]);
    expect(rebuilt.edges[1]?.sourceHandle).toBe("true");
    // Positions préservées (#225)
    expect(rebuilt.nodes.map((n) => n.position)).toEqual([
      { x: 0, y: 0 },
      { x: 100, y: 0 },
      { x: 200, y: 0 },
    ]);
  });
});

describe("generateNodeId", () => {
  it("returns the first free id matching the kind prefix", () => {
    expect(generateNodeId("action", asSet([]))).toBe("action-1");
    expect(generateNodeId("action", asSet(["action-1"]))).toBe("action-2");
    expect(generateNodeId("trigger", asSet(["trigger-1", "trigger-2"]))).toBe(
      "trigger-3",
    );
  });
});
