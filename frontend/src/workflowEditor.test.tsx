// Enregistrer, rouvrir et lancer un workflow depuis l'éditeur (#225).
//
// `<App />` assemblée contre un backend `fetch` simulé avec état (workflows
// et runs en mémoire, routés par méthode + URL) : on vérifie les requêtes
// réellement émises (POST puis PUT, positions dans le corps, run créé) et
// ce que l'utilisateur voit (état, onglet Exécutions, erreurs).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import { App } from "./App";
import type { TokenPair } from "./auth";
import { DND_MIME, STATIC_CATALOG, type CatalogItem } from "./catalog";
import type { WireGraph } from "./graph";
import { memorySessionStore } from "./session";
import { lastWorkflowKey } from "./useWorkflowEditor";

const SPEC = {
  openapi: "3.0.3",
  info: { title: "Autome API", version: "0.1.0" },
  paths: { "/workflows": {} },
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function b64url(obj: unknown): string {
  return btoa(JSON.stringify(obj)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function session() {
  const pair: TokenPair = {
    accessToken: `h.${b64url({ sub: "alice", exp: 9_999_999_999 })}.s`,
    refreshToken: "r",
  };
  return { store: memorySessionStore(pair), logoutImpl: vi.fn().mockResolvedValue(undefined) };
}

function memoryStorage(initial: Record<string, string> = {}): Storage {
  const m = new Map(Object.entries(initial));
  return {
    get length() {
      return m.size;
    },
    clear: () => m.clear(),
    getItem: (k) => m.get(k) ?? null,
    key: (i) => [...m.keys()][i] ?? null,
    removeItem: (k) => void m.delete(k),
    setItem: (k, v) => void m.set(k, v),
  };
}

interface Call {
  method: string;
  path: string;
  body: unknown;
}

/** Backend simulé : workflows + runs en mémoire ; `calls` garde l'historique. */
function installBackend(opts: {
  workflows?: WireGraph[];
  permissions?: string[];
  createStatus?: number[];
  updateStatus?: number;
} = {}) {
  const workflows = new Map((opts.workflows ?? []).map((w) => [w.id, w]));
  const runs: Record<string, unknown>[] = [];
  const calls: Call[] = [];
  const createStatus = [...(opts.createStatus ?? [])];
  globalThis.fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(typeof input === "string" ? input : (input as Request).url);
    const method = init?.method ?? "GET";
    const path = url.pathname;
    const body = typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
    calls.push({ method, path, body });
    if (path === "/openapi.json") return json(SPEC);
    if (path === "/auth/me") {
      return json({ id: "alice", email: "alice@example.io", role: "editor", permissions: opts.permissions ?? ["*"] });
    }
    if (path === "/workflows" && method === "GET") {
      const items = [...workflows.values()];
      return json({ items, total: items.length, offset: 0, limit: 100 });
    }
    if (path === "/workflows" && method === "POST") {
      const status = createStatus.shift();
      if (status) return new Response("refusé", { status });
      const wf = body as WireGraph;
      if (workflows.has(wf.id)) return new Response("déjà", { status: 409 });
      workflows.set(wf.id, wf);
      return json(wf, 201);
    }
    const one = /^\/workflows\/([^/]+)$/.exec(path);
    if (one) {
      const id = decodeURIComponent(one[1]!);
      if (method === "GET") return workflows.has(id) ? json(workflows.get(id)) : new Response("", { status: 404 });
      if (method === "PUT") {
        if (opts.updateStatus) return new Response("refusé", { status: opts.updateStatus });
        workflows.set(id, body as WireGraph);
        return json(body);
      }
      if (method === "DELETE") {
        workflows.delete(id);
        return new Response(null, { status: 204 });
      }
    }
    const runsOf = /^\/workflows\/([^/]+)\/runs$/.exec(path);
    if (runsOf) {
      const id = decodeURIComponent(runsOf[1]!);
      if (method === "POST") {
        const run = {
          run_id: `run-${runs.length + 1}`,
          workflow_id: id,
          version: 1,
          trigger_type: "manual",
          status: "Queued",
          started_at_ms: null,
          finished_at_ms: null,
          created_at_ms: 1_000,
          outputs: {},
          next_node: null,
          error: null,
        };
        runs.push(run);
        return json({ run_id: run.run_id, status: "queued" }, 202);
      }
      const items = runs.filter((r) => r.workflow_id === id);
      return json({ items, total: items.length, offset: 0, limit: 20 });
    }
    const run = /^\/runs\/([^/]+)$/.exec(path);
    if (run) {
      const found = runs.find((r) => r.run_id === run[1]);
      return found ? json(found) : new Response("", { status: 404 });
    }
    if (/^\/workflows\/[^/]+\/webhooks$/.test(path)) return json({ items: [] });
    return new Response("", { status: 404 });
  }) as typeof fetch;
  const writes = () => calls.filter((c) => c.method !== "GET");
  return { workflows, calls, writes };
}

function dropNode(itemId: string, x = 200, y = 150): void {
  const item = STATIC_CATALOG.find((i) => i.id === itemId) as CatalogItem;
  const store = new Map([[DND_MIME, JSON.stringify(item)]]);
  fireEvent.drop(screen.getByTestId("node-canvas"), {
    clientX: x,
    clientY: y,
    dataTransfer: {
      getData: (t: string) => store.get(t) ?? "",
      setData: () => undefined,
      dropEffect: "move",
      effectAllowed: "move",
    } as unknown as DataTransfer,
  });
}

const bar = () => within(screen.getByTestId("workflow-bar"));
const button = (name: string) => bar().getByRole("button", { name });

const SAVED: WireGraph = {
  id: "wf-saved",
  name: "Facturation",
  nodes: [
    { id: "trigger-1", name: "Manuel", kind: "trigger", trigger_type: "manual", params: {}, position: { x: 412, y: 96 } },
    { id: "action-1", name: "Appel", kind: "action", action_type: "http_request", params: { method: "GET" }, position: { x: 412, y: 260 } },
  ],
  edges: [{ from: "trigger-1", to: "action-1" }],
};

describe("éditeur : enregistrer, rouvrir, lancer (#225)", () => {
  let originalFetch: typeof fetch;
  beforeEach(() => {
    originalFetch = globalThis.fetch;
  });
  afterEach(() => {
    globalThis.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  it("crée le workflow au premier enregistrement (POST) puis le remplace (PUT)", async () => {
    const backend = installBackend();
    const storage = memoryStorage();
    render(<App sessionOptions={session()} editorOptions={{ storage }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Jamais enregistré");

    dropNode("trigger.webhook");
    fireEvent.change(bar().getByLabelText("Nom du workflow"), { target: { value: "  Relances  " } });
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });

    const [post] = backend.writes();
    expect(post).toMatchObject({ method: "POST", path: "/workflows" });
    const created = post!.body as WireGraph;
    expect(created.id).toMatch(/^wf-[0-9a-f]{12}$/);
    expect(created.name).toBe("Relances");
    expect(created.nodes).toHaveLength(1);
    expect(created.nodes[0]).toMatchObject({ kind: "trigger", trigger_type: "webhook" });
    // jsdom ne mesure rien : la position du drop n'est pas finie et n'est
    // donc pas envoyée (jamais de `null`, refusé par le backend).
    expect(JSON.stringify(created)).not.toContain("null");
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Enregistré");
    expect(button("Enregistrer")).toBeDisabled();
    expect(bar().getByRole("combobox", { name: "Workflow ouvert" })).toHaveDisplayValue("Relances");
    expect(storage.getItem(lastWorkflowKey("alice"))).toBe(created.id);

    dropNode("action.http_request", 300, 300);
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Modifications non enregistrées");
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });
    const put = backend.writes()[1]!;
    expect(put).toMatchObject({ method: "PUT", path: `/workflows/${created.id}` });
    expect((put.body as WireGraph).nodes).toHaveLength(2);
    expect(backend.workflows.size).toBe(1);
  });

  it("rouvre le dernier workflow avec ses positions, et les renvoie à l'enregistrement", async () => {
    const backend = installBackend({ workflows: [SAVED] });
    const storage = memoryStorage({ [lastWorkflowKey("alice")]: "wf-saved" });
    render(<App sessionOptions={session()} editorOptions={{ storage }} />);

    await waitFor(() => expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)"));
    expect(screen.getByTestId("edges-count")).toHaveTextContent("1 edge(s)");
    expect(bar().getByLabelText("Nom du workflow")).toHaveValue("Facturation");
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Enregistré");

    fireEvent.change(bar().getByLabelText("Nom du workflow"), { target: { value: "Facturation v2" } });
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });
    const put = backend.writes()[0]!;
    expect(put.method).toBe("PUT");
    expect(put.body).toEqual({ ...SAVED, name: "Facturation v2" });
  });

  it("« Lancer » enregistre d'abord, crée le run et ouvre l'onglet Exécutions dessus", async () => {
    const backend = installBackend();
    render(<App sessionOptions={session()} editorOptions={{ storage: null }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    expect(button("Lancer")).toBeDisabled(); // aucun node

    dropNode("trigger.webhook");
    await act(async () => {
      fireEvent.click(button("Lancer"));
    });

    const [post, run] = backend.writes();
    const id = (post!.body as WireGraph).id;
    expect(post!.method).toBe("POST");
    expect(run).toMatchObject({ method: "POST", path: `/workflows/${id}/runs` });
    expect(screen.getByRole("tab", { name: "Exécutions" })).toHaveAttribute("aria-selected", "true");
    expect(await screen.findByTestId("run-row-run-1")).toBeInTheDocument();
    await waitFor(() =>
      expect(backend.calls.some((c) => c.method === "GET" && c.path === "/runs/run-1")).toBe(true),
    );
    expect(screen.getByTestId("run-execution-panel")).toHaveTextContent(/run-1|queued/i);
  });

  it("un workflow enregistré et inchangé est lancé sans nouvel enregistrement", async () => {
    const backend = installBackend({ workflows: [SAVED] });
    const storage = memoryStorage({ [lastWorkflowKey("alice")]: "wf-saved" });
    render(<App sessionOptions={session()} editorOptions={{ storage }} />);
    await waitFor(() => expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)"));
    await act(async () => {
      fireEvent.click(button("Lancer"));
    });
    expect(backend.writes()).toEqual([
      { method: "POST", path: "/workflows/wf-saved/runs", body: {} },
    ]);
  });

  it("un id déjà pris dans l'espace est remplacé par un autre (409)", async () => {
    const backend = installBackend({ createStatus: [409] });
    render(<App sessionOptions={session()} editorOptions={{ storage: null }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    dropNode("trigger.webhook");
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });
    const [first, second] = backend.writes();
    expect((first!.body as WireGraph).id).not.toBe((second!.body as WireGraph).id);
    expect(backend.workflows.has((second!.body as WireGraph).id)).toBe(true);
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Enregistré");
  });

  it("affiche l'échec d'enregistrement et reste « non enregistré »", async () => {
    installBackend({ createStatus: [403] });
    render(<App sessionOptions={session()} editorOptions={{ storage: null }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    dropNode("trigger.webhook");
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });
    expect(bar().getByRole("alert")).toHaveTextContent("Enregistrement impossible : droits insuffisants.");
    expect(bar().getByTestId("workflow-state")).toHaveTextContent("Jamais enregistré");
  });

  it("refuse d'enregistrer sans nom", async () => {
    const backend = installBackend();
    render(<App sessionOptions={session()} editorOptions={{ storage: null }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    fireEvent.change(bar().getByLabelText("Nom du workflow"), { target: { value: "   " } });
    await act(async () => {
      fireEvent.click(button("Enregistrer"));
    });
    expect(bar().getByRole("alert")).toHaveTextContent(/Donnez un nom/);
    expect(backend.writes()).toEqual([]);
  });

  it("demande confirmation avant d'abandonner des modifications", async () => {
    installBackend({ workflows: [SAVED] });
    const confirmImpl = vi.fn().mockReturnValue(false);
    render(<App sessionOptions={session()} editorOptions={{ storage: null, confirmImpl }} />);
    await waitFor(() => expect(bar().getByRole("option", { name: "Facturation" })).toBeInTheDocument());
    dropNode("trigger.webhook");

    fireEvent.change(bar().getByRole("combobox", { name: "Workflow ouvert" }), { target: { value: "wf-saved" } });
    fireEvent.click(button("Nouveau"));
    expect(confirmImpl).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("1 node(s)");

    confirmImpl.mockReturnValue(true);
    fireEvent.change(bar().getByRole("combobox", { name: "Workflow ouvert" }), { target: { value: "wf-saved" } });
    await waitFor(() => expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)"));
  });

  it("supprime le workflow après confirmation et repart d'un nouveau", async () => {
    const backend = installBackend({ workflows: [SAVED] });
    const storage = memoryStorage({ [lastWorkflowKey("alice")]: "wf-saved" });
    const confirmImpl = vi.fn().mockReturnValue(true);
    render(<App sessionOptions={session()} editorOptions={{ storage, confirmImpl }} />);
    await waitFor(() => expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)"));
    await act(async () => {
      fireEvent.click(button("Supprimer"));
    });
    expect(confirmImpl).toHaveBeenCalledWith("Supprimer le workflow « Facturation » ?");
    expect(backend.writes()).toEqual([{ method: "DELETE", path: "/workflows/wf-saved", body: undefined }]);
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("0 node(s)");
    expect(bar().queryByRole("option", { name: "Facturation" })).not.toBeInTheDocument();
    expect(storage.getItem(lastWorkflowKey("alice"))).toBeNull();
  });

  it("sans `workflows.write` ni `runs.execute`, les actions sont désactivées", async () => {
    installBackend({ workflows: [SAVED], permissions: ["workflows.read", "runs.read"] });
    const storage = memoryStorage({ [lastWorkflowKey("alice")]: "wf-saved" });
    render(<App sessionOptions={session()} editorOptions={{ storage }} />);
    await waitFor(() => expect(screen.getByTestId("current-user")).toHaveTextContent("alice@example.io"));
    await waitFor(() => expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)"));
    expect(button("Enregistrer")).toBeDisabled();
    expect(button("Lancer")).toBeDisabled();
    expect(button("Supprimer")).toBeDisabled();
  });

  it("l'onglet Exécutions d'un workflow jamais enregistré n'interroge pas le serveur", async () => {
    const backend = installBackend();
    render(<App sessionOptions={session()} editorOptions={{ storage: null }} />);
    await waitFor(() => expect(button("Enregistrer")).toBeEnabled());
    fireEvent.click(screen.getByRole("tab", { name: "Exécutions" }));
    expect(screen.getByText("Enregistrez le workflow pour voir ses exécutions.")).toBeInTheDocument();
    expect(backend.calls.some((c) => c.path.endsWith("/runs"))).toBe(false);
  });
});
