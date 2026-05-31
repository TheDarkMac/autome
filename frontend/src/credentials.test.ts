import { describe, expect, it, vi } from "vitest";

import { fetchCredentials } from "./credentials";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("fetchCredentials", () => {
  it("appelle GET /credentials?provider=<x> avec encodage", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        items: [
          { id: "c1", name: "Workspace Acme", provider: "slack" },
          { id: "c2", name: "Personal", provider: "slack" },
        ],
      }),
    );
    const creds = await fetchCredentials("slack", fetchSpy as unknown as typeof fetch);
    expect(fetchSpy).toHaveBeenCalledOnce();
    const url = fetchSpy.mock.calls[0]?.[0] as string;
    expect(url).toContain("/credentials?provider=slack");
    expect(creds).toEqual([
      { id: "c1", name: "Workspace Acme", provider: "slack" },
      { id: "c2", name: "Personal", provider: "slack" },
    ]);
  });

  it("encode les caractères spéciaux du provider", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ items: [] }));
    await fetchCredentials("foo bar", fetchSpy as unknown as typeof fetch);
    expect(fetchSpy.mock.calls[0]?.[0]).toContain("provider=foo%20bar");
  });

  it("renvoie [] si le backend répond 404", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(new Response("", { status: 404 }));
    const creds = await fetchCredentials("slack", fetchSpy as unknown as typeof fetch);
    expect(creds).toEqual([]);
  });

  it("renvoie [] sur erreur de transport", async () => {
    const fetchSpy = vi.fn().mockRejectedValue(new Error("offline"));
    const creds = await fetchCredentials("slack", fetchSpy as unknown as typeof fetch);
    expect(creds).toEqual([]);
  });

  it("renvoie [] si la réponse n'est pas un tableau d'objets", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse({ items: "not-array" }));
    expect(
      await fetchCredentials("slack", fetchSpy as unknown as typeof fetch),
    ).toEqual([]);
  });

  it("filtre les entrées mal formées et conserve les valides", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      jsonResponse({
        items: [
          { id: "ok", name: "OK", provider: "slack" },
          { id: 42, name: "bad-id" },
          { name: "missing-id", provider: "slack" },
          null,
          "string-entry",
        ],
      }),
    );
    const creds = await fetchCredentials("slack", fetchSpy as unknown as typeof fetch);
    expect(creds).toEqual([{ id: "ok", name: "OK", provider: "slack" }]);
  });
});
