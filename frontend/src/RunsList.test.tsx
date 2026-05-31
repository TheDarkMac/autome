import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { RunsList } from "./RunsList";
import type { RunCheckpoint, RunListPage } from "./runs";

function mkRun(over: Partial<RunCheckpoint> = {}): RunCheckpoint {
  return {
    run_id: "r-0001",
    workflow_id: "wf-x",
    version: 1,
    trigger_type: "webhook",
    status: "Succeeded",
    started_at_ms: 1_000,
    finished_at_ms: 4_500,
    created_at_ms: 800,
    outputs: { "n-1": { ok: true } },
    next_node: null,
    error: null,
    ...over,
  };
}

function page(items: RunCheckpoint[], over: Partial<RunListPage> = {}): RunListPage {
  return {
    items,
    total: items.length,
    offset: 0,
    limit: 20,
    ...over,
  };
}

const noop = (): void => undefined;

describe("<RunsList />", () => {
  it("appelle fetch avec le workflowId et l'offset initial", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(page([]));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() => expect(fetchImpl).toHaveBeenCalledTimes(1));
    expect(fetchImpl).toHaveBeenCalledWith("wf-x", { offset: 0, limit: 20 });
  });

  it("affiche un message « aucun run » sur page vide", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(page([]));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByText(/Aucun run pour ce workflow/)).toBeInTheDocument(),
    );
  });

  it("rend une ligne par run avec son badge de statut", async () => {
    const items = [
      mkRun({ run_id: "ok", status: "Succeeded" }),
      mkRun({ run_id: "r-fail", status: "Failed" }),
      mkRun({ run_id: "r-run", status: "Running", finished_at_ms: null }),
      mkRun({ run_id: "r-q", status: "Queued", started_at_ms: null }),
    ];
    const fetchImpl = vi.fn().mockResolvedValue(page(items));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("run-row-ok")).toBeInTheDocument(),
    );
    expect(screen.getByTestId("badge-Succeeded")).toBeInTheDocument();
    expect(screen.getByTestId("badge-Failed")).toBeInTheDocument();
    expect(screen.getByTestId("badge-Running")).toBeInTheDocument();
    expect(screen.getByTestId("badge-Queued")).toBeInTheDocument();
  });

  it("propage onSelectRun au clic", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(page([mkRun({ run_id: "r-abc" })]));
    const onSelectRun = vi.fn();
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={onSelectRun}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("run-row-r-abc")).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByTestId("run-row-r-abc"));
    expect(onSelectRun).toHaveBeenCalledWith("r-abc");
  });

  it("marque la ligne sélectionnée via data-selected", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(page([mkRun({ run_id: "r-pick" })]));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId="r-pick"
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("run-row-r-pick")).toBeInTheDocument(),
    );
    expect(screen.getByTestId("run-row-r-pick")).toHaveAttribute(
      "data-selected",
      "true",
    );
  });

  it("affiche le message d'erreur si fetch rejette", async () => {
    const fetchImpl = vi.fn().mockRejectedValue(new Error("backend down"));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent(/backend down/),
    );
  });

  it("rafraîchit la liste au clic sur ↻", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(page([]));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() => expect(fetchImpl).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: /Rafraîchir/ }));
    await waitFor(() => expect(fetchImpl).toHaveBeenCalledTimes(2));
  });

  it("affiche la pagination uniquement si total > pageSize", async () => {
    const single = vi.fn().mockResolvedValue(page([mkRun()], { total: 1 }));
    const { unmount } = render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        pageSize={20}
        fetchImpl={single as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("run-row-r-0001")).toBeInTheDocument(),
    );
    expect(screen.queryByTestId("pagination-info")).toBeNull();
    unmount();

    const many = vi.fn().mockResolvedValue(page([mkRun()], { total: 42 }));
    render(
      <RunsList
        workflowId="wf-y"
        selectedRunId={null}
        onSelectRun={noop}
        pageSize={20}
        fetchImpl={many as never}
      />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("pagination-info")).toBeInTheDocument(),
    );
    expect(screen.getByTestId("pagination-info")).toHaveTextContent(
      /page 1 · 42 runs/,
    );
  });

  it("avance et recule via les boutons de pagination", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(page([mkRun()], { total: 50 }));
    render(
      <RunsList
        workflowId="wf-x"
        selectedRunId={null}
        onSelectRun={noop}
        pageSize={20}
        fetchImpl={fetchImpl as never}
      />,
    );
    await waitFor(() => expect(fetchImpl).toHaveBeenCalledTimes(1));

    fireEvent.click(screen.getByRole("button", { name: /Page suivante/ }));
    await waitFor(() =>
      expect(fetchImpl).toHaveBeenLastCalledWith("wf-x", {
        offset: 20,
        limit: 20,
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: /Page précédente/ }));
    await waitFor(() =>
      expect(fetchImpl).toHaveBeenLastCalledWith("wf-x", {
        offset: 0,
        limit: 20,
      }),
    );
  });
});
