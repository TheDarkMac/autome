// Test **end-to-end** de parcours utilisateur (F9.5).
//
// Là où les `*.test.tsx` couvrent chaque composant isolément et où
// `App.test.tsx` valide des transitions ponctuelles avec un mock par test,
// ce test traverse l'application assemblée (`<App />`) contre un **backend
// `fetch` simulé avec état**, routé par URL comme un vrai serveur.
//
// Parcours : session authentifiée → l'éditeur rouvre le dernier workflow
// enregistré (#225) → « Lancer » crée un run et bascule sur *Exécutions* →
// la `RunsList` affiche le run sélectionné → le **polling** (`useRunPolling`)
// surface la transition `Running → Succeeded` dans le `RunExecutionPanel`,
// puis s'éteint sur statut terminal. C'est le flux qui relie le plus de
// pièces (éditeur → `RunsList` → `useRunPolling` → `RunExecutionPanel`).
//
// Le polling est piloté par les **fake timers** Vitest : on évite `waitFor`
// (qui s'appuie sur de vrais `setTimeout`) et on flush microtasks + timers
// via `await act(() => vi.advanceTimersByTimeAsync(...))`, comme
// `useRunPolling.test.tsx`.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

import { App } from "./App";
import type { TokenPair } from "./auth";
import { memorySessionStore } from "./session";
import type { RunCheckpoint, RunStatus } from "./runs";
import type { UseSessionOptions } from "./useSession";
import { lastWorkflowKey } from "./useWorkflowEditor";

const WORKFLOW_ID = "wf-e2e";
const RUN_ID = "run-e2e";

const SAMPLE_SPEC = {
  openapi: "3.0.3",
  info: { title: "Autome API", version: "0.1.0" },
  paths: { "/workflows": {}, "/runs/{run_id}": {} },
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function b64url(obj: unknown): string {
  return btoa(JSON.stringify(obj))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/** Session authentifiée prête à l'emploi (access token longue durée). */
function authedSession(sub = "alice"): UseSessionOptions {
  const pair: TokenPair = {
    accessToken: `h.${b64url({ sub, exp: 9_999_999_999 })}.s`,
    refreshToken: "r.e.f",
  };
  return { store: memorySessionStore(pair) };
}

/** Checkpoint cohérent pour un statut donné (les champs suivent le statut). */
function checkpointFor(status: RunStatus): RunCheckpoint {
  const terminal = status === "Succeeded" || status === "Failed";
  return {
    run_id: RUN_ID,
    workflow_id: WORKFLOW_ID,
    version: 1,
    trigger_type: "manual",
    status,
    started_at_ms: status === "Queued" ? null : 1_000,
    finished_at_ms: terminal ? 2_000 : null,
    created_at_ms: 800,
    outputs: status === "Succeeded" ? { fetch: { value: 21 } } : {},
    next_node: status === "Running" ? "double" : null,
    error: null,
  };
}

const WORKFLOW = {
  id: WORKFLOW_ID,
  name: "Météo",
  nodes: [
    { id: "hook", name: "Webhook", kind: "trigger", trigger_type: "webhook", params: {}, position: { x: 0, y: 0 } },
    { id: "fetch", name: "Fetch", kind: "action", action_type: "http_request", params: {}, position: { x: 0, y: 160 } },
  ],
  edges: [{ from: "hook", to: "fetch" }],
};

/**
 * Backend `fetch` simulé avec état : un workflow enregistré ; `POST …/runs`
 * crée le run, puis la lecture `GET /runs/{id}` renvoie `Running` au premier
 * appel et `Succeeded` ensuite — on observe ainsi la transition via le
 * polling. Retourne un compteur des lectures du run.
 */
function installStatefulBackend(): { getRunCalls: () => number; runCreated: () => boolean } {
  let runReads = 0;
  let created = false;
  globalThis.fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : (input as Request).url;
    const method = init?.method ?? "GET";
    if (url.includes("/openapi.json")) return jsonResponse(SAMPLE_SPEC);
    if (url.includes("/catalog/nodes")) return new Response("", { status: 404 });
    if (url.includes("/workflows?")) {
      return jsonResponse({ items: [WORKFLOW], total: 1, offset: 0, limit: 100 });
    }
    if (url.endsWith(`/workflows/${WORKFLOW_ID}`)) return jsonResponse(WORKFLOW);
    if (url.includes(`/workflows/${WORKFLOW_ID}/runs`)) {
      if (method === "POST") {
        created = true;
        return jsonResponse({ run_id: RUN_ID, status: "queued" }, 202);
      }
      return jsonResponse({
        items: created ? [checkpointFor("Running")] : [],
        total: created ? 1 : 0,
        offset: 0,
        limit: 50,
      });
    }
    if (url.includes(`/runs/${RUN_ID}`)) {
      runReads += 1;
      return jsonResponse(checkpointFor(runReads >= 2 ? "Succeeded" : "Running"));
    }
    return new Response("", { status: 404 });
  }) as typeof fetch;
  return { getRunCalls: () => runReads, runCreated: () => created };
}

/** Flush microtasks + timers échus (fetch résolus, effets, polling). */
async function flush(ms = 0): Promise<void> {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

describe("parcours e2e — observation du cycle de vie d'un run", () => {
  let originalFetch: typeof fetch;
  beforeEach(() => {
    originalFetch = globalThis.fetch;
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    globalThis.fetch = originalFetch;
  });

  it("connecté → workflow rouvert → Lancer → run listé → polling Running puis Succeeded → arrêt", async () => {
    const backend = installStatefulBackend();
    const storage = {
      getItem: (k: string) => (k === lastWorkflowKey("alice") ? WORKFLOW_ID : null),
      setItem: () => undefined,
      removeItem: () => undefined,
    } as unknown as Storage;
    render(<App sessionOptions={authedSession()} editorOptions={{ storage }} />);

    // 1. Session présente → l'éditeur est monté (pas la page de login) et
    //    rouvre le dernier workflow enregistré.
    await flush();
    expect(
      screen.getByRole("heading", { name: "Autome" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("form", { name: "Connexion" }),
    ).not.toBeInTheDocument();
    expect(screen.getByTestId("nodes-count")).toHaveTextContent("2 node(s)");

    // 2. « Lancer » → run créé, bascule vers l'onglet Exécutions : la
    //    RunsList liste le run, déjà sélectionné.
    fireEvent.click(screen.getByRole("button", { name: "Lancer" }));
    await flush();
    expect(backend.runCreated()).toBe(true);
    expect(screen.getByTestId("runs-list")).toBeInTheDocument();
    expect(screen.getByTestId(`run-row-${RUN_ID}`)).toBeInTheDocument();

    // 3. Le run sélectionné est suivi par polling. 1ʳᵉ lecture : Running,
    //    surfacée dans le panneau d'exécution.
    expect(backend.getRunCalls()).toBe(1);
    expect(screen.getByTestId("run-execution-panel")).toHaveTextContent(
      /Run running/i,
    );

    // 4. Un intervalle de polling plus tard (2 000 ms par défaut) : 2ᵉ
    //    lecture → Succeeded, reflété dans le panneau.
    await flush(2_000);
    expect(backend.getRunCalls()).toBe(2);
    expect(screen.getByTestId("run-execution-panel")).toHaveTextContent(
      /Run succeeded/i,
    );

    // 5. Statut terminal → le polling s'éteint : aucune lecture
    //    supplémentaire même après plusieurs intervalles.
    await flush(10_000);
    expect(backend.getRunCalls()).toBe(2);
  });
});
