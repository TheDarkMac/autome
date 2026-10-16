import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import { API_PATHS, buildPath, canAdministerUsers, createApiClient } from "./apiClient";
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

  it("listWebhooks / rotateWebhook : chemins encodés (#202)", async () => {
    const item = { node_id: "n 1", name: "n", method: "POST", path: "/hooks/x", signing_secret: "s" };
    const { fetchImpl, calls } = mockFetch([json({ items: [item] }), json(item)]);
    const client = createApiClient(fetchImpl);
    expect((await client.listWebhooks("wf-1")).items).toEqual([item]);
    expect(calls[0]!.url).toMatch(/\/workflows\/wf-1\/webhooks$/);
    expect(await client.rotateWebhook("wf-1", "n 1")).toEqual(item);
    expect(calls[1]!.url).toMatch(/\/workflows\/wf-1\/webhooks\/n%201\/rotate$/);
    expect(calls[1]!.init.method).toBe("POST");
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

describe("createApiClient — administration (#208)", () => {
  const ALICE = {
    id: "u-1",
    email: "alice@example.io",
    role: "viewer",
    permissions: ["runs.read", "workflows.read"],
    disabled: false,
    created_at_ms: 0,
  };

  it("me : GET /auth/me", async () => {
    const me = { id: "u-1", email: "a@b.io", role: "admin", permissions: ["*"] };
    const { fetchImpl, calls } = mockFetch([json(me)]);
    expect(await createApiClient(fetchImpl).me()).toEqual(me);
    expect(calls[0]!.url).toMatch(/\/auth\/me$/);
  });

  it("listUsers : GET /admin/users avec filtre rogné et pagination", async () => {
    const { fetchImpl, calls } = mockFetch([
      json({ items: [ALICE], total: 1, offset: 0, limit: 10 }),
      json({ items: [], total: 0, offset: 0, limit: 50 }),
    ]);
    const client = createApiClient(fetchImpl);
    const page = await client.listUsers({ q: " ali ", offset: 0, limit: 10 });
    expect(calls[0]!.url).toContain("/admin/users?q=ali&offset=0&limit=10");
    expect(page.items[0]!.email).toBe("alice@example.io");
    await client.listUsers({ q: "  " });
    expect(calls[1]!.url).toMatch(/\/admin\/users$/);
  });

  it("setUserRole / disableUser / enableUser / logoutUser", async () => {
    const { fetchImpl, calls } = mockFetch([
      json(ALICE),
      json({ ...ALICE, disabled: true }),
      json(ALICE),
      new Response(null, { status: 204 }),
    ]);
    const client = createApiClient(fetchImpl);
    await client.setUserRole("u/1", "viewer");
    expect(calls[0]!.init.method).toBe("PUT");
    expect(calls[0]!.url).toContain("/admin/users/u%2F1/role");
    expect(JSON.parse(String(calls[0]!.init.body))).toEqual({ role: "viewer" });
    expect((await client.disableUser("u-1")).disabled).toBe(true);
    expect(calls[1]!.url).toContain("/admin/users/u-1/disable");
    await client.enableUser("u-1");
    expect(calls[2]!.url).toContain("/admin/users/u-1/enable");
    await client.logoutUser("u-1");
    expect(calls[3]!.init.method).toBe("POST");
    expect(calls[3]!.url).toContain("/admin/users/u-1/logout-all");
  });

  it("un refus garde son statut HTTP", async () => {
    const { fetchImpl } = mockFetch([new Response("forbidden", { status: 409 })]);
    await expect(createApiClient(fetchImpl).disableUser("me")).rejects.toMatchObject({
      status: 409,
    });
  });

  it("canAdministerUsers : `users.admin` ou `*`", () => {
    expect(canAdministerUsers(["*"])).toBe(true);
    expect(canAdministerUsers(["users.admin"])).toBe(true);
    expect(canAdministerUsers(["workflows.read", "workflows.write"])).toBe(false);
    expect(canAdministerUsers([])).toBe(false);
  });
});
