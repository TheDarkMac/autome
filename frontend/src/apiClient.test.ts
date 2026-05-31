import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import { API_PATHS, buildPath, createApiClient } from "./apiClient";
import type { WireGraph } from "./graph";

interface Call {
  url: string;
  init: RequestInit;
}

function mockFetch(responses: Response[]): {
  fetchImpl: typeof fetch;
  calls: Call[];
} {
  const calls: Call[] = [];
  let i = 0;
  const fetchImpl = (async (input: RequestInfo | URL, init: RequestInit = {}) => {
    calls.push({ url: String(input), init });
    return responses[i++] ?? new Response(null, { status: 500 });
  }) as typeof fetch;
  return { fetchImpl, calls };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const WF: WireGraph = { id: "wf-1", name: "Demo", nodes: [], edges: [] };

describe("API_PATHS ↔ openapi.json (garde anti-drift)", () => {
  it("tous les chemins du client existent dans la spec OpenAPI", () => {
    const specPath = resolve(process.cwd(), "openapi.json");
    const spec = JSON.parse(readFileSync(specPath, "utf8")) as {
      paths: Record<string, unknown>;
    };
    const specPaths = new Set(Object.keys(spec.paths));
    for (const template of Object.values(API_PATHS)) {
      expect(specPaths.has(template), `chemin absent de la spec : ${template}`).toBe(
        true,
      );
    }
  });
});

describe("buildPath", () => {
  it("substitue et encode les paramètres", () => {
    expect(buildPath(API_PATHS.workflow, { id: "a/b" })).toBe("/workflows/a%2Fb");
    expect(buildPath(API_PATHS.callback, { provider: "slack" })).toBe(
      "/credentials/slack/authorize".replace("authorize", "callback"),
    );
  });
  it("lève si un paramètre manque", () => {
    expect(() => buildPath(API_PATHS.workflow, {})).toThrow(ApiError);
  });
});

describe("createApiClient — workflows", () => {
  it("listWorkflows : GET avec query, mappe les items en WireGraph", async () => {
    const { fetchImpl, calls } = mockFetch([
      json({ items: [WF], total: 1, offset: 0, limit: 50 }),
    ]);
    const page = await createApiClient(fetchImpl).listWorkflows({ limit: 50 });

    expect(calls[0]!.url).toContain("/workflows?limit=50");
    expect(calls[0]!.init.method).toBe("GET");
    expect(page.total).toBe(1);
    expect(page.items[0]).toEqual(WF);
  });

  it("getWorkflow : GET /workflows/{id} encodé", async () => {
    const { fetchImpl, calls } = mockFetch([json(WF)]);
    const wf = await createApiClient(fetchImpl).getWorkflow("wf 1");
    expect(calls[0]!.url).toContain("/workflows/wf%201");
    expect(wf).toEqual(WF);
  });

  it("createWorkflow : POST avec body JSON", async () => {
    const { fetchImpl, calls } = mockFetch([json(WF, 201)]);
    await createApiClient(fetchImpl).createWorkflow(WF);
    expect(calls[0]!.init.method).toBe("POST");
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual(WF);
    expect(new Headers(calls[0]!.init.headers).get("Content-Type")).toBe(
      "application/json",
    );
  });

  it("updateWorkflow : PUT /workflows/{id}", async () => {
    const { fetchImpl, calls } = mockFetch([json(WF)]);
    await createApiClient(fetchImpl).updateWorkflow("wf-1", WF);
    expect(calls[0]!.init.method).toBe("PUT");
    expect(calls[0]!.url).toContain("/workflows/wf-1");
  });

  it("deleteWorkflow : DELETE, 204 sans corps", async () => {
    const { fetchImpl, calls } = mockFetch([new Response(null, { status: 204 })]);
    await expect(
      createApiClient(fetchImpl).deleteWorkflow("wf-1"),
    ).resolves.toBeUndefined();
    expect(calls[0]!.init.method).toBe("DELETE");
  });

  it("mappe une réponse non-ok en ApiError avec le status", async () => {
    const { fetchImpl } = mockFetch([json({ error: "nope" }, 409)]);
    await expect(
      createApiClient(fetchImpl).createWorkflow(WF),
    ).rejects.toMatchObject({ name: "ApiError", status: 409 });
  });

  it("rejette un workflow mal formé (id/name manquants)", async () => {
    const { fetchImpl } = mockFetch([json({ nodes: [] })]);
    await expect(createApiClient(fetchImpl).getWorkflow("x")).rejects.toThrow(
      ApiError,
    );
  });
});

describe("createApiClient — runs & credentials", () => {
  it("createRun : POST /workflows/{id}/runs → {run_id, status}", async () => {
    const { fetchImpl, calls } = mockFetch([
      json({ run_id: "run-1", status: "queued" }, 202),
    ]);
    const res = await createApiClient(fetchImpl).createRun("wf-1", { version: 2 });
    expect(calls[0]!.url).toContain("/workflows/wf-1/runs");
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({ version: 2 });
    expect(res).toEqual({ run_id: "run-1", status: "queued" });
  });

  it("listProviders : GET /credentials/providers", async () => {
    const { fetchImpl, calls } = mockFetch([
      json({ items: [{ provider_id: "slack", scopes: ["chat:write"] }] }),
    ]);
    const res = await createApiClient(fetchImpl).listProviders();
    expect(calls[0]!.url).toContain("/credentials/providers");
    expect(res.items[0]!.provider_id).toBe("slack");
  });

  it("startAuthorize : POST /credentials/{provider}/authorize", async () => {
    const { fetchImpl, calls } = mockFetch([
      json({ authorize_url: "https://x", state: "s", code_verifier: "v" }),
    ]);
    const res = await createApiClient(fetchImpl).startAuthorize("gmail");
    expect(calls[0]!.url).toContain("/credentials/gmail/authorize");
    expect(res.state).toBe("s");
  });

  it("completeAuthorize : POST callback avec body", async () => {
    const { fetchImpl, calls } = mockFetch([new Response(null, { status: 200 })]);
    await createApiClient(fetchImpl).completeAuthorize("notion", {
      code: "c",
      state: "s",
      code_verifier: "v",
    });
    expect(calls[0]!.url).toContain("/credentials/notion/callback");
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({
      code: "c",
      state: "s",
      code_verifier: "v",
    });
  });

  it("deleteCredential : DELETE /credentials/{provider}", async () => {
    const { fetchImpl, calls } = mockFetch([new Response(null, { status: 204 })]);
    await createApiClient(fetchImpl).deleteCredential("github");
    expect(calls[0]!.init.method).toBe("DELETE");
    expect(calls[0]!.url).toContain("/credentials/github");
  });

  it("transmet l'authFetch injecté (le Bearer est porté par fetchImpl)", async () => {
    const authFetch = vi.fn(async () => json({ items: [] })) as unknown as typeof fetch;
    await createApiClient(authFetch).listCredentials();
    expect(authFetch).toHaveBeenCalledOnce();
  });
});
