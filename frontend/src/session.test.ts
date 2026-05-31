import { describe, expect, it } from "vitest";

import type { TokenPair } from "./auth";
import {
  memorySessionStore,
  msUntilRefresh,
  sessionFromTokens,
  webSessionStore,
  SESSION_STORAGE_KEY,
  type Session,
} from "./session";

function b64url(obj: unknown): string {
  return btoa(JSON.stringify(obj))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}
function mkJwt(claims: Record<string, unknown>): string {
  return `h.${b64url(claims)}.s`;
}

const PAIR: TokenPair = {
  accessToken: mkJwt({ sub: "user-1", exp: 1000 }),
  refreshToken: "r.e.f",
};

describe("webSessionStore", () => {
  it("écrit, relit et efface une paire via Storage", () => {
    const store = webSessionStore(localStorage);
    expect(store.read()).toBeNull();
    store.write(PAIR);
    expect(localStorage.getItem(SESSION_STORAGE_KEY)).toContain("accessToken");
    expect(store.read()).toEqual(PAIR);
    store.clear();
    expect(store.read()).toBeNull();
  });

  it("renvoie null sur un JSON corrompu", () => {
    localStorage.setItem(SESSION_STORAGE_KEY, "{not json");
    expect(webSessionStore(localStorage).read()).toBeNull();
    localStorage.clear();
  });

  it("renvoie null si une clé manque", () => {
    localStorage.setItem(
      SESSION_STORAGE_KEY,
      JSON.stringify({ accessToken: "a" }),
    );
    expect(webSessionStore(localStorage).read()).toBeNull();
    localStorage.clear();
  });

  it("tolère l'absence de storage", () => {
    const store = webSessionStore(null);
    expect(store.read()).toBeNull();
    expect(() => store.write(PAIR)).not.toThrow();
  });
});

describe("memorySessionStore", () => {
  it("conserve l'état en mémoire", () => {
    const store = memorySessionStore();
    store.write(PAIR);
    expect(store.read()).toEqual(PAIR);
    store.clear();
    expect(store.read()).toBeNull();
  });
});

describe("sessionFromTokens", () => {
  it("dérive userId et expiration", () => {
    const s = sessionFromTokens(PAIR) as Session;
    expect(s.userId).toBe("user-1");
    expect(s.accessExpiresAtMs).toBe(1_000_000);
    expect(s.refreshToken).toBe("r.e.f");
  });

  it("renvoie null si l'access token est indécodable", () => {
    expect(sessionFromTokens({ accessToken: "x", refreshToken: "y" })).toBeNull();
  });
});

describe("msUntilRefresh", () => {
  const session = sessionFromTokens(PAIR) as Session; // exp = 1_000_000 ms

  it("vise skew avant l'expiration", () => {
    // target = 1_000_000 - 60_000 = 940_000 ; now = 900_000 → 40_000
    expect(msUntilRefresh(session, 900_000, 60_000, 300_000)).toBe(40_000);
  });

  it("ne descend jamais sous zéro", () => {
    expect(msUntilRefresh(session, 999_000, 60_000, 300_000)).toBe(0);
  });

  it("plafonne à maxMs", () => {
    expect(msUntilRefresh(session, 0, 60_000, 300_000)).toBe(300_000);
  });

  it("retombe sur maxMs si l'expiration est inconnue", () => {
    const noExp: Session = { ...session, accessExpiresAtMs: null };
    expect(msUntilRefresh(noExp, 0, 60_000, 300_000)).toBe(300_000);
  });
});
