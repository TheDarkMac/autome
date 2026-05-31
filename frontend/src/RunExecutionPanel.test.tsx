import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { RunExecutionPanel } from "./RunExecutionPanel";
import type { RunCheckpoint } from "./runs";
import { buildCanvasNode } from "./graph";

function mkRun(over: Partial<RunCheckpoint> = {}): RunCheckpoint {
  return {
    run_id: "r-1",
    workflow_id: "wf-x",
    version: 1,
    trigger_type: "webhook",
    status: "Succeeded",
    started_at_ms: 1_000,
    finished_at_ms: 5_000,
    created_at_ms: 800,
    outputs: {},
    next_node: null,
    error: null,
    ...over,
  };
}

const noop = (): void => undefined;

describe("<RunExecutionPanel />", () => {
  it("affiche un message vide quand aucun checkpoint n'est sélectionné", () => {
    render(
      <RunExecutionPanel
        checkpoint={null}
        node={null}
        edges={[]}
        onClose={noop}
      />,
    );
    const panel = screen.getByTestId("run-execution-panel");
    expect(panel).toHaveAttribute("data-empty", "true");
    expect(panel).toHaveTextContent(/Sélectionnez un run/);
  });

  it("affiche un message intermédiaire quand un run est sélectionné mais pas de node", () => {
    render(
      <RunExecutionPanel
        checkpoint={mkRun({ status: "Succeeded" })}
        node={null}
        edges={[]}
        onClose={noop}
      />,
    );
    const panel = screen.getByTestId("run-execution-panel");
    expect(panel).toHaveAttribute("data-empty", "true");
    expect(panel).toHaveTextContent(/succeeded/);
  });

  it("affiche le statut d'exécution dérivé pour le node sélectionné", () => {
    const node = buildCanvasNode({ id: "n-2", kind: "action", subtype: "http_request" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({
          status: "Running",
          outputs: { "n-1": 1 },
          next_node: "n-2",
        })}
        node={node}
        edges={[{ from: "n-1", to: "n-2" }]}
        onClose={noop}
      />,
    );
    expect(screen.getByTestId("run-execution-panel")).toHaveAttribute(
      "data-status",
      "running",
    );
    expect(screen.getByTestId("exec-status-badge")).toHaveTextContent("running");
  });

  it("rend l'output JSON brut quand le node a produit une sortie", () => {
    const node = buildCanvasNode({ id: "n-1", kind: "trigger", subtype: "webhook" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({
          outputs: { "n-1": { ok: true, count: 42 } },
        })}
        node={node}
        edges={[]}
        onClose={noop}
      />,
    );
    const out = screen.getByTestId("output-json");
    expect(out.textContent).toContain('"ok": true');
    expect(out.textContent).toContain('"count": 42');
  });

  it("indique « pas exécuté » dans l'output quand le node est pending", () => {
    const node = buildCanvasNode({ id: "n-3", kind: "action", subtype: "http_request" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({ status: "Running", next_node: "n-1" })}
        node={node}
        edges={[]}
        onClose={noop}
      />,
    );
    expect(screen.getByTestId("output-empty")).toHaveTextContent(
      /Node non exécuté/,
    );
  });

  it("indique « payload du trigger » pour un node racine", () => {
    const node = buildCanvasNode({ id: "n-1", kind: "trigger", subtype: "webhook" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({ outputs: { "n-1": 1 } })}
        node={node}
        edges={[]}
        onClose={noop}
      />,
    );
    expect(screen.getByTestId("input-section")).toHaveTextContent(
      /Payload du trigger/,
    );
  });

  it("rend les outputs des prédécesseurs comme input", () => {
    const node = buildCanvasNode({ id: "n-3", kind: "action", subtype: "http_request" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({
          outputs: { "n-1": { v: 1 }, "n-2": { v: 2 } },
        })}
        node={node}
        edges={[
          { from: "n-1", to: "n-3" },
          { from: "n-2", to: "n-3" },
        ]}
        onClose={noop}
      />,
    );
    const inputJson = screen.getByTestId("input-json").textContent ?? "";
    expect(inputJson).toContain('"n-1"');
    expect(inputJson).toContain('"v": 1');
    expect(inputJson).toContain('"n-2"');
    expect(inputJson).toContain('"v": 2');
  });

  it("indique « aucune sortie de prédécesseur » si aucun pred n'a encore tourné", () => {
    const node = buildCanvasNode({ id: "n-2", kind: "action", subtype: "http_request" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({ status: "Running", next_node: "n-1", outputs: {} })}
        node={node}
        edges={[{ from: "n-1", to: "n-2" }]}
        onClose={noop}
      />,
    );
    expect(screen.getByTestId("input-empty")).toHaveTextContent(
      /Aucune sortie de prédécesseur/,
    );
  });

  it("affiche le bloc erreur quand le node est failed et que le run porte un error", () => {
    const node = buildCanvasNode({ id: "n-2", kind: "action", subtype: "http_request" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({
          status: "Failed",
          next_node: "n-2",
          error: "HTTP 500: upstream timeout",
        })}
        node={node}
        edges={[]}
        onClose={noop}
      />,
    );
    expect(screen.getByTestId("exec-error")).toHaveTextContent(
      /HTTP 500: upstream timeout/,
    );
  });

  it("appelle onClose au clic sur ×", () => {
    const onClose = vi.fn();
    const node = buildCanvasNode({ id: "n-1", kind: "trigger", subtype: "webhook" });
    render(
      <RunExecutionPanel
        checkpoint={mkRun({ outputs: { "n-1": 1 } })}
        node={node}
        edges={[]}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /Fermer/ }));
    expect(onClose).toHaveBeenCalledOnce();
  });
});
