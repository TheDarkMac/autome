import { describe, expect, it, vi } from "vitest";

import { createAuthFetch } from "./authFetch";
import type { TokenPair } from "./auth";

const PAIR: TokenPair = { accessToken: "access-1", refreshToken: "refresh-1" };
const PAIR2: TokenPair = { accessToken: "access-2", refreshToken: "refresh-2" };

/** fetch factice : renvoie successivement les réponses scriptées, enregistre les inits. */
function scriptedFetch(responses: Response[]): {
  fetchImpl: typeof fetch;
  calls: { url: string; init: RequestInit }[];
} {
  const calls: { url: string; init: RequestInit }[] = [];
  let i = 0;
  const fetchImpl = (async (input: RequestInfo | URL, init: RequestInit = {}) => {
    calls.push({ url: String(input), init });
    const res = responses[i++] ?? new Response(null, { status: 500 });
    return res;
  }) as typeof fetch;
  return { fetchImpl, calls };
}

function authHeader(init: RequestInit): string | null {
  return new Headers(init.headers).get("Authorization");
}

describe("createAuthFetch", () => {
  it("injecte le Bearer token sur une requête nominale", async () => {
    const { fetchImpl, calls } = scriptedFetch([new Response("ok", { status: 200 })]);
    const af = createAuthFetch({
      getTokens: () => PAIR,
      onTokens: vi.fn(),
      onUnauthenticated: vi.fn(),
      fetchImpl,
    });

    const res = await af("/x");
    expect(res.status).toBe(200);
    expect(calls).toHaveLength(1);
    expect(authHeader(calls[0]!.init)).toBe("Bearer access-1");
  });

  it("n'ajoute pas d'en-tête Authorization sans session", async () => {
    const { fetchImpl, calls } = scriptedFetch([new Response("ok", { status: 200 })]);
    const af = createAuthFetch({
      getTokens: () => null,
      onTokens: vi.fn(),
      onUnauthenticated: vi.fn(),
      fetchImpl,
    });

    await af("/x");
    expect(authHeader(calls[0]!.init)).toBeNull();
  });

  it("préserve un Authorization explicite du caller", async () => {
    const { fetchImpl, calls } = scriptedFetch([new Response("ok", { status: 200 })]);
    const af = createAuthFetch({
      getTokens: () => PAIR,
      onTokens: vi.fn(),
      onUnauthenticated: vi.fn(),
      fetchImpl,
    });

    await af("/x", { headers: { Authorization: "Bearer custom" } });
    expect(authHeader(calls[0]!.init)).toBe("Bearer custom");
  });

  it("sur 401, refresh puis rejoue la requête avec le nouveau token", async () => {
    const { fetchImpl, calls } = scriptedFetch([
      new Response("unauth", { status: 401 }),
      new Response("ok", { status: 200 }),
    ]);
    const onTokens = vi.fn();
    const refreshImpl = vi.fn(async () => PAIR2);
    const af = createAuthFetch({
      getTokens: () => PAIR,
      onTokens,
      onUnauthenticated: vi.fn(),
      refreshImpl: refreshImpl as never,
      fetchImpl,
    });

    const res = await af("/x");
    expect(res.status).toBe(200);
    expect(refreshImpl).toHaveBeenCalledOnce();
    expect(refreshImpl).toHaveBeenCalledWith("refresh-1", fetchImpl);
    expect(onTokens).toHaveBeenCalledWith(PAIR2);
    expect(calls).toHaveLength(2);
    expect(authHeader(calls[0]!.init)).toBe("Bearer access-1");
    expect(authHeader(calls[1]!.init)).toBe("Bearer access-2");
  });

  it("renvoie le 401 d'origine et signale la déconnexion si le refresh échoue", async () => {
    const { fetchImpl, calls } = scriptedFetch([new Response("unauth", { status: 401 })]);
    const onUnauthenticated = vi.fn();
    const refreshImpl = vi.fn(async () => {
      throw new Error("refresh expiré");
    });
    const af = createAuthFetch({
      getTokens: () => PAIR,
      onTokens: vi.fn(),
      onUnauthenticated,
      refreshImpl: refreshImpl as never,
      fetchImpl,
    });

    const res = await af("/x");
    expect(res.status).toBe(401);
    expect(onUnauthenticated).toHaveBeenCalledOnce();
    expect(calls).toHaveLength(1); // pas de rejeu
  });

  it("ne tente pas de refresh si aucun token n'est disponible", async () => {
    const { fetchImpl } = scriptedFetch([new Response("unauth", { status: 401 })]);
    const refreshImpl = vi.fn();
    const onUnauthenticated = vi.fn();
    const af = createAuthFetch({
      getTokens: () => null,
      onTokens: vi.fn(),
      onUnauthenticated,
      refreshImpl: refreshImpl as never,
      fetchImpl,
    });

    const res = await af("/x");
    expect(res.status).toBe(401);
    expect(refreshImpl).not.toHaveBeenCalled();
    expect(onUnauthenticated).not.toHaveBeenCalled();
  });

  it("refresh single-flight : un seul /auth/refresh pour deux 401 concurrents", async () => {
    const { fetchImpl } = scriptedFetch([
      new Response("unauth", { status: 401 }),
      new Response("unauth", { status: 401 }),
      new Response("a", { status: 200 }),
      new Response("b", { status: 200 }),
    ]);
    const refreshImpl = vi.fn(async () => PAIR2);
    const af = createAuthFetch({
      getTokens: () => PAIR,
      onTokens: vi.fn(),
      onUnauthenticated: vi.fn(),
      refreshImpl: refreshImpl as never,
      fetchImpl,
    });

    const [r1, r2] = await Promise.all([af("/a"), af("/b")]);
    expect(r1.status).toBe(200);
    expect(r2.status).toBe(200);
    expect(refreshImpl).toHaveBeenCalledOnce();
  });
});
