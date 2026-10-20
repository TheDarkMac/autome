import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { App } from "./App";
import type { TokenPair } from "./auth";
import { memorySessionStore } from "./session";
import type { UseSessionOptions } from "./useSession";
import { SAMPLE_ACTIONS } from "./testCatalog";

const sampleSpec = {
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
function authedSession(
  sub = "user-1",
  extra: Partial<UseSessionOptions> = {},
): UseSessionOptions {
  const pair: TokenPair = {
    accessToken: `h.${b64url({ sub, exp: 9_999_999_999 })}.s`,
    refreshToken: "r.e.f",
  };
  return {
    store: memorySessionStore(pair),
    logoutImpl: vi.fn().mockResolvedValue(undefined),
    ...extra,
  };
}

describe("<App />", () => {
  let originalFetch: typeof fetch;
  beforeEach(() => {
    originalFetch = globalThis.fetch;
  });
  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  it("affiche la page de login si aucune session", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    render(<App sessionOptions={{ store: memorySessionStore() }} />);
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: "Autome" }),
    ).not.toBeInTheDocument();
    // Laisse le canari openapi (effet monté) se résoudre dans act.
    await act(async () => undefined);
  });

  it("ouvre la réinitialisation depuis le lien #reset= puis revient à la connexion (#204)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    window.history.replaceState(null, "", "/#reset=uid.1.mac");
    const resetPasswordImpl = vi.fn().mockResolvedValue(undefined);
    render(
      <App
        sessionOptions={{ store: memorySessionStore() }}
        resetPasswordImpl={resetPasswordImpl}
      />,
    );
    expect(screen.getByRole("form", { name: "Réinitialisation du mot de passe" })).toBeInTheDocument();
    // Le jeton quitte aussitôt la barre d'adresse.
    await waitFor(() => expect(window.location.hash).toBe(""));

    fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
      target: { value: "new-password" },
    });
    fireEvent.change(screen.getByLabelText("Confirmation"), {
      target: { value: "new-password" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Enregistrer le mot de passe" }));

    expect(await screen.findByRole("form", { name: "Connexion" })).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent(/réinitialisé/);
    expect(resetPasswordImpl).toHaveBeenCalledWith("uid.1.mac", "new-password");
  });

  it("vérifie l'adresse depuis le lien #verify= puis revient à la connexion (#231)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    window.history.replaceState(null, "", "/#verify=uid.1.mac");
    const verifyEmailImpl = vi.fn().mockResolvedValue(undefined);
    render(
      <App sessionOptions={{ store: memorySessionStore() }} verifyEmailImpl={verifyEmailImpl} />,
    );
    expect(await screen.findByRole("form", { name: "Connexion" })).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent(/Adresse email vérifiée/);
    expect(verifyEmailImpl).toHaveBeenCalledWith("uid.1.mac");
    // Le jeton quitte la barre d'adresse.
    expect(window.location.hash).toBe("");
  });

  it("active un compte invité depuis le lien #invite= et ouvre sa session (#241)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    window.history.replaceState(null, "", "/#invite=uid.1.mac");
    const acceptInvitationImpl = vi.fn().mockResolvedValue({
      accessToken: `h.${b64url({ sub: "ivy", exp: 9_999_999_999 })}.s`,
      refreshToken: "r",
    });
    render(<App sessionOptions={{ store: memorySessionStore(), acceptInvitationImpl }} />);
    expect(screen.getByRole("form", { name: "Activation du compte" })).toBeInTheDocument();
    await waitFor(() => expect(window.location.hash).toBe(""));

    fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
      target: { value: "chosen-pw" },
    });
    fireEvent.change(screen.getByLabelText("Confirmation"), {
      target: { value: "other-pw" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Activer mon compte" }));
    expect(screen.getByRole("alert")).toHaveTextContent(/confirmation ne correspond pas/);
    expect(acceptInvitationImpl).not.toHaveBeenCalled();

    fireEvent.change(screen.getByLabelText("Confirmation"), {
      target: { value: "chosen-pw" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Activer mon compte" }));
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument(),
    );
    expect(acceptInvitationImpl).toHaveBeenCalledWith("uid.1.mac", "chosen-pw");
    expect(screen.getByTestId("current-user")).toHaveTextContent("ivy");
  });

  it("affiche l'erreur d'un lien d'invitation refusé et permet de revenir (#241)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    window.history.replaceState(null, "", "/#invite=uid.1.mac");
    const acceptInvitationImpl = vi
      .fn()
      .mockRejectedValue(new Error("ce lien d'invitation est invalide, expiré ou déjà utilisé"));
    render(<App sessionOptions={{ store: memorySessionStore(), acceptInvitationImpl }} />);
    fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
      target: { value: "chosen-pw" },
    });
    fireEvent.change(screen.getByLabelText("Confirmation"), {
      target: { value: "chosen-pw" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Activer mon compte" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/invalide, expiré/);
    fireEvent.click(screen.getByRole("button", { name: "Retour à la connexion" }));
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
  });

  it("réagit à un lien #reset= collé dans l'onglet déjà ouvert (#204)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    window.history.replaceState(null, "", "/");
    render(<App sessionOptions={{ store: memorySessionStore() }} />);
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
    await act(async () => {
      window.location.hash = "#reset=uid.2.mac";
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    expect(
      await screen.findByRole("form", { name: "Réinitialisation du mot de passe" }),
    ).toBeInTheDocument();
    await waitFor(() => expect(window.location.hash).toBe(""));
  });

  it("demande un lien depuis la page de connexion (#204)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    const requestPasswordResetImpl = vi.fn().mockResolvedValue(undefined);
    render(
      <App
        sessionOptions={{ store: memorySessionStore() }}
        requestPasswordResetImpl={requestPasswordResetImpl}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Mot de passe oublié ?" }));
    fireEvent.change(screen.getByLabelText("Email"), { target: { value: "ana@b.io" } });
    fireEvent.click(screen.getByRole("button", { name: "Envoyer le lien" }));
    expect(await screen.findByRole("status")).toHaveTextContent(/lien de réinitialisation/);
    expect(requestPasswordResetImpl).toHaveBeenCalledWith("ana@b.io");
  });

  it("connecte l'utilisateur puis affiche l'éditeur", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    const loginImpl = vi.fn().mockResolvedValue({
      accessToken: `h.${b64url({ sub: "alice", exp: 9_999_999_999 })}.s`,
      refreshToken: "r",
    });
    render(
      <App
        sessionOptions={{ store: memorySessionStore(), loginImpl }}
      />,
    );
    fireEvent.change(screen.getByLabelText("Email"), {
      target: { value: "a@b.io" },
    });
    fireEvent.change(screen.getByLabelText("Mot de passe"), {
      target: { value: "longenough" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument(),
    );
    expect(screen.getByTestId("current-user")).toHaveTextContent("alice");
  });

  it("shows backend ok state with endpoint count", async () => {
    globalThis.fetch = vi.fn(async (input: RequestInfo | URL) =>
      String(input).includes("/connectors/actions")
        ? jsonResponse({ actions: [] })
        : jsonResponse(sampleSpec),
    ) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.getByRole("status")).toHaveTextContent(/Autome API/);
    });
    expect(screen.getByRole("status")).toHaveTextContent(/2 endpoints/);
  });

  it("liste dans la palette les actions servies par GET /connectors/actions (#257)", async () => {
    globalThis.fetch = vi.fn(async (input: RequestInfo | URL) =>
      String(input).includes("/connectors/actions")
        ? jsonResponse({ actions: SAMPLE_ACTIONS })
        : jsonResponse(sampleSpec),
    ) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    expect(
      await screen.findByTestId("palette-item-action.slack.send_message"),
    ).toHaveTextContent("Slack — Envoyer un message");
    expect(screen.queryByTestId("palette-connectors-unavailable")).toBeNull();
  });

  it("shows error state when backend is unreachable", async () => {
    globalThis.fetch = vi
      .fn()
      .mockRejectedValue(new Error("ECONNREFUSED")) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(/erreur/i);
    });
  });

  it("bascule vers l'onglet Exécutions et monte la RunsList du workflow ouvert (F8.5.a, #225)", async () => {
    globalThis.fetch = vi.fn(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
      if (url.endsWith("/workflows?offset=0&limit=100")) {
        return jsonResponse({
          items: [{ id: "wf-1", name: "Démo", nodes: [], edges: [] }],
          total: 1,
          offset: 0,
          limit: 100,
        });
      }
      if (url.endsWith("/workflows/wf-1")) {
        return jsonResponse({ id: "wf-1", name: "Démo", nodes: [], edges: [] });
      }
      if (url.includes("/workflows/wf-1/runs")) {
        return jsonResponse({ items: [], total: 0, offset: 0, limit: 50 });
      }
      return new Response("", { status: 404 });
    }) as typeof fetch;
    const storage = { getItem: () => "wf-1", setItem: () => undefined, removeItem: () => undefined } as unknown as Storage;
    render(<App sessionOptions={authedSession()} editorOptions={{ storage }} />);
    await waitFor(() =>
      expect(screen.getByLabelText("Nom du workflow")).toHaveValue("Démo"),
    );
    fireEvent.click(screen.getByRole("tab", { name: /Exécutions/ }));
    await waitFor(() =>
      expect(screen.getByTestId("runs-list")).toHaveTextContent("wf-1"),
    );
    await waitFor(() =>
      expect(screen.getByText(/Aucun run pour ce workflow/)).toBeInTheDocument(),
    );
  });

  it("bascule vers l'onglet Connecteurs et monte le panneau (F8.6.c)", async () => {
    globalThis.fetch = vi.fn(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
      if (url.endsWith("/credentials/providers")) {
        return jsonResponse({ items: [{ provider_id: "slack", scopes: [] }] });
      }
      if (url.endsWith("/credentials")) return jsonResponse({ items: [] });
      return new Response("", { status: 404 });
    }) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("tab", { name: /Connecteurs/ }));
    await waitFor(() =>
      expect(screen.getByTestId("connectors-panel")).toBeInTheDocument(),
    );
    expect(await screen.findByTestId("connector-slack")).toBeInTheDocument();
  });

  /** Backend dont `GET /credentials` renvoie `credentials()` à chaque appel. */
  function credentialsBackend(credentials: () => unknown): typeof fetch {
    return vi.fn(async (input, init?: RequestInit) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      const method = init?.method ?? "GET";
      if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
      if (url.endsWith("/credentials/providers")) {
        return jsonResponse({
          items: [
            { provider_id: "github", scopes: [] },
            { provider_id: "slack", scopes: [] },
          ],
        });
      }
      if (method === "POST" && url.endsWith("/credentials/slack/callback")) {
        return jsonResponse({ provider_id: "slack", has_refresh_token: true });
      }
      if (url.endsWith("/credentials")) return jsonResponse(credentials());
      return new Response("", { status: 404 });
    }) as typeof fetch;
  }

  const revokedSlack = (): unknown => ({
    items: [
      { provider_id: "slack", has_refresh_token: true, needs_reconnect: true },
      { provider_id: "github", has_refresh_token: true, needs_reconnect: false },
    ],
  });

  it("signale dans l'en-tête un connecteur à reconnecter et mène à l'onglet (#265)", async () => {
    globalThis.fetch = credentialsBackend(revokedSlack);
    render(<App sessionOptions={authedSession()} />);

    const indicator = await screen.findByTestId("reconnect-indicator");
    expect(indicator).toHaveTextContent("Connecteur à reconnecter : slack");
    expect(
      screen.getByRole("tab", { name: "Connecteurs (1 à reconnecter)" }),
    ).toBeInTheDocument();

    fireEvent.click(indicator);
    const row = await screen.findByTestId("connector-slack");
    expect(row).toHaveTextContent("À reconnecter");
  });

  it("aucun indicateur quand tout est connecté ou que la lecture échoue (#265)", async () => {
    globalThis.fetch = credentialsBackend(() => ({
      items: [{ provider_id: "slack", has_refresh_token: true, needs_reconnect: false }],
    }));
    const { unmount } = render(<App sessionOptions={authedSession()} />);
    await waitFor(() => expect(globalThis.fetch).toHaveBeenCalledWith(
      expect.stringMatching(/\/credentials$/),
      expect.anything(),
    ));
    await act(async () => undefined);
    expect(screen.queryByTestId("reconnect-indicator")).toBeNull();
    expect(screen.getByRole("tab", { name: "Connecteurs" })).toBeInTheDocument();
    unmount();

    globalThis.fetch = vi.fn().mockRejectedValue(new Error("ECONNREFUSED")) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    await act(async () => undefined);
    expect(screen.queryByTestId("reconnect-indicator")).toBeNull();
  });

  it("plusieurs connecteurs à reconnecter : compteur et liste (#265)", async () => {
    globalThis.fetch = credentialsBackend(() => ({
      items: [
        { provider_id: "slack", needs_reconnect: true },
        { provider_id: "github", needs_reconnect: true },
      ],
    }));
    render(<App sessionOptions={authedSession()} />);
    expect(await screen.findByTestId("reconnect-indicator")).toHaveTextContent(
      "2 connecteurs à reconnecter : github, slack",
    );
  });

  it("le lien #connecteurs de l'email ouvre l'onglet Connecteurs (#265)", async () => {
    window.history.replaceState(null, "", "/#connecteurs");
    globalThis.fetch = credentialsBackend(revokedSlack);
    render(<App sessionOptions={authedSession()} />);
    expect(await screen.findByTestId("connectors-panel")).toBeInTheDocument();
    expect(window.location.hash).toBe("");

    // Lien recollé dans l'onglet déjà ouvert.
    fireEvent.click(screen.getByRole("tab", { name: "Compte" }));
    expect(screen.queryByTestId("connectors-panel")).toBeNull();
    await act(async () => {
      window.history.replaceState(null, "", "/#connecteurs");
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    expect(screen.getByTestId("connectors-panel")).toBeInTheDocument();
    expect(window.location.hash).toBe("");
  });

  it("au retour du provider OAuth, finalise la reconnexion et retire l'indicateur (#265)", async () => {
    let revoked = true;
    globalThis.fetch = credentialsBackend(() =>
      revoked
        ? revokedSlack()
        : { items: [{ provider_id: "slack", has_refresh_token: true }] },
    );
    window.sessionStorage.setItem(
      "autome.oauth.challenge",
      JSON.stringify({
        provider: "slack",
        authorizeUrl: "https://x",
        state: "st-1",
        codeVerifier: "ver-1",
      }),
    );
    window.history.replaceState(null, "", "/?code=c-1&state=st-1");
    // Le provider a accepté : la marque est effacée côté serveur.
    revoked = false;
    try {
      render(<App sessionOptions={authedSession()} />);
      expect(await screen.findByText(/slack connecté/i)).toBeInTheDocument();
      expect(window.location.search).toBe("");
      await act(async () => undefined);
      expect(screen.queryByTestId("reconnect-indicator")).toBeNull();
    } finally {
      window.sessionStorage.clear();
      window.history.replaceState(null, "", "/");
    }
  });

  it("se déconnecte et revient à la page de login", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Se déconnecter" }));
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
    await act(async () => undefined);
  });

  it("l'onglet Compte change le mot de passe via la session", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    const renewed: TokenPair = {
      accessToken: `h.${b64url({ sub: "user-1", exp: 9_999_999_999 })}.s2`,
      refreshToken: "r.e.f2",
    };
    const changePasswordImpl = vi.fn().mockResolvedValue(renewed);
    render(<App sessionOptions={authedSession("user-1", { changePasswordImpl })} />);

    fireEvent.click(screen.getByRole("tab", { name: "Compte" }));
    fireEvent.change(screen.getByLabelText("Mot de passe actuel"), {
      target: { value: "old-password" },
    });
    fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
      target: { value: "new-password" },
    });
    fireEvent.change(screen.getByLabelText("Confirmer le nouveau mot de passe"), {
      target: { value: "new-password" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Changer le mot de passe" }));
    });

    expect(changePasswordImpl).toHaveBeenCalledWith(
      "old-password",
      "new-password",
      expect.any(Function),
    );
    expect(await screen.findByText(/autres appareils ont été déconnectés/)).toBeInTheDocument();
    expect(screen.getByTestId("current-user")).toHaveTextContent("user-1");
  });

  it("l'onglet Compte supprime le compte puis revient à la connexion (#229)", async () => {
    globalThis.fetch = vi.fn(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.endsWith("/auth/me")) {
        return jsonResponse({
          id: "user-1",
          email: "sam@example.io",
          role: "editor",
          permissions: ["workflows.read"],
        });
      }
      return jsonResponse(sampleSpec);
    }) as typeof fetch;
    window.localStorage.setItem("autome.lastWorkflow.user-1", "wf-1");
    const deleteAccountImpl = vi.fn().mockResolvedValue(undefined);
    render(<App sessionOptions={authedSession("user-1", { deleteAccountImpl })} />);

    fireEvent.click(screen.getByRole("tab", { name: "Compte" }));
    fireEvent.change(screen.getByLabelText("Votre mot de passe"), {
      target: { value: "the-password" },
    });
    const confirmation = await screen.findByLabelText("Saisissez « sam@example.io » pour confirmer");
    fireEvent.change(confirmation, { target: { value: "sam@example.io" } });
    await act(async () => {
      fireEvent.click(
        screen.getByRole("button", { name: "Supprimer définitivement mon compte" }),
      );
    });

    expect(deleteAccountImpl).toHaveBeenCalledWith("the-password", expect.any(Function));
    expect(await screen.findByText(/compte et vos données ont été supprimés/)).toBeInTheDocument();
    expect(screen.queryByTestId("current-user")).toBeNull();
    expect(window.localStorage.getItem("autome.lastWorkflow.user-1")).toBeNull();
  });

  describe("administration des utilisateurs (#208)", () => {
    function backend(permissions: string[]): typeof fetch {
      return vi.fn(async (input) => {
        const url = typeof input === "string" ? input : (input as Request).url;
        if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
        if (url.endsWith("/auth/me")) {
          return jsonResponse({
            id: "user-1",
            email: "root@example.io",
            role: permissions.includes("*") ? "admin" : "editor",
            permissions,
          });
        }
        if (url.includes("/admin/workflows")) {
          return jsonResponse({
            items: [
              {
                owner_id: "user-2",
                owner_email: "alice@example.io",
                id: "wf-draft",
                name: "Factures",
                node_count: 2,
              },
            ],
            total: 1,
            offset: 0,
            limit: 50,
          });
        }
        if (url.includes("/admin/users")) {
          return jsonResponse({
            items: [
              {
                id: "user-1",
                email: "root@example.io",
                role: "admin",
                permissions: ["*"],
                disabled: false,
                created_at_ms: 0,
              },
            ],
            total: 1,
            offset: 0,
            limit: 50,
          });
        }
        return new Response("", { status: 404 });
      }) as typeof fetch;
    }

    it("un administrateur voit l'onglet et la liste des comptes", async () => {
      globalThis.fetch = backend(["*"]);
      render(<App sessionOptions={authedSession("user-1")} />);
      const tab = await screen.findByRole("tab", { name: "Administration" });
      expect(screen.getByTestId("current-user")).toHaveTextContent("root@example.io");
      fireEvent.click(tab);
      expect(
        await screen.findByRole("heading", { name: "Administration des utilisateurs" }),
      ).toBeInTheDocument();
      expect(await screen.findByText("(vous)")).toBeInTheDocument();
      // Super-admin : workflows de tous les comptes (#243).
      expect(await screen.findByText("Factures")).toBeInTheDocument();
    });

    it("`users.admin` sans `*` : comptes, mais pas les workflows des autres", async () => {
      const fetchImpl = backend(["users.admin"]);
      globalThis.fetch = fetchImpl;
      render(<App sessionOptions={authedSession("user-1")} />);
      fireEvent.click(await screen.findByRole("tab", { name: "Administration" }));
      expect(await screen.findByText("(vous)")).toBeInTheDocument();
      expect(
        screen.queryByRole("region", { name: "Workflows de tous les comptes" }),
      ).not.toBeInTheDocument();
      const urls = vi.mocked(fetchImpl).mock.calls.map(([input]) => String(input));
      expect(urls.some((u) => u.includes("/admin/workflows"))).toBe(false);
    });

    it("sans `users.admin`, pas d'onglet", async () => {
      globalThis.fetch = backend(["workflows.read"]);
      render(<App sessionOptions={authedSession("user-1")} />);
      await waitFor(() =>
        expect(screen.getByTestId("current-user")).toHaveTextContent("root@example.io"),
      );
      expect(screen.queryByRole("tab", { name: "Administration" })).not.toBeInTheDocument();
    });
  });

  describe("déconnecter tous les appareils", () => {
    afterEach(() => vi.restoreAllMocks());

    it("après confirmation, ferme toutes les sessions et revient au login", async () => {
      globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
      vi.spyOn(window, "confirm").mockReturnValue(true);
      const logoutAllImpl = vi.fn().mockResolvedValue(undefined);
      render(<App sessionOptions={authedSession("user-1", { logoutAllImpl })} />);

      fireEvent.click(
        screen.getByRole("button", { name: "Déconnecter tous les appareils" }),
      );

      expect(
        await screen.findByRole("form", { name: "Connexion" }),
      ).toBeInTheDocument();
      expect(logoutAllImpl).toHaveBeenCalledOnce();
    });

    it("sans confirmation, ne fait rien", async () => {
      globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
      vi.spyOn(window, "confirm").mockReturnValue(false);
      const logoutAllImpl = vi.fn().mockResolvedValue(undefined);
      render(<App sessionOptions={authedSession("user-1", { logoutAllImpl })} />);

      fireEvent.click(
        screen.getByRole("button", { name: "Déconnecter tous les appareils" }),
      );
      await act(async () => undefined);

      expect(logoutAllImpl).not.toHaveBeenCalled();
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    });

    it("affiche l'échec et garde la session ouverte", async () => {
      globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
      vi.spyOn(window, "confirm").mockReturnValue(true);
      const logoutAllImpl = vi.fn().mockRejectedValue(new Error("backend injoignable"));
      render(<App sessionOptions={authedSession("user-1", { logoutAllImpl })} />);

      fireEvent.click(
        screen.getByRole("button", { name: "Déconnecter tous les appareils" }),
      );

      expect(await screen.findByText(/Échec de la déconnexion des appareils/)).toBeInTheDocument();
      expect(screen.getByTestId("current-user")).toHaveTextContent("user-1");
    });
  });
});
