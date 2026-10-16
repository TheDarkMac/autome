import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ApiError } from "./api";
import { ResetPasswordPage } from "./ResetPasswordPage";

function fill(next: string, confirm: string): void {
  fireEvent.change(screen.getByLabelText("Nouveau mot de passe"), {
    target: { value: next },
  });
  fireEvent.change(screen.getByLabelText("Confirmation"), {
    target: { value: confirm },
  });
}

describe("<ResetPasswordPage />", () => {
  it("envoie le jeton et le nouveau mot de passe puis appelle onDone", async () => {
    const resetPassword = vi.fn().mockResolvedValue(undefined);
    const onDone = vi.fn();
    render(
      <ResetPasswordPage
        token="uid.1.mac"
        resetPassword={resetPassword}
        onDone={onDone}
        onCancel={vi.fn()}
      />,
    );
    fill("new-password", "new-password");
    fireEvent.click(screen.getByRole("button", { name: "Enregistrer le mot de passe" }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(resetPassword).toHaveBeenCalledWith("uid.1.mac", "new-password");
  });

  it("valide la saisie sans appel réseau", async () => {
    const resetPassword = vi.fn();
    render(
      <ResetPasswordPage
        token="t"
        resetPassword={resetPassword}
        onDone={vi.fn()}
        onCancel={vi.fn()}
      />,
    );
    fill("new-password", "autre-chose");
    fireEvent.click(screen.getByRole("button", { name: "Enregistrer le mot de passe" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/confirmation/);
    fill("short", "short");
    fireEvent.click(screen.getByRole("button", { name: "Enregistrer le mot de passe" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/au moins/);
    expect(resetPassword).not.toHaveBeenCalled();
  });

  it("affiche le refus du serveur et permet de revenir à la connexion", async () => {
    const resetPassword = vi
      .fn()
      .mockRejectedValue(new ApiError("ce lien de réinitialisation est invalide", 400));
    const onDone = vi.fn();
    const onCancel = vi.fn();
    render(
      <ResetPasswordPage
        token="t"
        resetPassword={resetPassword}
        onDone={onDone}
        onCancel={onCancel}
      />,
    );
    fill("new-password", "new-password");
    fireEvent.click(screen.getByRole("button", { name: "Enregistrer le mot de passe" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("lien de réinitialisation");
    expect(onDone).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Enregistrer le mot de passe" })).toBeEnabled();

    fireEvent.click(screen.getByRole("button", { name: "Retour à la connexion" }));
    expect(onCancel).toHaveBeenCalled();
  });
});
