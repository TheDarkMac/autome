import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ApiError } from "./api";
import { LoginPage } from "./LoginPage";

function fill(email: string, password: string): void {
  fireEvent.change(screen.getByLabelText("Email"), { target: { value: email } });
  fireEvent.change(screen.getByLabelText("Mot de passe"), {
    target: { value: password },
  });
}

describe("<LoginPage />", () => {
  it("appelle login avec les identifiants valides", async () => {
    const login = vi.fn().mockResolvedValue(undefined);
    const signup = vi.fn();
    render(<LoginPage login={login} signup={signup} />);

    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    await waitFor(() =>
      expect(login).toHaveBeenCalledWith({
        email: "a@b.io",
        password: "longenough",
      }),
    );
    expect(signup).not.toHaveBeenCalled();
  });

  it("bloque la soumission et affiche l'erreur si la saisie est invalide", async () => {
    const login = vi.fn();
    render(<LoginPage login={login} signup={vi.fn()} />);

    fill("bad", "x");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/email/i);
    expect(login).not.toHaveBeenCalled();
  });

  it("affiche le message d'erreur serveur (401)", async () => {
    const login = vi.fn().mockRejectedValue(new ApiError("identifiants incorrects", 401));
    render(<LoginPage login={login} signup={vi.fn()} />);

    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("identifiants incorrects");
  });

  it("bascule en mode inscription et appelle signup", async () => {
    const signup = vi.fn().mockResolvedValue(undefined);
    render(<LoginPage login={vi.fn()} signup={signup} />);

    fireEvent.click(screen.getByRole("button", { name: "Créer un compte" }));
    expect(screen.getByRole("form", { name: "Inscription" })).toBeInTheDocument();

    fill("new@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Créer le compte" }));

    await waitFor(() =>
      expect(signup).toHaveBeenCalledWith({
        email: "new@b.io",
        password: "longenough",
      }),
    );
  });

  it("démarre directement en mode signup via initialMode", () => {
    render(<LoginPage login={vi.fn()} signup={vi.fn()} initialMode="signup" />);
    expect(screen.getByRole("form", { name: "Inscription" })).toBeInTheDocument();
  });

  it("réinitialise l'erreur en changeant de mode", async () => {
    render(<LoginPage login={vi.fn()} signup={vi.fn()} />);
    fill("bad", "x");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));
    expect(await screen.findByRole("alert")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Créer un compte" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("désactive le bouton pendant la soumission", async () => {
    let resolve!: () => void;
    const login = vi.fn().mockReturnValue(new Promise<void>((r) => (resolve = r)));
    render(<LoginPage login={login} signup={vi.fn()} />);

    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    const btn = await screen.findByRole("button", { name: /patienter/i });
    expect(btn).toBeDisabled();
    // Flush la résolution dans act pour réactiver le bouton sans warning.
    await act(async () => {
      resolve();
    });
    expect(screen.getByRole("button", { name: "Se connecter" })).toBeEnabled();
  });
});
