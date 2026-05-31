import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { App } from "./App";
import type { TokenPair } from "./auth";
import { memorySessionStore } from "./session";
import type { UseSessionOptions } from "./useSession";

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
function authedSession(sub = "user-1"): UseSessionOptions {
  const pair: TokenPair = {
    accessToken: `h.${b64url({ sub, exp: 9_999_999_999 })}.s`,
    refreshToken: "r.e.f",
  };
  return { store: memorySessionStore(pair) };
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
    globalThis.fetch = vi
      .fn()
      .mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    await waitFor(() => {
      expect(screen.getByRole("status")).toHaveTextContent(/Autome API/);
    });
    expect(screen.getByRole("status")).toHaveTextContent(/2 endpoints/);
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

  it("bascule vers l'onglet Exécutions et monte la RunsList (F8.5.a)", async () => {
    globalThis.fetch = vi.fn(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/openapi.json")) return jsonResponse(sampleSpec);
      if (url.includes("/workflows/wf-draft/runs")) {
        return jsonResponse({ items: [], total: 0, offset: 0, limit: 50 });
      }
      return new Response("", { status: 404 });
    }) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("tab", { name: /Exécutions/ }));
    await waitFor(() =>
      expect(screen.getByTestId("runs-list")).toBeInTheDocument(),
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

  it("se déconnecte et revient à la page de login", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(jsonResponse(sampleSpec)) as typeof fetch;
    render(<App sessionOptions={authedSession()} />);
    expect(screen.getByRole("heading", { name: "Autome" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Se déconnecter" }));
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
    await act(async () => undefined);
  });
});
