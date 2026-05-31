import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import {
  clearChallenge,
  completeAuthorize,
  fetchConnectedCredentials,
  fetchProviders,
  loadChallenge,
  parseCallbackParams,
  revokeCredential,
  saveChallenge,
  startAuthorize,
  type AuthorizeChallenge,
} from "./connectors";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const TOKEN = "a.b.c";

describe("fetchProviders", () => {
  it("parse les providers et envoie le Bearer", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(
      json({
        items: [
          { provider_id: "slack", scopes: ["chat:write"] },
          { provider_id: "github", scopes: [] },
        ],
      }),
    );
    const providers = await fetchProviders(TOKEN, fetchImpl);
    expect(providers).toEqual([
      { providerId: "slack", scopes: ["chat:write"] },
      { providerId: "github", scopes: [] },
    ]);
    expect(fetchImpl.mock.calls[0]![1].headers.Authorization).toBe("Bearer a.b.c");
  });

  it("ignore les entrées sans provider_id", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(json({ items: [{ scopes: [] }, { provider_id: "ok" }] }));
    expect(await fetchProviders(TOKEN, fetchImpl)).toEqual([
      { providerId: "ok", scopes: [] },
    ]);
  });

  it("lève sur 401", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(json({}, 401));
    await expect(fetchProviders(TOKEN, fetchImpl)).rejects.toMatchObject({
      status: 401,
    });
  });
});

describe("fetchConnectedCredentials", () => {
  it("parse les credentials connectés", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(
      json({
        items: [
          {
            provider_id: "slack",
            has_refresh_token: true,
            scope: "chat:write",
            expires_at_ms: 1234,
          },
        ],
      }),
    );
    expect(await fetchConnectedCredentials(TOKEN, fetchImpl)).toEqual([
      {
        providerId: "slack",
        hasRefreshToken: true,
        scope: "chat:write",
        expiresAtMs: 1234,
      },
    ]);
  });

  it("tolère les champs optionnels absents", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(json({ items: [{ provider_id: "github" }] }));
    expect(await fetchConnectedCredentials(TOKEN, fetchImpl)).toEqual([
      {
        providerId: "github",
        hasRefreshToken: false,
        scope: null,
        expiresAtMs: null,
      },
    ]);
  });
});

describe("startAuthorize", () => {
  it("POST authorize et renvoie le challenge", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(
      json({
        authorize_url: "https://slack.com/oauth?x=1",
        state: "st-1",
        code_verifier: "ver-1",
      }),
    );
    const ch = await startAuthorize("slack", TOKEN, fetchImpl);
    expect(ch).toEqual({
      provider: "slack",
      authorizeUrl: "https://slack.com/oauth?x=1",
      state: "st-1",
      codeVerifier: "ver-1",
    });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/credentials\/slack\/authorize$/);
    expect(init.method).toBe("POST");
  });

  it("rejette un challenge incomplet", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(json({ authorize_url: "x", state: "y" }));
    await expect(startAuthorize("slack", TOKEN, fetchImpl)).rejects.toMatchObject(
      { message: /mal formée/i },
    );
  });
});

describe("completeAuthorize", () => {
  it("POST callback avec code_verifier renommé", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(json({ provider_id: "slack", has_refresh_token: true }));
    const cred = await completeAuthorize(
      "slack",
      { code: "c", state: "s", codeVerifier: "v" },
      TOKEN,
      fetchImpl,
    );
    expect(cred.providerId).toBe("slack");
    expect(cred.hasRefreshToken).toBe(true);
    expect(JSON.parse(fetchImpl.mock.calls[0]![1].body)).toEqual({
      code: "c",
      state: "s",
      code_verifier: "v",
    });
  });

  it("traduit un 502 (échec upstream)", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(json({}, 502));
    await expect(
      completeAuthorize("slack", { code: "c", state: "s", codeVerifier: "v" }, TOKEN, fetchImpl),
    ).rejects.toMatchObject({ status: 502, message: /provider/i });
  });
});

describe("revokeCredential", () => {
  it("DELETE le provider", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await revokeCredential("slack", TOKEN, fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/credentials\/slack$/);
    expect(init.method).toBe("DELETE");
  });

  it("est idempotent sur 404", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response("", { status: 404 }));
    await expect(revokeCredential("slack", TOKEN, fetchImpl)).resolves.toBeUndefined();
  });

  it("lève sur 500", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response("", { status: 500 }));
    await expect(revokeCredential("slack", TOKEN, fetchImpl)).rejects.toBeInstanceOf(
      ApiError,
    );
  });
});

describe("persistance du challenge", () => {
  const ch: AuthorizeChallenge = {
    provider: "slack",
    authorizeUrl: "https://x",
    state: "st",
    codeVerifier: "v",
  };

  it("save/load/clear via storage", () => {
    const store = window.sessionStorage;
    saveChallenge(ch, store);
    expect(loadChallenge(store)).toEqual(ch);
    clearChallenge(store);
    expect(loadChallenge(store)).toBeNull();
  });

  it("loadChallenge renvoie null sur données corrompues", () => {
    const store = window.sessionStorage;
    store.setItem("autome.oauth.challenge", "{bad");
    expect(loadChallenge(store)).toBeNull();
    store.clear();
  });
});

describe("parseCallbackParams", () => {
  it("extrait code et state (avec ou sans ?)", () => {
    expect(parseCallbackParams("?code=abc&state=xyz")).toEqual({
      code: "abc",
      state: "xyz",
    });
    expect(parseCallbackParams("code=abc&state=xyz")).toEqual({
      code: "abc",
      state: "xyz",
    });
  });

  it("renvoie null si code ou state manque", () => {
    expect(parseCallbackParams("?code=abc")).toBeNull();
    expect(parseCallbackParams("")).toBeNull();
  });
});
