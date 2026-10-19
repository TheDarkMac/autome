import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { TokenPair } from "./auth";
import { memorySessionStore } from "./session";
import { useSession } from "./useSession";

function b64url(obj: unknown): string {
  return btoa(JSON.stringify(obj))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}
function mkPair(sub: string, expSeconds: number): TokenPair {
  return {
    accessToken: `h.${b64url({ sub, exp: expSeconds })}.s`,
    refreshToken: `refresh-${sub}`,
  };
}

describe("useSession", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("démarre déconnecté quand le store est vide", () => {
    const { result } = renderHook(() =>
      useSession({ store: memorySessionStore() }),
    );
    expect(result.current.loading).toBe(false);
    expect(result.current.authenticated).toBe(false);
    expect(result.current.session).toBeNull();
  });

  it("hydrate la session depuis le store au montage", () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const { result } = renderHook(() => useSession({ store }));
    expect(result.current.loading).toBe(false);
    expect(result.current.authenticated).toBe(true);
    expect(result.current.session?.userId).toBe("user-1");
  });

  it("login persiste la paire et met à jour la session", async () => {
    const store = memorySessionStore();
    const loginImpl = vi.fn().mockResolvedValue(mkPair("alice", 10_000));
    const { result } = renderHook(() => useSession({ store, loginImpl }));

    await act(async () => {
      await result.current.login({ email: "a@b.io", password: "longenough" });
    });

    expect(result.current.session?.userId).toBe("alice");
    expect(store.read()?.refreshToken).toBe("refresh-alice");
  });

  it("signup persiste la paire", async () => {
    const store = memorySessionStore();
    const signupImpl = vi
      .fn()
      .mockResolvedValue({ kind: "signedIn", tokens: mkPair("bob", 10_000) });
    const { result } = renderHook(() => useSession({ store, signupImpl }));

    let pending: unknown = "unset";
    await act(async () => {
      pending = await result.current.signup({ email: "b@b.io", password: "longenough" });
    });

    expect(pending).toBeNull();
    expect(result.current.session?.userId).toBe("bob");
    expect(signupImpl).toHaveBeenCalledOnce();
  });

  it("signup en attente de vérification n'ouvre aucune session (#231)", async () => {
    const store = memorySessionStore();
    const signupImpl = vi
      .fn()
      .mockResolvedValue({ kind: "verificationRequired", email: "b@b.io" });
    const { result } = renderHook(() => useSession({ store, signupImpl }));

    let pending: unknown = null;
    await act(async () => {
      pending = await result.current.signup({ email: "B@b.io", password: "longenough" });
    });

    expect(pending).toEqual({ email: "b@b.io" });
    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  it("logout efface la session et le store, et révoque côté serveur", () => {
    const pair = mkPair("user-1", 10_000);
    const store = memorySessionStore(pair);
    const logoutImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() => useSession({ store, logoutImpl }));
    expect(result.current.authenticated).toBe(true);

    act(() => result.current.logout());

    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
    expect(logoutImpl).toHaveBeenCalledOnce();
    expect(logoutImpl).toHaveBeenCalledWith(pair);
  });

  it("logout vide la session même si la révocation échoue", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const logoutImpl = vi.fn().mockRejectedValue(new Error("offline"));
    const { result } = renderHook(() => useSession({ store, logoutImpl }));

    await act(async () => {
      result.current.logout();
      await Promise.resolve();
    });

    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  it("logoutAll ferme les sessions côté serveur via authFetch puis vide la session", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const logoutAllImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() => useSession({ store, logoutAllImpl }));

    await act(async () => {
      await result.current.logoutAll();
    });

    expect(logoutAllImpl).toHaveBeenCalledWith(result.current.authFetch);
    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  it("logoutAll garde la session locale si le serveur n'a pas confirmé", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const logoutAllImpl = vi.fn().mockRejectedValue(new Error("offline"));
    const { result } = renderHook(() => useSession({ store, logoutAllImpl }));

    await act(async () => {
      await expect(result.current.logoutAll()).rejects.toThrow("offline");
    });

    expect(result.current.session?.userId).toBe("user-1");
    expect(store.read()).not.toBeNull();
  });

  it("changePassword remplace la session par la paire renvoyée", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const renewed = mkPair("user-1", 20_000);
    const changePasswordImpl = vi.fn().mockResolvedValue(renewed);
    const { result } = renderHook(() => useSession({ store, changePasswordImpl }));

    await act(async () => {
      await result.current.changePassword("old-password", "new-password");
    });

    expect(changePasswordImpl).toHaveBeenCalledWith(
      "old-password",
      "new-password",
      result.current.authFetch,
    );
    expect(store.read()).toEqual(renewed);
    expect(result.current.session?.accessToken).toBe(renewed.accessToken);
  });

  it("changePassword en échec ne touche pas à la session", async () => {
    const pair = mkPair("user-1", 10_000);
    const store = memorySessionStore(pair);
    const changePasswordImpl = vi.fn().mockRejectedValue(new Error("refusé"));
    const { result } = renderHook(() => useSession({ store, changePasswordImpl }));

    await act(async () => {
      await expect(
        result.current.changePassword("old-password", "new-password"),
      ).rejects.toThrow("refusé");
    });
    expect(store.read()).toEqual(pair);
  });

  it("deleteAccount supprime le compte via authFetch puis vide la session", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const deleteAccountImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() => useSession({ store, deleteAccountImpl }));

    await act(async () => {
      await result.current.deleteAccount("the-password");
    });

    expect(deleteAccountImpl).toHaveBeenCalledWith("the-password", result.current.authFetch);
    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  it("deleteAccount refusé garde la session locale", async () => {
    const pair = mkPair("user-1", 10_000);
    const store = memorySessionStore(pair);
    const deleteAccountImpl = vi.fn().mockRejectedValue(new Error("refusé"));
    const { result } = renderHook(() => useSession({ store, deleteAccountImpl }));

    await act(async () => {
      await expect(result.current.deleteAccount("bad")).rejects.toThrow("refusé");
    });
    expect(store.read()).toEqual(pair);
    expect(result.current.session?.userId).toBe("user-1");
  });

  it("acceptInvitation ouvre la session du compte invité (#241)", async () => {
    const store = memorySessionStore();
    const acceptInvitationImpl = vi.fn().mockResolvedValue(mkPair("ivy", 10_000));
    const logoutImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() =>
      useSession({ store, acceptInvitationImpl, logoutImpl }),
    );
    await act(async () => {
      await result.current.acceptInvitation("uid.1.mac", "chosen-pw");
    });
    expect(acceptInvitationImpl).toHaveBeenCalledWith("uid.1.mac", "chosen-pw");
    expect(result.current.session?.userId).toBe("ivy");
    expect(store.read()?.refreshToken).toBe("refresh-ivy");
    expect(logoutImpl).not.toHaveBeenCalled();
  });

  it("acceptInvitation ferme d'abord la session déjà ouverte ; un refus ne change rien", async () => {
    const store = memorySessionStore(mkPair("root", 10_000));
    const acceptInvitationImpl = vi
      .fn()
      .mockRejectedValueOnce(new Error("lien invalide"))
      .mockResolvedValueOnce(mkPair("ivy", 10_000));
    const logoutImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() =>
      useSession({ store, acceptInvitationImpl, logoutImpl }),
    );
    await act(async () => {
      await expect(result.current.acceptInvitation("bad", "chosen-pw")).rejects.toThrow(
        "lien invalide",
      );
    });
    expect(result.current.session?.userId).toBe("root");
    expect(logoutImpl).not.toHaveBeenCalled();
    await act(async () => {
      await result.current.acceptInvitation("uid.1.mac", "chosen-pw");
    });
    expect(logoutImpl).toHaveBeenCalledWith({
      accessToken: mkPair("root", 10_000).accessToken,
      refreshToken: "refresh-root",
    });
    expect(result.current.session?.userId).toBe("ivy");
  });

  it("logout sans session n'appelle pas le serveur", () => {
    const logoutImpl = vi.fn().mockResolvedValue(undefined);
    const { result } = renderHook(() =>
      useSession({ store: memorySessionStore(), logoutImpl }),
    );

    act(() => result.current.logout());

    expect(logoutImpl).not.toHaveBeenCalled();
  });

  it("rafraîchit proactivement l'access token avant l'expiration", async () => {
    const store = memorySessionStore(mkPair("user-1", 120)); // exp = 120_000 ms
    const renewed = mkPair("user-1-renewed", 600);
    const refreshImpl = vi.fn().mockResolvedValue(renewed);
    renderHook(() =>
      useSession({
        store,
        refreshImpl,
        skewMs: 60_000,
        maxRefreshDelayMs: 300_000,
      }),
    );
    // delay = exp(120_000) - skew(60_000) - now(0) = 60_000 ms
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(refreshImpl).toHaveBeenCalledWith("refresh-user-1");
    expect(store.read()?.accessToken).toBe(renewed.accessToken);
  });

  it("déconnecte si le refresh échoue", async () => {
    const store = memorySessionStore(mkPair("user-1", 120));
    const refreshImpl = vi.fn().mockRejectedValue(new Error("revoked"));
    const { result } = renderHook(() =>
      useSession({ store, refreshImpl, skewMs: 60_000 }),
    );
    expect(result.current.authenticated).toBe(true);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });

    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  // ─────────────────── authFetch (F8.7.c) ───────────────────

  it("authFetch injecte le Bearer de la session courante", async () => {
    const pair = mkPair("user-1", 10_000);
    const store = memorySessionStore(pair);
    const fetchSpy = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit) =>
        new Response("ok", { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchSpy);
    const { result } = renderHook(() => useSession({ store }));

    await act(async () => {
      await result.current.authFetch("/x");
    });

    const init = fetchSpy.mock.calls[0]![1] as RequestInit;
    expect(new Headers(init.headers).get("Authorization")).toBe(
      `Bearer ${pair.accessToken}`,
    );
  });

  it("authFetch rafraîchit sur 401, rejoue et met à jour la session", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const renewed = mkPair("user-1-renewed", 10_000);
    const refreshImpl = vi.fn().mockResolvedValue(renewed);
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(new Response("unauth", { status: 401 }))
      .mockResolvedValueOnce(new Response("ok", { status: 200 }));
    vi.stubGlobal("fetch", fetchSpy);
    const { result } = renderHook(() => useSession({ store, refreshImpl }));

    let res!: Response;
    await act(async () => {
      res = await result.current.authFetch("/x");
    });

    expect(res.status).toBe(200);
    expect(refreshImpl).toHaveBeenCalledWith(
      "refresh-user-1",
      expect.any(Function),
    );
    expect(result.current.session?.userId).toBe("user-1-renewed");
    expect(store.read()?.accessToken).toBe(renewed.accessToken);
    const init2 = fetchSpy.mock.calls[1]![1] as RequestInit;
    expect(new Headers(init2.headers).get("Authorization")).toBe(
      `Bearer ${renewed.accessToken}`,
    );
  });

  it("authFetch déconnecte si le refresh échoue sur 401", async () => {
    const store = memorySessionStore(mkPair("user-1", 10_000));
    const refreshImpl = vi.fn().mockRejectedValue(new Error("revoked"));
    const fetchSpy = vi.fn(async () => new Response("unauth", { status: 401 }));
    vi.stubGlobal("fetch", fetchSpy);
    const { result } = renderHook(() => useSession({ store, refreshImpl }));

    let res!: Response;
    await act(async () => {
      res = await result.current.authFetch("/x");
    });

    expect(res.status).toBe(401);
    expect(result.current.session).toBeNull();
    expect(store.read()).toBeNull();
  });

  it("verifyMfa ouvre la session renvoyée ; un refus ne change rien (#245)", async () => {
    const store = memorySessionStore();
    const verifyMfaImpl = vi
      .fn()
      .mockRejectedValueOnce(new Error("code incorrect"))
      .mockResolvedValue(mkPair("carol", 10_000));
    const { result } = renderHook(() => useSession({ store, verifyMfaImpl }));

    await act(async () => {
      await expect(result.current.verifyMfa("u.1.mac", "000000")).rejects.toThrow(
        "code incorrect",
      );
    });
    expect(result.current.session).toBeNull();

    await act(async () => {
      await result.current.verifyMfa("u.1.mac", "123456");
    });
    expect(verifyMfaImpl).toHaveBeenLastCalledWith("u.1.mac", "123456");
    expect(result.current.session?.userId).toBe("carol");
    expect(store.read()?.refreshToken).toBe("refresh-carol");
  });

  it("enableMfa remplace la session et renvoie les codes de secours (#245)", async () => {
    const store = memorySessionStore(mkPair("dan", 10_000));
    const enableMfaImpl = vi.fn().mockResolvedValue({
      recoveryCodes: ["abcde-fghjk"],
      tokens: { ...mkPair("dan", 20_000), refreshToken: "refresh-dan-2" },
    });
    const { result } = renderHook(() => useSession({ store, enableMfaImpl }));

    let codes: string[] = [];
    await act(async () => {
      codes = await result.current.enableMfa("123456");
    });
    expect(codes).toEqual(["abcde-fghjk"]);
    // Appel authentifié : l'`authFetch` de la session est transmis.
    expect(enableMfaImpl).toHaveBeenCalledWith("123456", result.current.authFetch);
    expect(store.read()?.refreshToken).toBe("refresh-dan-2");
  });
});

