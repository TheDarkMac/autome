import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import {
  changePassword,
  decodeJwt,
  deleteAccount,
  isTokenExpired,
  login,
  logout,
  logoutAll,
  readResetToken,
  refresh,
  requestPasswordReset,
  resetPassword,
  signup,
  tokenExpiresAtMs,
  validateCredentials,
  validatePasswordReset,
  whoami,
  type TokenPair,
} from "./auth";

// ─────────────── helpers de test ───────────────

function b64url(obj: unknown): string {
  const json = JSON.stringify(obj);
  const b64 = btoa(
    encodeURIComponent(json).replace(/%([0-9A-F]{2})/g, (_, h) =>
      String.fromCharCode(parseInt(h, 16)),
    ),
  );
  return b64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Construit un JWT signé bidon (header.payload.signature). */
function mkJwt(claims: Record<string, unknown>): string {
  return `${b64url({ alg: "HS256", typ: "JWT" })}.${b64url(claims)}.sig`;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const TOKENS = { access_token: "a.b.c", refresh_token: "d.e.f" };

describe("validateCredentials", () => {
  it("accepte un email + mot de passe valides", () => {
    expect(
      validateCredentials({ email: "a@b.io", password: "longenough" }),
    ).toEqual([]);
  });

  it("rejette un email malformé", () => {
    const errs = validateCredentials({ email: "nope", password: "longenough" });
    expect(errs).toHaveLength(1);
    expect(errs[0]).toMatch(/email/i);
  });

  it("rejette un mot de passe trop court", () => {
    const errs = validateCredentials({ email: "a@b.io", password: "short" });
    expect(errs[0]).toMatch(/8 caractères/);
  });

  it("trim l'email avant validation", () => {
    expect(
      validateCredentials({ email: "  a@b.io  ", password: "longenough" }),
    ).toEqual([]);
  });
});

describe("decodeJwt / tokenExpiresAtMs", () => {
  it("décode sub et exp d'un JWT bien formé", () => {
    const tok = mkJwt({ sub: "user-1", exp: 1000, iat: 100, jti: "x", kind: "access" });
    expect(decodeJwt(tok)).toEqual({
      sub: "user-1",
      exp: 1000,
      iat: 100,
      jti: "x",
      kind: "access",
    });
  });

  it("décode un sub non-ASCII", () => {
    const tok = mkJwt({ sub: "andré@exämple", exp: 10 });
    expect(decodeJwt(tok)?.sub).toBe("andré@exämple");
  });

  it("renvoie null si le format n'est pas x.y.z", () => {
    expect(decodeJwt("pas-un-jwt")).toBeNull();
  });

  it("renvoie null si le payload n'est pas un JSON valide", () => {
    expect(decodeJwt("a.@@@.c")).toBeNull();
  });

  it("renvoie null si sub ou exp manque", () => {
    expect(decodeJwt(mkJwt({ sub: "x" }))).toBeNull();
    expect(decodeJwt(mkJwt({ exp: 1 }))).toBeNull();
  });

  it("tokenExpiresAtMs convertit exp (s) en ms", () => {
    expect(tokenExpiresAtMs(mkJwt({ sub: "u", exp: 1500 }))).toBe(1_500_000);
    expect(tokenExpiresAtMs("garbage")).toBeNull();
  });
});

describe("isTokenExpired", () => {
  const tok = mkJwt({ sub: "u", exp: 1000 }); // expire à 1_000_000 ms

  it("false avant l'expiration", () => {
    expect(isTokenExpired(tok, 999_000)).toBe(false);
  });

  it("true à/après l'expiration", () => {
    expect(isTokenExpired(tok, 1_000_000)).toBe(true);
    expect(isTokenExpired(tok, 1_200_000)).toBe(true);
  });

  it("applique le skew pour expirer de manière proactive", () => {
    expect(isTokenExpired(tok, 995_000, 10_000)).toBe(true);
    expect(isTokenExpired(tok, 980_000, 10_000)).toBe(false);
  });

  it("considère un jeton indécodable comme expiré", () => {
    expect(isTokenExpired("garbage", 0)).toBe(true);
  });
});

describe("login / signup", () => {
  it("POST /auth/login avec email trimmé et renvoie la paire", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(TOKENS));
    const pair: TokenPair = await login(
      { email: "  a@b.io ", password: "longenough" },
      fetchImpl,
    );
    expect(pair).toEqual({ accessToken: "a.b.c", refreshToken: "d.e.f" });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/login$/);
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual({
      email: "a@b.io",
      password: "longenough",
    });
  });

  it("ne tape pas le réseau si la saisie est invalide", async () => {
    const fetchImpl = vi.fn();
    await expect(
      login({ email: "bad", password: "x" }, fetchImpl),
    ).rejects.toBeInstanceOf(ApiError);
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("traduit un 401 login en message clair", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 401));
    await expect(
      login({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ status: 401, message: /incorrect/i });
  });

  it("traduit un 403 login en compte suspendu (#208)", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 403));
    await expect(
      login({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ status: 403, message: /suspendu/i });
  });

  it("signup POST /auth/signup et traduit un 409 en conflit", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 409));
    await expect(
      signup({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ status: 409, message: /existe déjà/i });
    expect(fetchImpl.mock.calls[0]![0]).toMatch(/\/auth\/signup$/);
  });

  it("traduit un 429 en délai d'attente lisible", async () => {
    const limited = new Response("too many attempts", {
      status: 429,
      headers: { "Retry-After": "61" },
    });
    await expect(
      login({ email: "a@b.io", password: "longenough" }, vi.fn().mockResolvedValue(limited)),
    ).rejects.toMatchObject({ status: 429, message: /réessayez dans 2 min/ });
    const noHeader = new Response("", { status: 429 });
    await expect(
      signup({ email: "a@b.io", password: "longenough" }, vi.fn().mockResolvedValue(noHeader)),
    ).rejects.toMatchObject({ status: 429, message: /réessayez plus tard/ });
  });

  it("rejette une paire mal formée", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(jsonResponse({ access_token: "only" }));
    await expect(
      login({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ message: /mal formée/i });
  });

  it("emballe une erreur transport en ApiError", async () => {
    const fetchImpl = vi.fn().mockRejectedValue(new Error("offline"));
    await expect(
      login({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ message: /injoignable/i });
  });
});

describe("refresh", () => {
  it("POST /auth/refresh avec le refresh token", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(TOKENS));
    const pair = await refresh("d.e.f", fetchImpl);
    expect(pair.accessToken).toBe("a.b.c");
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/refresh$/);
    expect(JSON.parse(init.body)).toEqual({ refresh_token: "d.e.f" });
  });

  it("traduit un 401 en refresh invalide", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 401));
    await expect(refresh("x", fetchImpl)).rejects.toMatchObject({
      status: 401,
      message: /invalide ou expiré/i,
    });
  });
});

describe("logout", () => {
  const PAIR: TokenPair = { accessToken: "a.b.c", refreshToken: "d.e.f" };

  it("POST /auth/logout avec le refresh en body et l'access en Bearer", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await logout(PAIR, fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/logout$/);
    expect(init.method).toBe("POST");
    expect(init.headers.Authorization).toBe("Bearer a.b.c");
    expect(JSON.parse(init.body)).toEqual({ refresh_token: "d.e.f" });
  });

  it("un 401 (session déjà expirée) n'est pas une erreur", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 401));
    await expect(logout(PAIR, fetchImpl)).resolves.toBeUndefined();
  });

  it("remonte une erreur serveur", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 500));
    await expect(logout(PAIR, fetchImpl)).rejects.toMatchObject({ status: 500 });
  });
});

describe("logoutAll", () => {
  it("POST /auth/logout-all via le fetch authentifié", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await logoutAll(fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/logout-all$/);
    expect(init.method).toBe("POST");
  });

  it("traduit un 401 en session expirée", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({}, 401));
    await expect(logoutAll(fetchImpl)).rejects.toMatchObject({
      status: 401,
      message: /session expirée/,
    });
  });

  it("remonte une erreur serveur ou transport", async () => {
    await expect(
      logoutAll(vi.fn().mockResolvedValue(jsonResponse({}, 500))),
    ).rejects.toMatchObject({ status: 500 });
    await expect(
      logoutAll(vi.fn().mockRejectedValue(new Error("offline"))),
    ).rejects.toThrow(/injoignable/);
  });
});

describe("changePassword", () => {
  it("POST /auth/password et renvoie la nouvelle paire", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(TOKENS));
    const pair = await changePassword("old-password", "new-password", fetchImpl);
    expect(pair).toEqual({ accessToken: "a.b.c", refreshToken: "d.e.f" });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/password$/);
    expect(JSON.parse(init.body)).toEqual({
      current_password: "old-password",
      new_password: "new-password",
    });
  });

  it("refuse un nouveau mot de passe trop court sans appel réseau", async () => {
    const fetchImpl = vi.fn();
    await expect(changePassword("old-password", "short", fetchImpl)).rejects.toMatchObject({
      status: 400,
    });
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("traduit 403, 409 et 429", async () => {
    const call = (res: Response) =>
      changePassword("old-password", "new-password", vi.fn().mockResolvedValue(res));
    await expect(call(jsonResponse({}, 403))).rejects.toMatchObject({
      status: 403,
      message: /actuel incorrect/,
    });
    await expect(call(jsonResponse({}, 409))).rejects.toMatchObject({ status: 409 });
    await expect(
      call(new Response("", { status: 429, headers: { "Retry-After": "120" } })),
    ).rejects.toMatchObject({ status: 429, message: /2 min/ });
  });
});

describe("deleteAccount", () => {
  it("DELETE /auth/me avec le mot de passe actuel ; 204 suffit", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await deleteAccount("the-password", fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/me$/);
    expect(init.method).toBe("DELETE");
    expect(JSON.parse(init.body)).toEqual({ current_password: "the-password" });
  });

  it("traduit 401, 403, 409, 429 et 503", async () => {
    const call = (res: Response) => deleteAccount("pw", vi.fn().mockResolvedValue(res));
    await expect(call(jsonResponse({}, 401))).rejects.toMatchObject({ status: 401 });
    await expect(call(jsonResponse({}, 403))).rejects.toMatchObject({
      status: 403,
      message: /actuel incorrect/,
    });
    await expect(call(jsonResponse({}, 409))).rejects.toMatchObject({
      status: 409,
      message: /administrateur/,
    });
    await expect(
      call(new Response("", { status: 429, headers: { "Retry-After": "60" } })),
    ).rejects.toMatchObject({ status: 429, message: /1 min/ });
    await expect(call(jsonResponse({}, 503))).rejects.toMatchObject({
      status: 503,
      message: /pas disponible/,
    });
  });

  it("signale un backend injoignable", async () => {
    await expect(
      deleteAccount("pw", vi.fn().mockRejectedValue(new Error("offline"))),
    ).rejects.toThrow(/injoignable/);
  });
});

describe("requestPasswordReset", () => {
  it("POST /auth/password/forgot avec l'email nettoyé ; 202 suffit", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 202 }));
    await requestPasswordReset("  ana@example.io ", fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/password\/forgot$/);
    expect(JSON.parse(init.body)).toEqual({ email: "ana@example.io" });
  });

  it("refuse un email invalide sans appel réseau", async () => {
    const fetchImpl = vi.fn();
    await expect(requestPasswordReset("pas-un-email", fetchImpl)).rejects.toMatchObject({
      status: 400,
    });
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("traduit 429 et 503", async () => {
    const call = (res: Response) =>
      requestPasswordReset("ana@example.io", vi.fn().mockResolvedValue(res));
    await expect(
      call(new Response("", { status: 429, headers: { "Retry-After": "900" } })),
    ).rejects.toMatchObject({ status: 429, message: /15 min/ });
    await expect(call(new Response("", { status: 503 }))).rejects.toMatchObject({
      status: 503,
      message: /pas disponible/,
    });
  });
});

describe("resetPassword", () => {
  it("POST /auth/password/reset avec le jeton ; 204 attendu", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await resetPassword("uid.123.mac", "new-password", fetchImpl);
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/password\/reset$/);
    expect(JSON.parse(init.body)).toEqual({
      token: "uid.123.mac",
      new_password: "new-password",
    });
  });

  it("refuse un mot de passe trop court sans appel réseau", async () => {
    const fetchImpl = vi.fn();
    await expect(resetPassword("t", "short", fetchImpl)).rejects.toMatchObject({
      status: 400,
    });
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("explique un lien refusé et relaie les autres 400", async () => {
    const call = (body: string) =>
      resetPassword(
        "t",
        "new-password",
        vi.fn().mockResolvedValue(new Response(body, { status: 400 })),
      );
    await expect(call("invalid or expired reset link")).rejects.toMatchObject({
      status: 400,
      message: /invalide, expiré ou déjà utilisé/,
    });
    await expect(call("mot de passe trop long")).rejects.toMatchObject({
      message: "mot de passe trop long",
    });
  });
});

describe("readResetToken / validatePasswordReset", () => {
  it("extrait le jeton du fragment #reset=", () => {
    expect(readResetToken("#reset=uid.1.mac")).toBe("uid.1.mac");
    expect(readResetToken("#reset=")).toBeNull();
    expect(readResetToken("#autre=x")).toBeNull();
    expect(readResetToken("")).toBeNull();
  });

  it("valide longueur et confirmation", () => {
    expect(validatePasswordReset("short", "short")).toMatch(/au moins/);
    expect(validatePasswordReset("new-password", "other-password")).toMatch(/confirmation/);
    expect(validatePasswordReset("new-password", "new-password")).toBeNull();
  });
});

describe("whoami", () => {
  it("envoie le Bearer et extrait le sub de « hello <sub> »", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(new Response("hello user-7", { status: 200 }));
    const sub = await whoami("a.b.c", fetchImpl);
    expect(sub).toBe("user-7");
    expect(fetchImpl.mock.calls[0]![1].headers.Authorization).toBe("Bearer a.b.c");
  });

  it("tolère un identifiant brut sans préfixe hello", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(new Response("user-9", { status: 200 }));
    expect(await whoami("a.b.c", fetchImpl)).toBe("user-9");
  });

  it("lève sur 401 (session expirée)", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(new Response("", { status: 401 }));
    await expect(whoami("a.b.c", fetchImpl)).rejects.toMatchObject({
      status: 401,
    });
  });
});
