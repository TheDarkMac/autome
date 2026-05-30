import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { NodeCanvas } from "./NodeCanvas";
import { buildCanvasNode, type WireGraph } from "./graph";

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

  it("appends a node when a palette button is clicked", () => {
    const onChange = vi.fn();
    render(<NodeCanvas onChange={onChange} />);
    fireEvent.click(screen.getByRole("button", { name: /Trigger · webhook/ }));
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("1 node(s)");
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    expect(wire.nodes[0]?.kind).toBe("trigger");
    expect(wire.nodes[0]?.trigger_type).toBe("webhook");
  });

  it("generates unique ids when the same kind is added repeatedly", () => {
    const onChange = vi.fn();
    render(<NodeCanvas onChange={onChange} />);
    const btn = screen.getByRole("button", { name: /Action · slack.send/ });
    fireEvent.click(btn);
    fireEvent.click(btn);
    fireEvent.click(btn);
    const wire = onChange.mock.calls.at(-1)?.[0] as WireGraph;
    const ids = wire.nodes.map((n) => n.id);
    expect(new Set(ids).size).toBe(3);
    expect(ids).toEqual(["action-1", "action-2", "action-3"]);
  });

  it("renders the React Flow viewport with controls + minimap (F8.2.d)", () => {
    render(<NodeCanvas />);
    // React Flow expose un nœud avec role/aria reconnaissable.
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
});
