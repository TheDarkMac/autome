import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

import { NodeCanvas, type NodeCanvasController } from "./NodeCanvas";
import { DND_MIME, STATIC_CATALOG, type CatalogItem } from "./catalog";
import { buildCanvasNode, type WireGraph } from "./graph";

/** Construit un DataTransfer compatible jsdom pour `fireEvent.drop`. */
function makeDataTransfer(item: CatalogItem): DataTransfer {
  const store = new Map<string, string>();
  store.set(DND_MIME, JSON.stringify(item));
  return {
    getData: (type: string) => store.get(type) ?? "",
    setData: (type: string, value: string) => {
      store.set(type, value);
    },
    dropEffect: "move",
    effectAllowed: "move",
  } as unknown as DataTransfer;
}

describe("<NodeCanvas />", () => {
  it("starts empty and exposes a serialized graph", () => {
    const onChange = vi.fn();
    render(<NodeCanvas onChange={onChange} />);
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("0 node(s)");
    expect(screen.getByTestId("edges-count")).toHaveTextContent("0 edge(s)");
    const lastCall = onChange.mock.calls.at(-1)?.[0] as WireGraph | undefined;
    expect(lastCall?.nodes).toHaveLength(0);
    expect(lastCall?.edges).toHaveLength(0);
  });

  it("appends a node when a palette item is dropped on the canvas (F8.3.b)", () => {
    const onChange = vi.fn();
    const item = STATIC_CATALOG.find((it) => it.id === "trigger.webhook")!;
    render(<NodeCanvas onChange={onChange} />);
    const target = screen.getByTestId("node-canvas");
    fireEvent.drop(target, {
      clientX: 200,
      clientY: 150,
      dataTransfer: makeDataTransfer(item),
    });
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("1 node(s)");
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    expect(wire.nodes[0]?.kind).toBe("trigger");
    expect(wire.nodes[0]?.trigger_type).toBe("webhook");
  });

  it("ignores drops without the autome MIME", () => {
    const onChange = vi.fn();
    render(<NodeCanvas onChange={onChange} />);
    const target = screen.getByTestId("node-canvas");
    fireEvent.drop(target, {
      clientX: 0,
      clientY: 0,
      dataTransfer: {
        getData: () => "",
        setData: () => undefined,
        dropEffect: "move",
        effectAllowed: "move",
      } as unknown as DataTransfer,
    });
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("0 node(s)");
  });

  it("generates unique ids when the same kind is dropped repeatedly", () => {
    const onChange = vi.fn();
    const item = STATIC_CATALOG.find((it) => it.id === "action.slack.send")!;
    render(<NodeCanvas onChange={onChange} />);
    const target = screen.getByTestId("node-canvas");
    for (let i = 0; i < 3; i += 1) {
      fireEvent.drop(target, {
        clientX: 100 + i * 10,
        clientY: 100,
        dataTransfer: makeDataTransfer(item),
      });
    }
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    const ids = wire.nodes.map((n) => n.id);
    expect(new Set(ids).size).toBe(3);
    expect(ids).toEqual(["action-1", "action-2", "action-3"]);
  });

  it("renders the React Flow viewport with controls + minimap (F8.2.d)", () => {
    render(<NodeCanvas />);
    expect(document.querySelector(".react-flow")).not.toBeNull();
    expect(document.querySelector(".react-flow__controls")).not.toBeNull();
    expect(document.querySelector(".react-flow__minimap")).not.toBeNull();
  });

  it("rehydrates from initialNodes/initialEdges", () => {
    const node = buildCanvasNode({
      id: "t1",
      kind: "trigger",
      subtype: "webhook",
      label: "Entry",
    });
    render(<NodeCanvas initialNodes={[node]} />);
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("1 node(s)");
  });

  it("applique les defaultParams du schéma à l'ajout par drop (F8.4.b)", () => {
    const onChange = vi.fn();
    const item = STATIC_CATALOG.find((it) => it.id === "trigger.webhook")!;
    render(<NodeCanvas onChange={onChange} />);
    fireEvent.drop(screen.getByTestId("node-canvas"), {
      clientX: 200,
      clientY: 150,
      dataTransfer: makeDataTransfer(item),
    });
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    // method=POST est dans defaultParams(webhook) — propagé via params sérialisé.
    expect(wire.nodes[0]?.params).toEqual({ method: "POST" });
  });

  it("expose un controller patchNodeParams qui met à jour le node ciblé", () => {
    let ctrl: NodeCanvasController | null = null;
    const onChange = vi.fn();
    const initial = buildCanvasNode({
      id: "n-1",
      kind: "trigger",
      subtype: "webhook",
      params: { method: "POST" },
    });
    render(
      <NodeCanvas
        initialNodes={[initial]}
        onChange={onChange}
        onReady={(c) => {
          ctrl = c;
        }}
      />,
    );
    expect(ctrl).not.toBeNull();
    act(() => {
      ctrl!.patchNodeParams("n-1", { method: "GET", path: "/x" });
    });
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    expect(wire.nodes[0]?.params).toEqual({ method: "GET", path: "/x" });
  });
});
