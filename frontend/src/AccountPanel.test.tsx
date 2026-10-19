import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { AccountPanel, DELETE_CONFIRMATION_WORD } from "./AccountPanel";
import { validatePasswordChange } from "./auth";

function fill(current: string, next: string, confirm: string): void {
  fireEvent.change(screen.getByLabelText("Mot de passe actuel"), {
    target: { value: current },
  });
  fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
    target: { value: next },
  });
  fireEvent.change(screen.getByLabelText("Confirmer le nouveau mot de passe"), {
    target: { value: confirm },
  });
}

async function submit(): Promise<void> {
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Changer le mot de passe" }));
  });
}

describe("validatePasswordChange", () => {
  it("accepte une saisie cohérente", () => {
    expect(validatePasswordChange("old-pass", "new-password", "new-password")).toBeNull();
  });

  it("signale chaque erreur de saisie", () => {
    expect(validatePasswordChange("", "new-password", "new-password")).toMatch(/actuel/);
    expect(validatePasswordChange("old-pass", "short", "short")).toMatch(/8 caractères/);
    expect(validatePasswordChange("same-pass", "same-pass", "same-pass")).toMatch(/différer/);
    expect(validatePasswordChange("old-pass", "new-password", "other-pass")).toMatch(
      /confirmation/,
    );
  });
});

describe("<AccountPanel />", () => {
  it("change le mot de passe et vide le formulaire", async () => {
    const changePassword = vi.fn().mockResolvedValue(undefined);
    render(<AccountPanel email="a@b.io" changePassword={changePassword} />);
    fill("old-password", "new-password", "new-password");
    await submit();

    expect(changePassword).toHaveBeenCalledWith("old-password", "new-password");
    expect(screen.getByRole("status")).toHaveTextContent(/autres appareils ont été déconnectés/);
    expect(screen.getByLabelText("Mot de passe actuel")).toHaveValue("");
  });

  it("ne contacte pas le serveur si la confirmation diffère", async () => {
    const changePassword = vi.fn();
    render(<AccountPanel changePassword={changePassword} />);
    fill("old-password", "new-password", "typo-password");
    await submit();

    expect(changePassword).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent(/confirmation/);
  });

  it("affiche le refus du serveur sans vider la saisie", async () => {
    const changePassword = vi
      .fn()
      .mockRejectedValue(new Error("mot de passe actuel incorrect"));
    render(<AccountPanel changePassword={changePassword} />);
    fill("bad-password", "new-password", "new-password");
    await submit();

    expect(screen.getByRole("alert")).toHaveTextContent("mot de passe actuel incorrect");
    expect(screen.getByLabelText("Nouveau mot de passe")).toHaveValue("new-password");
  });
});

describe("<AccountPanel /> — suppression du compte (#229)", () => {
  const noChange = vi.fn();

  function fillDelete(password: string, confirmation: string): void {
    fireEvent.change(screen.getByLabelText("Votre mot de passe"), {
      target: { value: password },
    });
    fireEvent.change(screen.getByLabelText(/pour confirmer/), {
      target: { value: confirmation },
    });
  }

  function deleteButton(): HTMLElement {
    return screen.getByRole("button", { name: "Supprimer définitivement mon compte" });
  }

  it("est masquée sans action de suppression", () => {
    render(<AccountPanel email="a@b.io" changePassword={noChange} />);
    expect(screen.queryByRole("form", { name: "Supprimer mon compte" })).toBeNull();
  });

  it("liste ce qui sera effacé et exige le mot de passe et l'email retapé", async () => {
    const deleteAccount = vi.fn().mockResolvedValue(undefined);
    render(
      <AccountPanel email="sam@example.io" changePassword={noChange} deleteAccount={deleteAccount} />,
    );
    const form = screen.getByRole("form", { name: "Supprimer mon compte" });
    expect(form).toHaveTextContent(/workflows/);
    expect(form).toHaveTextContent(/exécutions/);
    expect(form).toHaveTextContent(/connexions aux services/);
    expect(form).toHaveTextContent(/identifiant\s+anonyme/);
    expect(deleteButton()).toBeDisabled();

    fillDelete("the-password", "autre@example.io");
    expect(deleteButton()).toBeDisabled();
    fillDelete("the-password", "  SAM@example.io ");
    expect(deleteButton()).toBeEnabled();
    await act(async () => {
      fireEvent.click(deleteButton());
    });
    expect(deleteAccount).toHaveBeenCalledWith("the-password");
  });

  it("affiche le refus du serveur et permet de réessayer", async () => {
    const deleteAccount = vi
      .fn()
      .mockRejectedValue(new Error("mot de passe actuel incorrect"));
    render(
      <AccountPanel email="sam@example.io" changePassword={noChange} deleteAccount={deleteAccount} />,
    );
    fillDelete("wrong", "sam@example.io");
    await act(async () => {
      fireEvent.click(deleteButton());
    });
    expect(screen.getByRole("alert")).toHaveTextContent("mot de passe actuel incorrect");
    expect(deleteButton()).toBeEnabled();
  });

  it("sans email connu, la confirmation est un mot fixe", () => {
    render(<AccountPanel changePassword={noChange} deleteAccount={vi.fn()} />);
    fillDelete("the-password", DELETE_CONFIRMATION_WORD);
    expect(deleteButton()).toBeEnabled();
  });
});
