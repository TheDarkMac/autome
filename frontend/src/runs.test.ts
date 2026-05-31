import { describe, expect, it, vi } from "vitest";

import { buildCanvasNode, type WireEdge } from "./graph";
import {
  decorateNodesWithExecStatus,
  execStatusColor,
  fetchRun,
  fetchRunsForWorkflow,
  formatDuration,
  formatTime,
  inputForNode,
  isTerminalStatus,
  nodeExecutionStatus,
  type RunCheckpoint,
} from "./runs";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const baseCheckpoint: RunCheckpoint = {
  run_id: "r-1",
  workflow_id: "wf-x",
  version: 1,
  trigger_type: "webhook",
  status: "Running",
  started_at_ms: 1_000,
  finished_at_ms: null,
  created_at_ms: 500,
  outputs: {},
  next_node: null,
  error: null,
};

describe("isTerminalStatus", () => {
  it("renvoie true pour Succeeded/Failed", () => {
    expect(isTerminalStatus("Succeeded")).toBe(true);
    expect(isTerminalStatus("Failed")).toBe(true);
  });

  it("renvoie false pour Queued/Running", () => {
    expect(isTerminalStatus("Queued")).toBe(false);
    expect(isTerminalStatus("Running")).toBe(false);
  });
});

describe("formatDuration", () => {
  it("renvoie « — » quand started_at est null", () => {
    expect(formatDuration(null, null)).toBe("—");
    expect(formatDuration(null, 5_000)).toBe("—");
  });

  it("utilise nowMs si finished_at est null (run en cours)", () => {
    expect(formatDuration(1_000, null, 1_750)).toBe("750 ms");
  });

  it("formate en ms sous la seconde", () => {
    expect(formatDuration(1_000, 1_400)).toBe("400 ms");
  });

  it("formate en secondes avec 1 décimale entre 1s et 60s", () => {
    expect(formatDuration(1_000, 2_500)).toBe("1.5 s");
  });

  it("formate en min/s au-delà d'une minute", () => {
    expect(formatDuration(0, 65_000)).toBe("1 min 05 s");
    expect(formatDuration(0, 125_000)).toBe("2 min 05 s");
  });

  it("clampe à 0 si finished est antérieur à started (horloge sale)", () => {
    expect(formatDuration(5_000, 4_000)).toBe("0 ms");
  });
});

describe("formatTime", () => {
  it("formate en HH:MM avec padding zéro", () => {
    const d = new Date(2024, 5, 1, 7, 5, 0);
    expect(formatTime(d.getTime())).toBe("07:05");
  });
});

describe("nodeExecutionStatus", () => {
  it("succeeded si le node a produit une sortie", () => {
    const ck: RunCheckpoint = {
      ...baseCheckpoint,
      outputs: { "n-1": { hello: "world" } },
      next_node: "n-2",
    };
    expect(nodeExecutionStatus(ck, "n-1")).toBe("succeeded");
  });

  it("running si next_node correspond et run en Running", () => {
    const ck: RunCheckpoint = { ...baseCheckpoint, next_node: "n-2", status: "Running" };
    expect(nodeExecutionStatus(ck, "n-2")).toBe("running");
  });

  it("failed si next_node correspond et run en Failed", () => {
    const ck: RunCheckpoint = {
      ...baseCheckpoint,
      next_node: "n-2",
      status: "Failed",
      error: "boom",
    };
    expect(nodeExecutionStatus(ck, "n-2")).toBe("failed");
  });

  it("pending pour un node jamais atteint", () => {
    const ck: RunCheckpoint = { ...baseCheckpoint, next_node: "n-2" };
    expect(nodeExecutionStatus(ck, "n-3")).toBe("pending");
  });

  it("pending si run Queued (next_node éventuel mais pas running)", () => {
    const ck: RunCheckpoint = { ...baseCheckpoint, status: "Queued", next_node: "n-1" };
    expect(nodeExecutionStatus(ck, "n-1")).toBe("pending");
  });

  it("succeeded a priorité sur next_node (auto-loop hypothétique)", () => {
    const ck: RunCheckpoint = {
      ...baseCheckpoint,
      outputs: { "n-1": 1 },
      next_node: "n-1",
      status: "Running",
    };
    expect(nodeExecutionStatus(ck, "n-1")).toBe("succeeded");
  });
});

describe("inputForNode", () => {
  const edges: readonly WireEdge[] = [
    { from: "n-1", to: "n-2" },
    { from: "n-1", to: "n-3" },
    { from: "n-2", to: "n-3" },
  ];

  it("renvoie kind=trigger pour un node racine (aucun prédécesseur)", () => {
    const ck: RunCheckpoint = { ...baseCheckpoint, outputs: { "n-1": 42 } };
    expect(inputForNode(ck, edges, "n-1")).toEqual({ kind: "trigger" });
  });

  it("renvoie les outputs des prédécesseurs indexés par leur node_id", () => {
    const ck: RunCheckpoint = {
      ...baseCheckpoint,
      outputs: { "n-1": { v: 1 }, "n-2": { v: 2 } },
    };
    expect(inputForNode(ck, edges, "n-3")).toEqual({
      kind: "predecessors",
      inputs: { "n-1": { v: 1 }, "n-2": { v: 2 } },
    });
  });

  it("omet les prédécesseurs non encore exécutés", () => {
    const ck: RunCheckpoint = { ...baseCheckpoint, outputs: { "n-1": 1 } };
    expect(inputForNode(ck, edges, "n-3")).toEqual({
      kind: "predecessors",
      inputs: { "n-1": 1 },
    });
  });
});

describe("execStatusColor", () => {
  it("renvoie une couleur distincte par statut d'exécution", () => {
    const colors = new Set([
      execStatusColor("succeeded"),
      execStatusColor("running"),
      execStatusColor("failed"),
      execStatusColor("pending"),
    ]);
    expect(colors.size).toBe(4);
  });
});

describe("decorateNodesWithExecStatus", () => {
  it("ajoute borderColor et data.execStatus pour chaque node listé", () => {
    const nodes = [
      buildCanvasNode({ id: "n-1", kind: "trigger", subtype: "webhook" }),
      buildCanvasNode({ id: "n-2", kind: "action", subtype: "http_request" }),
    ];
    const decorated = decorateNodesWithExecStatus(nodes, {
      "n-1": "succeeded",
      "n-2": "failed",
    });
    expect(decorated[0]?.style?.borderColor).toBe(execStatusColor("succeeded"));
    expect(decorated[1]?.style?.borderColor).toBe(execStatusColor("failed"));
    expect(decorated[0]?.data["execStatus"]).toBe("succeeded");
    expect(decorated[1]?.data["execStatus"]).toBe("failed");
  });

  it("laisse les nodes sans statut intacts", () => {
    const nodes = [
      buildCanvasNode({ id: "n-1", kind: "trigger", subtype: "webhook" }),
    ];
    expect(decorateNodesWithExecStatus(nodes, {})).toEqual(nodes);
  });
});

describe("fetchRunsForWorkflow", () => {
  it("appelle GET /workflows/{id}/runs avec offset/limit par défaut", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        items: [
          {
            run_id: "r-1",
            workflow_id: "wf-x",
            version: 1,
            trigger_type: "webhook",
            status: "Succeeded",
            started_at_ms: 1_000,
            finished_at_ms: 2_500,
            created_at_ms: 800,
            outputs: { "n-1": { ok: true } },
            next_node: null,
            error: null,
          },
        ],
        total: 1,
        offset: 0,
        limit: 50,
      }),
    );
    const page = await fetchRunsForWorkflow(
      "wf-x",
      {},
      fetchSpy as unknown as typeof fetch,
    );
    expect(fetchSpy).toHaveBeenCalledOnce();
    const url = fetchSpy.mock.calls[0]?.[0] as string;
    expect(url).toContain("/workflows/wf-x/runs?offset=0&limit=50");
    expect(page.items).toHaveLength(1);
    expect(page.items[0]?.run_id).toBe("r-1");
    expect(page.total).toBe(1);
  });

  it("propage offset/limit explicites", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ items: [], total: 0, offset: 10, limit: 5 }));
    await fetchRunsForWorkflow(
      "wf-x",
      { offset: 10, limit: 5 },
      fetchSpy as unknown as typeof fetch,
    );
    const url = fetchSpy.mock.calls[0]?.[0] as string;
    expect(url).toContain("offset=10&limit=5");
  });

  it("encode l'ID de workflow", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ items: [], total: 0, offset: 0, limit: 50 }));
    await fetchRunsForWorkflow(
      "wf with space",
      {},
      fetchSpy as unknown as typeof fetch,
    );
    expect(fetchSpy.mock.calls[0]?.[0]).toContain("/workflows/wf%20with%20space/runs");
  });

  it("filtre les items mal formés sans casser la page", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        items: [
          // OK
          {
            run_id: "ok",
            workflow_id: "wf-x",
            status: "Succeeded",
            version: 1,
            trigger_type: "webhook",
            outputs: {},
            created_at_ms: 1,
          },
          // status inconnu
          { run_id: "bad", workflow_id: "wf-x", status: "Mystery" },
          // missing run_id
          { workflow_id: "wf-x", status: "Running" },
          null,
        ],
        total: 4,
        offset: 0,
        limit: 50,
      }),
    );
    const page = await fetchRunsForWorkflow(
      "wf-x",
      {},
      fetchSpy as unknown as typeof fetch,
    );
    expect(page.items.map((r) => r.run_id)).toEqual(["ok"]);
    expect(page.total).toBe(4);
  });

  it("lève ApiError sur réponse non-2xx", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(new Response("", { status: 500 }));
    await expect(
      fetchRunsForWorkflow("wf-x", {}, fetchSpy as unknown as typeof fetch),
    ).rejects.toMatchObject({ status: 500 });
  });

  it("lève ApiError sur erreur de transport", async () => {
    const fetchSpy = vi.fn().mockRejectedValue(new Error("offline"));
    await expect(
      fetchRunsForWorkflow("wf-x", {}, fetchSpy as unknown as typeof fetch),
    ).rejects.toThrow(/injoignables/);
  });
});

describe("fetchRun", () => {
  it("appelle GET /runs/{id} et parse le checkpoint", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        run_id: "r-9",
        workflow_id: "wf-x",
        version: 1,
        trigger_type: "schedule",
        status: "Running",
        started_at_ms: 100,
        finished_at_ms: null,
        created_at_ms: 50,
        outputs: { "n-1": { ok: true } },
        next_node: "n-2",
        error: null,
      }),
    );
    const ck = await fetchRun("r-9", fetchSpy as unknown as typeof fetch);
    expect(fetchSpy.mock.calls[0]?.[0]).toContain("/runs/r-9");
    expect(ck.run_id).toBe("r-9");
    expect(ck.next_node).toBe("n-2");
    expect(ck.outputs).toEqual({ "n-1": { ok: true } });
  });

  it("encode l'ID du run", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        run_id: "a/b",
        workflow_id: "wf-x",
        version: 1,
        trigger_type: "webhook",
        status: "Queued",
        outputs: {},
        created_at_ms: 1,
      }),
    );
    await fetchRun("a/b", fetchSpy as unknown as typeof fetch);
    expect(fetchSpy.mock.calls[0]?.[0]).toContain("/runs/a%2Fb");
  });

  it("lève ApiError sur 404", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(new Response("", { status: 404 }));
    await expect(
      fetchRun("missing", fetchSpy as unknown as typeof fetch),
    ).rejects.toMatchObject({ status: 404 });
  });

  it("lève ApiError sur payload mal formé", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse({ run_id: "x" }));
    await expect(
      fetchRun("x", fetchSpy as unknown as typeof fetch),
    ).rejects.toThrow(/mal formé/);
  });

  it("lève ApiError sur transport down", async () => {
    const fetchSpy = vi.fn().mockRejectedValue(new Error("ECONN"));
    await expect(
      fetchRun("x", fetchSpy as unknown as typeof fetch),
    ).rejects.toThrow(/injoignable/);
  });
});
