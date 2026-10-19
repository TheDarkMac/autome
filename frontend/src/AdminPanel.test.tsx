import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import { AdminPanel, type AdminApi } from "./AdminPanel";
import type { Schemas } from "./apiClient";

type AdminUser = Schemas["AdminUser"];

function user(id: string, email: string, extra: Partial<AdminUser> = {}): AdminUser {
  return {
    id,
    email,
    role: "editor",
    permissions: ["workflows.read"],
    disabled: false,
    email_verified: true,
    invitation_pending: false,
    created_at_ms: Date.UTC(2026, 9, 1),
    ...extra,
  };
}

const ME = user("me", "root@example.io", { role: "admin", permissions: ["*"] });
const ALICE = user("u-alice", "alice@example.io");
const BOSS = user("u-boss", "boss@example.io", { role: "admin", permissions: ["*"] });

function fakeApi(items: AdminUser[] = [ME, ALICE, BOSS], total = items.length): AdminApi & {
  listUsers: ReturnType<typeof vi.fn>;
} {
  return {
    listUsers: vi.fn().mockResolvedValue({ items, total, offset: 0, limit: 50 }),
    setUserRole: vi.fn(async (id: string, role: string) => ({ ...ALICE, id, role })),
    disableUser: vi.fn(async (id: string) => ({ ...ALICE, id, disabled: true })),
    enableUser: vi.fn(async (id: string) => ({ ...ALICE, id, disabled: false })),
    logoutUser: vi.fn().mockResolvedValue(undefined),
    deleteUser: vi.fn().mockResolvedValue(undefined),
    verifyUserEmail: vi.fn(async (id: string) => ({ ...ALICE, id, email_verified: true })),
    inviteUser: vi.fn(async (email: string, role: string) => ({
      user: user("u-new", email, { role, email_verified: false, invitation_pending: true }),
      invitation_sent: true,
    })),
    resendInvitation: vi.fn().mockResolvedValue(undefined),
    listAllWorkflows: vi.fn().mockResolvedValue({ items: [], total: 0, offset: 0, limit: 50 }),
  };
}

function row(email: string): HTMLElement {
  const accounts = screen.getByRole("table", { name: "Comptes" });
  return within(accounts).getByText(email).closest("tr") as HTMLElement;
}

async function renderPanel(
  api: AdminApi,
  props: { superAdmin?: boolean; confirm?: (m: string) => boolean; pageSize?: number } = {},
): Promise<void> {
  render(
    <AdminPanel
      api={api}
      currentUserId="me"
      superAdmin={props.superAdmin ?? true}
      confirm={props.confirm ?? (() => true)}
      {...(props.pageSize !== undefined ? { pageSize: props.pageSize } : {})}
    />,
  );
  await waitFor(() => expect(row("alice@example.io")).toBeInTheDocument());
}

describe("<AdminPanel />", () => {
  it("liste les comptes avec rôle et statut, et grise son propre compte", async () => {
    await renderPanel(fakeApi());
    const mine = row("root@example.io");
    expect(within(mine).getByText("(vous)")).toBeInTheDocument();
    expect(within(mine).getByRole("combobox")).toBeDisabled();
    expect(within(mine).getByRole("button", { name: /Suspendre/ })).toBeDisabled();
    const alice = row("alice@example.io");
    expect(within(alice).getByRole("combobox")).toHaveValue("editor");
    expect(within(alice).getByText("Actif")).toBeInTheDocument();
    expect(within(alice).getByRole("button", { name: /Suspendre/ })).toBeEnabled();
    expect(screen.getByText("1–3 sur 3")).toBeInTheDocument();
  });

  it("change un rôle et met la ligne à jour", async () => {
    const api = fakeApi();
    await renderPanel(api);
    await act(async () => {
      fireEvent.change(within(row("alice@example.io")).getByRole("combobox"), {
        target: { value: "viewer" },
      });
    });
    expect(api.setUserRole).toHaveBeenCalledWith("u-alice", "viewer");
    expect(within(row("alice@example.io")).getByRole("combobox")).toHaveValue("viewer");
    expect(screen.getByRole("status")).toHaveTextContent("rôle Lecteur");
  });

  it("suspend après confirmation, puis réactive", async () => {
    const api = fakeApi();
    const confirm = vi.fn().mockReturnValueOnce(false).mockReturnValue(true);
    await renderPanel(api, { confirm });
    const suspend = (): HTMLElement =>
      within(row("alice@example.io")).getByRole("button", { name: /Suspendre/ });

    await act(async () => {
      fireEvent.click(suspend());
    });
    expect(api.disableUser).not.toHaveBeenCalled();

    await act(async () => {
      fireEvent.click(suspend());
    });
    expect(api.disableUser).toHaveBeenCalledWith("u-alice");
    expect(confirm).toHaveBeenLastCalledWith(expect.stringMatching(/sessions seront fermées/));
    expect(within(row("alice@example.io")).getByText("Suspendu")).toBeInTheDocument();

    await act(async () => {
      fireEvent.click(within(row("alice@example.io")).getByRole("button", { name: /Réactiver/ }));
    });
    expect(api.enableUser).toHaveBeenCalledWith("u-alice");
    expect(within(row("alice@example.io")).getByText("Actif")).toBeInTheDocument();
  });

  it("signale une adresse non vérifiée et la vérifie après confirmation (#231)", async () => {
    const pending = user("u-pending", "pending@example.io", { email_verified: false });
    const api = fakeApi([ME, ALICE, pending]);
    api.verifyUserEmail = vi.fn(async (id: string) => ({ ...pending, id, email_verified: true }));
    const confirm = vi.fn().mockReturnValueOnce(false).mockReturnValue(true);
    await renderPanel(api, { confirm });
    expect(within(row("alice@example.io")).queryByText("Email non vérifié")).toBeNull();
    expect(within(row("alice@example.io")).queryByRole("button", { name: /Vérifier/ })).toBeNull();
    const line = (): HTMLElement => row("pending@example.io");
    expect(within(line()).getByText("Email non vérifié")).toBeInTheDocument();

    const verify = (): HTMLElement =>
      within(line()).getByRole("button", { name: "Vérifier l'adresse de pending@example.io" });
    await act(async () => {
      fireEvent.click(verify());
    });
    expect(api.verifyUserEmail).not.toHaveBeenCalled();
    await act(async () => {
      fireEvent.click(verify());
    });
    expect(confirm).toHaveBeenLastCalledWith(expect.stringMatching(/lui appartient/));
    expect(api.verifyUserEmail).toHaveBeenCalledWith("u-pending");
    expect(within(line()).queryByText("Email non vérifié")).toBeNull();
    expect(screen.getByRole("status")).toHaveTextContent("Adresse pending@example.io vérifiée.");
  });

  it("ferme les sessions d'un compte après confirmation", async () => {
    const api = fakeApi();
    await renderPanel(api);
    await act(async () => {
      fireEvent.click(
        within(row("alice@example.io")).getByRole("button", { name: /Fermer les sessions/ }),
      );
    });
    expect(api.logoutUser).toHaveBeenCalledWith("u-alice");
    expect(screen.getByRole("status")).toHaveTextContent("Sessions de alice@example.io fermées");
  });

  it("sans `*`, le rôle admin et les super-admins sont verrouillés", async () => {
    await renderPanel(fakeApi(), { superAdmin: false });
    const boss = row("boss@example.io");
    expect(within(boss).getByRole("combobox")).toBeDisabled();
    expect(within(boss).getByRole("button", { name: /Suspendre/ })).toBeDisabled();
    expect(within(boss).getByRole("button", { name: /Fermer les sessions/ })).toBeDisabled();
    const adminOption = within(row("alice@example.io")).getByRole("option", {
      name: "Administrateur",
    }) as HTMLOptionElement;
    expect(adminOption.disabled).toBe(true);
  });

  it("traduit un refus du serveur sans modifier la ligne", async () => {
    const api = fakeApi();
    api.setUserRole = vi.fn().mockRejectedValue(new ApiError("PUT a échoué (403)", 403));
    await renderPanel(api);
    await act(async () => {
      fireEvent.change(within(row("alice@example.io")).getByRole("combobox"), {
        target: { value: "viewer" },
      });
    });
    expect(screen.getByRole("alert")).toHaveTextContent(
      "alice@example.io : action réservée aux super-administrateurs",
    );
    expect(within(row("alice@example.io")).getByRole("combobox")).toHaveValue("editor");
  });

  it("recherche par email et pagine", async () => {
    const api = fakeApi([ALICE], 3);
    await renderPanel(api, { pageSize: 1 });
    expect(api.listUsers).toHaveBeenLastCalledWith({ q: "", offset: 0, limit: 1 });

    const pager = screen.getByRole("navigation", { name: "Pagination des comptes" });
    await act(async () => {
      fireEvent.click(within(pager).getByRole("button", { name: "Suivant" }));
    });
    expect(api.listUsers).toHaveBeenLastCalledWith({ q: "", offset: 1, limit: 1 });

    const search = screen.getByLabelText("Rechercher par email");
    fireEvent.change(search, { target: { value: " alice " } });
    await act(async () => {
      fireEvent.click(
        within(search.closest("form") as HTMLElement).getByRole("button", {
          name: "Rechercher",
        }),
      );
    });
    expect(api.listUsers).toHaveBeenLastCalledWith({ q: "alice", offset: 0, limit: 1 });
  });

  it("signale une liste indisponible", async () => {
    const api = fakeApi();
    api.listUsers.mockRejectedValue(new ApiError("GET a échoué (403)", 403));
    render(<AdminPanel api={api} currentUserId="me" superAdmin={false} />);
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent(/Liste des comptes indisponible/),
    );
  });
  it("supprime un compte après confirmation et le retire de la liste", async () => {
    const api = fakeApi();
    const confirm = vi.fn().mockReturnValueOnce(false).mockReturnValue(true);
    await renderPanel(api, { confirm });
    const button = within(row("alice@example.io")).getByRole("button", {
      name: "Supprimer alice@example.io",
    });

    fireEvent.click(button);
    expect(api.deleteUser).not.toHaveBeenCalled();

    await act(async () => {
      fireEvent.click(button);
    });
    expect(confirm).toHaveBeenLastCalledWith(expect.stringMatching(/irréversible/));
    expect(api.deleteUser).toHaveBeenCalledWith("u-alice");
    expect(screen.queryByText("alice@example.io")).toBeNull();
    expect(screen.getByRole("status")).toHaveTextContent(/supprimé/);
    expect(screen.getByText("1–2 sur 2")).toBeInTheDocument();
    // Son propre compte et, sans `*`, les super-admins ne sont pas supprimables.
    expect(
      within(row("root@example.io")).getByRole("button", { name: /Supprimer/ }),
    ).toBeDisabled();
  });

  it("invite une personne avec un rôle puis recharge la liste (#241)", async () => {
    const api = fakeApi();
    await renderPanel(api, { superAdmin: false });
    const form = screen.getByRole("form", { name: "Inviter une personne" });
    const admin = within(form).getByRole("option", { name: "Administrateur" }) as HTMLOptionElement;
    expect(admin.disabled).toBe(true);
    expect(within(form).getByRole("combobox")).toHaveValue("editor");

    fireEvent.change(within(form).getByLabelText("Inviter par email"), {
      target: { value: "ivy@example.io" },
    });
    fireEvent.change(within(form).getByRole("combobox"), { target: { value: "viewer" } });
    await act(async () => {
      fireEvent.click(within(form).getByRole("button", { name: "Inviter" }));
    });
    expect(api.inviteUser).toHaveBeenCalledWith("ivy@example.io", "viewer");
    expect(screen.getByRole("status")).toHaveTextContent("Invitation envoyée à ivy@example.io.");
    expect(api.listUsers).toHaveBeenCalledTimes(2);
    expect(within(form).getByLabelText("Inviter par email")).toHaveValue("");
  });

  it("signale un email non parti et traduit les refus d'invitation", async () => {
    const api = fakeApi();
    api.inviteUser = vi
      .fn()
      .mockResolvedValueOnce({
        user: user("u-new", "ivy@example.io", { invitation_pending: true }),
        invitation_sent: false,
      })
      .mockRejectedValueOnce(new ApiError("POST a échoué (409)", 409))
      .mockRejectedValueOnce(new ApiError("POST a échoué (503)", 503));
    await renderPanel(api);
    const form = screen.getByRole("form", { name: "Inviter une personne" });
    const invite = async (email: string): Promise<void> => {
      fireEvent.change(within(form).getByLabelText("Inviter par email"), {
        target: { value: email },
      });
      await act(async () => {
        fireEvent.click(within(form).getByRole("button", { name: "Inviter" }));
      });
    };
    await invite("ivy@example.io");
    expect(screen.getByRole("alert")).toHaveTextContent(/l'email n'est pas parti : renvoyez/);
    await invite("alice@example.io");
    expect(screen.getByRole("alert")).toHaveTextContent("un compte existe déjà pour cet email");
    expect(within(form).getByLabelText("Inviter par email")).toHaveValue("alice@example.io");
    await invite("joe@example.io");
    expect(screen.getByRole("alert")).toHaveTextContent(/aucun envoi d'email/);
  });

  it("montre une invitation en attente, la renvoie ou l'annule", async () => {
    const ivy = user("u-ivy", "ivy@example.io", {
      email_verified: false,
      invitation_pending: true,
    });
    const api = fakeApi([ME, ALICE, ivy]);
    const confirm = vi.fn().mockReturnValue(true);
    await renderPanel(api, { confirm });
    const line = row("ivy@example.io");
    expect(within(line).getByText("Invitation en attente")).toBeInTheDocument();
    expect(within(line).queryByText("Actif")).toBeNull();
    expect(within(line).queryByText("Email non vérifié")).toBeNull();
    expect(within(line).queryByRole("button", { name: /Vérifier/ })).toBeNull();
    expect(
      within(row("alice@example.io")).queryByRole("button", { name: /Renvoyer/ }),
    ).toBeNull();

    await act(async () => {
      fireEvent.click(
        within(line).getByRole("button", { name: "Renvoyer l'invitation de ivy@example.io" }),
      );
    });
    expect(api.resendInvitation).toHaveBeenCalledWith("u-ivy");
    expect(screen.getByRole("status")).toHaveTextContent(/lien précédent ne fonctionne plus/);

    api.resendInvitation = vi.fn().mockRejectedValue(new ApiError("POST a échoué (502)", 502));
    await act(async () => {
      fireEvent.click(within(line).getByRole("button", { name: /Renvoyer/ }));
    });
    expect(screen.getByRole("alert")).toHaveTextContent(/n'a pas pu être envoyé/);

    await act(async () => {
      fireEvent.click(
        within(line).getByRole("button", { name: "Annuler l'invitation de ivy@example.io" }),
      );
    });
    expect(confirm).toHaveBeenLastCalledWith(expect.stringMatching(/Annuler l'invitation/));
    expect(api.deleteUser).toHaveBeenCalledWith("u-ivy");
    expect(screen.getByRole("status")).toHaveTextContent("Invitation de ivy@example.io annulée.");
  });

  describe("workflows de tous les comptes (#243)", () => {
    type AdminWorkflow = Schemas["AdminWorkflow"];
    const wf = (
      owner_id: string,
      owner_email: string | null,
      id: string,
      name: string,
      node_count = 1,
    ): AdminWorkflow => ({ owner_id, owner_email, id, name, node_count });
    const LEGACY = wf("", null, "old", "Ancien", 2);
    const ALICE_WF = wf("u-alice", "alice@example.io", "wf-draft", "Factures", 3);
    const GONE = wf("u-gone", null, "orphan", "");

    function withWorkflows(items: AdminWorkflow[], total = items.length) {
      const api = fakeApi();
      api.listAllWorkflows = vi.fn().mockResolvedValue({ items, total, offset: 0, limit: 50 });
      return api as typeof api & { listAllWorkflows: ReturnType<typeof vi.fn> };
    }

    function section(): HTMLElement {
      return screen.getByRole("region", { name: "Workflows de tous les comptes" });
    }

    it("un super-admin voit les workflows de tous les comptes avec leur propriétaire", async () => {
      const api = withWorkflows([LEGACY, ALICE_WF, GONE]);
      await renderPanel(api);
      const list = section();
      await waitFor(() => expect(within(list).getByText("Factures")).toBeInTheDocument());
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({ q: "", offset: 0, limit: 50 });
      const alice = within(list).getByText("Factures").closest("tr") as HTMLElement;
      expect(within(alice).getByText("wf-draft")).toBeInTheDocument();
      expect(within(alice).getByText("alice@example.io")).toBeInTheDocument();
      expect(within(alice).getByText("3")).toBeInTheDocument();
      expect(within(list).getByText("Sans propriétaire")).toBeInTheDocument();
      // Sans nom : l'id tient lieu de titre ; compte disparu signalé.
      expect(within(list).getAllByText("orphan")).toHaveLength(2);
      expect(within(list).getByText("Compte introuvable (u-gone)")).toBeInTheDocument();
    });

    it("sans `*`, ni section ni bouton Workflows, et aucun appel", async () => {
      const api = withWorkflows([ALICE_WF]);
      await renderPanel(api, { superAdmin: false });
      expect(
        screen.queryByRole("region", { name: "Workflows de tous les comptes" }),
      ).not.toBeInTheDocument();
      expect(
        within(row("alice@example.io")).queryByRole("button", { name: /workflows/i }),
      ).not.toBeInTheDocument();
      expect(api.listAllWorkflows).not.toHaveBeenCalled();
    });

    it("filtre par propriétaire depuis la liste des comptes, puis retire le filtre", async () => {
      const api = withWorkflows([ALICE_WF]);
      await renderPanel(api);
      await act(async () => {
        fireEvent.click(
          within(row("alice@example.io")).getByRole("button", {
            name: "Voir les workflows de alice@example.io",
          }),
        );
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({
        owner: "u-alice",
        q: "",
        offset: 0,
        limit: 50,
      });
      expect(within(section()).getByText("Propriétaire : alice@example.io")).toBeInTheDocument();
      await act(async () => {
        fireEvent.click(
          within(section()).getByRole("button", { name: "Retirer le filtre par propriétaire" }),
        );
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({ q: "", offset: 0, limit: 50 });
    });

    it("filtre par propriétaire depuis une ligne, y compris les workflows sans propriétaire", async () => {
      const api = withWorkflows([LEGACY, ALICE_WF]);
      await renderPanel(api);
      await waitFor(() => expect(within(section()).getByText("Factures")).toBeInTheDocument());
      await act(async () => {
        fireEvent.click(
          within(section()).getByRole("button", { name: "Workflows de Sans propriétaire" }),
        );
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({
        owner: "",
        q: "",
        offset: 0,
        limit: 50,
      });
      // Filtre actif : le propriétaire n'est plus un lien.
      expect(
        within(section()).queryByRole("button", { name: /^Workflows de / }),
      ).not.toBeInTheDocument();
    });

    it("recherche par id ou nom et pagine ; un autre propriétaire repart de la première page", async () => {
      const api = withWorkflows([ALICE_WF], 3);
      await renderPanel(api, { pageSize: 1 });
      const list = section();
      await waitFor(() => expect(within(list).getByText("Factures")).toBeInTheDocument());
      await act(async () => {
        fireEvent.click(
          within(
            within(list).getByRole("navigation", { name: "Pagination des workflows" }),
          ).getByRole("button", { name: "Suivant" }),
        );
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({ q: "", offset: 1, limit: 1 });

      fireEvent.change(within(list).getByLabelText("Rechercher un workflow par id ou nom"), {
        target: { value: " fact " },
      });
      await act(async () => {
        fireEvent.click(within(list).getByRole("button", { name: "Rechercher" }));
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({ q: "fact", offset: 0, limit: 1 });

      await act(async () => {
        fireEvent.click(
          within(list).getByRole("navigation", { name: "Pagination des workflows" })
            .querySelector("button:last-child") as HTMLElement,
        );
      });
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({ q: "fact", offset: 1, limit: 1 });
      const calls = api.listAllWorkflows.mock.calls.length;
      await act(async () => {
        fireEvent.click(within(list).getByRole("button", { name: "Workflows de alice@example.io" }));
      });
      expect(api.listAllWorkflows).toHaveBeenCalledTimes(calls + 1);
      expect(api.listAllWorkflows).toHaveBeenLastCalledWith({
        owner: "u-alice",
        q: "fact",
        offset: 0,
        limit: 1,
      });
    });

    it("signale une liste refusée", async () => {
      const api = fakeApi();
      api.listAllWorkflows = vi.fn().mockRejectedValue(new ApiError("GET a échoué (403)", 403));
      await renderPanel(api);
      expect(
        await within(section()).findByText(
          "Liste des workflows indisponible : réservée aux super-administrateurs",
        ),
      ).toBeInTheDocument();
    });
  });
});
