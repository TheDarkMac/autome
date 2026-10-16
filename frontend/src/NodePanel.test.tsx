import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { NodePanel } from "./NodePanel";
import { buildCanvasNode } from "./graph";
import type { Credential } from "./credentials";

const noop = (): void => undefined;
const emptyCreds = async (): Promise<readonly Credential[]> => [];

describe("<NodePanel /> — vide / inconnu", () => {
  it("affiche un message quand aucun node n'est sélectionné", () => {
    render(<NodePanel node={null} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const panel = screen.getByTestId("node-panel");
    expect(panel).toHaveAttribute("data-empty", "true");
    expect(panel).toHaveTextContent(/Sélectionnez un node/);
  });

  it("affiche un message si aucun schéma n'est trouvé", () => {
    const node = buildCanvasNode({ id: "x-1", kind: "action", subtype: "no.such.thing", label: "X" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    expect(screen.getByTestId("node-panel")).toHaveAttribute("data-unknown", "true");
  });
});

describe("<NodePanel /> — F8.4.a : formulaire généré", () => {
  it("rend un champ par paramètre du schéma webhook", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    expect(screen.getByTestId("field-path")).toBeInTheDocument();
    expect(screen.getByTestId("field-method")).toBeInTheDocument();
  });

  it("rend un textarea pour les champs de type text", () => {
    const node = buildCanvasNode({ id: "a-1", kind: "action", subtype: "http_request" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    expect(screen.getByTestId("field-body").querySelector("textarea")).not.toBeNull();
  });

  it("rend un select pour les enums avec les options", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const opts = Array.from(
      screen.getByTestId("field-method").querySelectorAll("option"),
    ).map((o) => o.value);
    expect(opts).toEqual(["", "GET", "POST", "PUT", "DELETE"]);
  });
});

describe("<NodePanel /> — F8.4.b : validation & defaults", () => {
  it("pré-remplit les defaults du schéma (method=POST pour webhook)", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const sel = screen.getByTestId("field-method").querySelector("select") as HTMLSelectElement;
    expect(sel.value).toBe("POST");
  });

  it("affiche l'erreur « champ requis » sur les required vides", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "polling" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    expect(screen.getByTestId("error-url")).toHaveTextContent(/Champ requis/);
    expect(screen.getByTestId("panel-status")).toHaveTextContent(/1 erreur/);
  });

  it("passe en statut « valide » quand tous les required sont remplis", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    const onChange = vi.fn();
    render(<NodePanel node={node} onChange={onChange} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const pathInput = screen
      .getByTestId("field-path")
      .querySelector("input") as HTMLInputElement;
    fireEvent.change(pathInput, { target: { value: "/hooks/x" } });
    expect(screen.getByTestId("panel-status")).toHaveTextContent(/valide/);
    expect(screen.queryByTestId("error-path")).toBeNull();
    expect(onChange).toHaveBeenLastCalledWith("t-1", { method: "POST", path: "/hooks/x" });
  });

  it("affiche une erreur de pattern et la fait disparaître quand corrigée", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const pathInput = screen
      .getByTestId("field-path")
      .querySelector("input") as HTMLInputElement;
    fireEvent.change(pathInput, { target: { value: "bad" } });
    expect(screen.getByTestId("error-path")).toHaveTextContent(/Format invalide/);
    fireEvent.change(pathInput, { target: { value: "/ok" } });
    expect(screen.queryByTestId("error-path")).toBeNull();
  });

  it("respecte la borne min sur un number (polling.interval_seconds)", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "polling" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const input = screen
      .getByTestId("field-interval_seconds")
      .querySelector("input") as HTMLInputElement;
    expect(input.value).toBe("60"); // default
    fireEvent.change(input, { target: { value: "2" } });
    expect(screen.getByTestId("error-interval_seconds")).toHaveTextContent(/≥ 5/);
  });
});

describe("<NodePanel /> — F8.4.c : expressions", () => {
  it("détecte les références {{ $node('id').path }} dans un champ string", () => {
    const node = buildCanvasNode({ id: "if-1", kind: "if" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const input = screen
      .getByTestId("field-condition")
      .querySelector("input") as HTMLInputElement;
    fireEvent.change(input, {
      target: { value: "{{ $node('trigger-1').output.ok }}" },
    });
    const chips = screen.getByTestId("refs").querySelectorAll("li");
    expect(chips).toHaveLength(1);
    expect(chips[0]?.textContent).toBe("trigger-1.output.ok");
  });

  it("n'affiche pas de chip quand il n'y a pas de référence", () => {
    const node = buildCanvasNode({ id: "if-1", kind: "if" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    const input = screen
      .getByTestId("field-condition")
      .querySelector("input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "true == true" } });
    expect(screen.queryByTestId("refs")).toBeNull();
  });
});

describe("<NodePanel /> — F8.4.d : credentials", () => {
  it("affiche la dropdown peuplée par fetchCredentials", async () => {
    const node = buildCanvasNode({ id: "s-1", kind: "action", subtype: "slack.send" });
    const fetchImpl = vi.fn().mockResolvedValue([
      { id: "c-acme", name: "Workspace Acme", provider: "slack" },
      { id: "c-perso", name: "Perso", provider: "slack" },
    ]);
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={fetchImpl as never} />);
    expect(fetchImpl).toHaveBeenCalledWith("slack");
    await waitFor(() => {
      expect(screen.getByTestId("credential-field").querySelector("select")).not.toBeNull();
    });
    const opts = Array.from(
      screen.getByTestId("credential-field").querySelectorAll("option"),
    ).map((o) => o.textContent);
    expect(opts).toEqual(["—", "Workspace Acme", "Perso"]);
  });

  it("affiche un message « aucun credential » quand la liste est vide", async () => {
    const node = buildCanvasNode({ id: "s-1", kind: "action", subtype: "slack.send" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    await waitFor(() => {
      expect(screen.getByTestId("credential-field")).toHaveTextContent(/Aucun credential/);
    });
  });

  it("ne rend pas de champ credential si le schéma n'en déclare pas (http_request)", () => {
    const node = buildCanvasNode({ id: "h-1", kind: "action", subtype: "http_request" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} />);
    expect(screen.queryByTestId("credential-field")).toBeNull();
  });

  it("propage credential_id dans onChange quand l'utilisateur en sélectionne un", async () => {
    const node = buildCanvasNode({ id: "s-1", kind: "action", subtype: "slack.send" });
    const fetchImpl = vi.fn().mockResolvedValue([
      { id: "c-acme", name: "Workspace Acme", provider: "slack" },
    ]);
    const onChange = vi.fn();
    render(<NodePanel node={node} onChange={onChange} onClose={noop} fetchCredentialsImpl={fetchImpl as never} />);
    await waitFor(() => {
      expect(screen.getByTestId("credential-field").querySelector("select")).not.toBeNull();
    });
    const select = screen
      .getByTestId("credential-field")
      .querySelector("select") as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "c-acme" } });
    const lastCall = onChange.mock.calls.at(-1);
    expect(lastCall?.[0]).toBe("s-1");
    expect((lastCall?.[1] as Record<string, unknown>)["credential_id"]).toBe("c-acme");
  });
});

describe("<NodePanel /> — bouton fermer", () => {
  it("appelle onClose au clic sur ×", () => {
    const node = buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });
    const onClose = vi.fn();
    render(<NodePanel node={node} onChange={noop} onClose={onClose} fetchCredentialsImpl={emptyCreds} />);
    fireEvent.click(screen.getByRole("button", { name: /Fermer/ }));
    expect(onClose).toHaveBeenCalledOnce();
  });
});

describe("<NodePanel /> — #202 : URL du webhook", () => {
  const info = {
    node_id: "t-1",
    name: "Webhook",
    method: "POST",
    path: `/hooks/${"a".repeat(64)}`,
    signature: null,
    signing_secret: "s3cr3t",
  };
  const webhookNode = () => buildCanvasNode({ id: "t-1", kind: "trigger", subtype: "webhook" });

  it("affiche l'URL complète du nœud et masque le secret jusqu'au clic", async () => {
    const webhooks = { list: vi.fn().mockResolvedValue({ items: [info] }), rotate: vi.fn() };
    render(<NodePanel node={webhookNode()} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} webhooks={webhooks} />);
    const url = (await screen.findByTestId("webhook-url")) as HTMLInputElement;
    expect(url.value).toMatch(new RegExp(`${info.path}$`));
    expect(screen.queryByTestId("webhook-secret")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Afficher" }));
    expect(screen.getByTestId("webhook-secret")).toHaveTextContent("s3cr3t");
  });

  it("invite à enregistrer un workflow inconnu du serveur (404) ou sans ce nœud", async () => {
    const { ApiError } = await import("./api");
    const missing = { list: vi.fn().mockRejectedValue(new ApiError("absent", 404)), rotate: vi.fn() };
    const { unmount } = render(
      <NodePanel node={webhookNode()} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} webhooks={missing} />,
    );
    expect(await screen.findByText(/Enregistrez le workflow/)).toBeInTheDocument();
    unmount();
    const other = { list: vi.fn().mockResolvedValue({ items: [{ ...info, node_id: "autre" }] }), rotate: vi.fn() };
    render(<NodePanel node={webhookNode()} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} webhooks={other} />);
    expect(await screen.findByText(/Enregistrez le workflow/)).toBeInTheDocument();
  });

  it("régénère l'URL après confirmation", async () => {
    const rotated = { ...info, path: `/hooks/${"b".repeat(64)}` };
    const webhooks = {
      list: vi.fn().mockResolvedValue({ items: [info] }),
      rotate: vi.fn().mockResolvedValue(rotated),
    };
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<NodePanel node={webhookNode()} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} webhooks={webhooks} />);
    await screen.findByTestId("webhook-url");
    fireEvent.click(screen.getByRole("button", { name: /Régénérer/ }));
    await waitFor(() => {
      expect((screen.getByTestId("webhook-url") as HTMLInputElement).value).toMatch(/b{64}$/);
    });
    expect(webhooks.rotate).toHaveBeenCalledWith("t-1");
    confirm.mockRestore();
  });

  it("n'affiche rien pour un autre type de nœud", () => {
    const webhooks = { list: vi.fn(), rotate: vi.fn() };
    const node = buildCanvasNode({ id: "p-1", kind: "trigger", subtype: "polling" });
    render(<NodePanel node={node} onChange={noop} onClose={noop} fetchCredentialsImpl={emptyCreds} webhooks={webhooks} />);
    expect(screen.queryByTestId("webhook-field")).toBeNull();
    expect(webhooks.list).not.toHaveBeenCalled();
  });
});
