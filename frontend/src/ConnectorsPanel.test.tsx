import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";

import { ConnectorsPanel } from "./ConnectorsPanel";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

/** Routeur fetch mock par (méthode, suffixe d'URL). */
function router(routes: Array<[RegExp, () => Response]>): typeof fetch {
  return vi.fn(async (url: string | URL | Request, init?: RequestInit) => {
    const u = String(url);
    const method = init?.method ?? "GET";
    for (const [re, make] of routes) {
      if (re.test(`${method} ${u}`)) return make();
    }
    throw new Error(`route non mockée : ${method} ${u}`);
  }) as unknown as typeof fetch;
}

// Fabrique : un `Response` ne peut être lu qu'une fois — on en crée un
// frais à chaque appel pour supporter les multiples GET (reload, etc.).
const providersResponse = (): Response =>
  json({
    items: [
      { provider_id: "slack", scopes: ["chat:write"] },
      { provider_id: "github", scopes: [] },
    ],
  });

describe("<ConnectorsPanel />", () => {
  it("liste les providers avec leur état de connexion", async () => {
    const fetchImpl = router([
      [/GET .*\/credentials\/providers$/, providersResponse],
      [
        /GET .*\/credentials$/,
        () => json({ items: [{ provider_id: "slack", has_refresh_token: true }] }),
      ],
    ]);
    render(
      <ConnectorsPanel accessToken="t" fetchImpl={fetchImpl} callbackSearch="" />,
    );

    await screen.findByTestId("connector-slack");
    expect(screen.getByTestId("connector-slack")).toHaveTextContent("connecté");
    expect(screen.getByTestId("connector-github")).not.toHaveTextContent(
      "connecté",
    );
    // slack connecté → bouton Déconnecter ; github → Connecter
    expect(screen.getByRole("button", { name: "Déconnecter" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connecter" })).toBeInTheDocument();
  });

  it("démarre le flow OAuth : authorize → persiste challenge → redirige", async () => {
    const fetchImpl = router([
      [/GET .*\/credentials\/providers$/, providersResponse],
      [/GET .*\/credentials$/, () => json({ items: [] })],
      [
        /POST .*\/credentials\/github\/authorize$/,
        () =>
          json({
            authorize_url: "https://github.com/login/oauth?x=1",
            state: "st-9",
            code_verifier: "ver-9",
          }),
      ],
    ]);
    const redirect = vi.fn();
    const storage = window.sessionStorage;
    storage.clear();

    render(
      <ConnectorsPanel
        accessToken="t"
        fetchImpl={fetchImpl}
        callbackSearch=""
        storage={storage}
        redirect={redirect}
      />,
    );

    const githubRow = await screen.findByTestId("connector-github");
    fireEvent.click(within(githubRow).getByRole("button", { name: "Connecter" }));

    await waitFor(() =>
      expect(redirect).toHaveBeenCalledWith("https://github.com/login/oauth?x=1"),
    );
    expect(storage.getItem("autome.oauth.challenge")).toContain("ver-9");
  });

  it("traite le callback OAuth au montage et affiche un succès", async () => {
    const storage = window.sessionStorage;
    storage.clear();
    storage.setItem(
      "autome.oauth.challenge",
      JSON.stringify({
        provider: "slack",
        authorizeUrl: "https://x",
        state: "st-cb",
        codeVerifier: "ver-cb",
      }),
    );
    const callback = vi.fn();
    const fetchImpl = router([
      [
        /POST .*\/credentials\/slack\/callback$/,
        () => json({ provider_id: "slack", has_refresh_token: true }),
      ],
      [/GET .*\/credentials\/providers$/, providersResponse],
      [
        /GET .*\/credentials$/,
        () => json({ items: [{ provider_id: "slack", has_refresh_token: true }] }),
      ],
    ]);

    render(
      <ConnectorsPanel
        accessToken="t"
        fetchImpl={fetchImpl}
        callbackSearch="?code=the-code&state=st-cb"
        storage={storage}
        onCallbackHandled={callback}
      />,
    );

    expect(await screen.findByText(/slack connecté/i)).toBeInTheDocument();
    expect(callback).toHaveBeenCalled();
    // Le challenge est consommé.
    expect(storage.getItem("autome.oauth.challenge")).toBeNull();
  });

  it("ignore le callback si le state ne correspond pas", async () => {
    const storage = window.sessionStorage;
    storage.clear();
    storage.setItem(
      "autome.oauth.challenge",
      JSON.stringify({
        provider: "slack",
        authorizeUrl: "https://x",
        state: "st-real",
        codeVerifier: "v",
      }),
    );
    const callbackSpy = vi.fn(() => json({ provider_id: "slack" }));
    const fetchImpl = router([
      [/POST .*\/callback$/, callbackSpy],
      [/GET .*\/credentials\/providers$/, providersResponse],
      [/GET .*\/credentials$/, () => json({ items: [] })],
    ]);

    render(
      <ConnectorsPanel
        accessToken="t"
        fetchImpl={fetchImpl}
        callbackSearch="?code=c&state=st-forged"
        storage={storage}
      />,
    );

    await screen.findByTestId("connector-slack");
    expect(callbackSpy).not.toHaveBeenCalled();
  });

  it("déconnecte un provider et recharge", async () => {
    let connected = true;
    const fetchImpl = router([
      [/GET .*\/credentials\/providers$/, providersResponse],
      [
        /GET .*\/credentials$/,
        () =>
          json({
            items: connected ? [{ provider_id: "slack", has_refresh_token: true }] : [],
          }),
      ],
      [
        /DELETE .*\/credentials\/slack$/,
        () => {
          connected = false;
          return new Response(null, { status: 204 });
        },
      ],
    ]);

    render(
      <ConnectorsPanel accessToken="t" fetchImpl={fetchImpl} callbackSearch="" />,
    );

    fireEvent.click(await screen.findByRole("button", { name: "Déconnecter" }));

    await waitFor(() =>
      expect(screen.getByText(/slack déconnecté/i)).toBeInTheDocument(),
    );
    const slackRow = screen.getByTestId("connector-slack");
    expect(
      within(slackRow).getByRole("button", { name: "Connecter" }),
    ).toBeInTheDocument();
  });

  it("affiche une erreur si le chargement échoue", async () => {
    const fetchImpl = router([
      [/GET .*\/credentials\/providers$/, () => json({}, 500)],
      [/GET .*\/credentials$/, () => json({ items: [] })],
    ]);
    render(
      <ConnectorsPanel accessToken="t" fetchImpl={fetchImpl} callbackSearch="" />,
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(/erreur/i);
  });
});
