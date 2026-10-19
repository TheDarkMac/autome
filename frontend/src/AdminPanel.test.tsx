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
  };
}

function row(email: string): HTMLElement {
  return screen.getByText(email).closest("tr") as HTMLElement;
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
  await waitFor(() => expect(screen.getByText("alice@example.io")).toBeInTheDocument());
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

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Suivant" }));
    });
    expect(api.listUsers).toHaveBeenLastCalledWith({ q: "", offset: 1, limit: 1 });

    fireEvent.change(screen.getByLabelText("Rechercher par email"), {
      target: { value: " alice " },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Rechercher" }));
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
});
