import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ApiError } from "./api";
import { EmailNotVerifiedError, MfaRequiredError } from "./auth";
import { MfaChallengeExpiredError } from "./mfa";
import {
  LoginPage,
  RESET_REQUESTED_MESSAGE,
  VERIFICATION_RESENT_MESSAGE,
} from "./LoginPage";

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

  describe("mot de passe oublié (#204)", () => {
    it("n'affiche pas le lien sans requestPasswordReset", () => {
      render(<LoginPage login={vi.fn()} signup={vi.fn()} />);
      expect(
        screen.queryByRole("button", { name: "Mot de passe oublié ?" }),
      ).not.toBeInTheDocument();
    });

    it("envoie la demande puis affiche une confirmation neutre", async () => {
      const requestPasswordReset = vi.fn().mockResolvedValue(undefined);
      render(
        <LoginPage
          login={vi.fn()}
          signup={vi.fn()}
          requestPasswordReset={requestPasswordReset}
        />,
      );
      fireEvent.change(screen.getByLabelText("Email"), { target: { value: "ana@b.io" } });
      fireEvent.click(screen.getByRole("button", { name: "Mot de passe oublié ?" }));
      const form = screen.getByRole("form", { name: "Mot de passe oublié" });
      expect(form).toBeInTheDocument();
      // L'email déjà saisi est repris.
      expect(screen.getByLabelText("Email")).toHaveValue("ana@b.io");

      fireEvent.click(screen.getByRole("button", { name: "Envoyer le lien" }));
      expect(await screen.findByRole("status")).toHaveTextContent(RESET_REQUESTED_MESSAGE);
      expect(requestPasswordReset).toHaveBeenCalledWith("ana@b.io");

      fireEvent.click(screen.getByRole("button", { name: "Retour à la connexion" }));
      expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
    });

    it("affiche l'erreur de la demande (429, 503…)", async () => {
      const requestPasswordReset = vi
        .fn()
        .mockRejectedValue(new ApiError("trop de tentatives : réessayez dans 15 min", 429));
      render(
        <LoginPage
          login={vi.fn()}
          signup={vi.fn()}
          requestPasswordReset={requestPasswordReset}
          initialMode="forgot"
        />,
      );
      fireEvent.change(screen.getByLabelText("Email"), { target: { value: "ana@b.io" } });
      fireEvent.click(screen.getByRole("button", { name: "Envoyer le lien" }));
      expect(await screen.findByRole("alert")).toHaveTextContent("15 min");
      expect(screen.queryByRole("status")).not.toBeInTheDocument();
    });

    it("affiche une information au-dessus du formulaire de connexion", () => {
      render(
        <LoginPage login={vi.fn()} signup={vi.fn()} notice="Mot de passe réinitialisé" />,
      );
      expect(screen.getByRole("status")).toHaveTextContent("Mot de passe réinitialisé");
    });
  });
});

describe("<LoginPage /> — vérification de l'email (#231)", () => {
  it("après une inscription à vérifier, invite à ouvrir le lien et le renvoie", async () => {
    const signup = vi.fn().mockResolvedValue({ email: "new@b.io" });
    const resendVerification = vi.fn().mockResolvedValue(undefined);
    render(
      <LoginPage login={vi.fn()} signup={signup} resendVerification={resendVerification} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Créer un compte" }));
    fill("New@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Créer le compte" }));

    const notice = await screen.findByRole("region", { name: "Vérification de l'email" });
    expect(notice).toHaveTextContent("Votre compte est créé.");
    expect(notice).toHaveTextContent("new@b.io");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Renvoyer le lien" }));
    });
    expect(resendVerification).toHaveBeenCalledWith("new@b.io");
    expect(screen.getByRole("status")).toHaveTextContent(VERIFICATION_RESENT_MESSAGE);

    fireEvent.click(screen.getByRole("button", { name: "Retour à la connexion" }));
    expect(screen.getByRole("form", { name: "Connexion" })).toBeInTheDocument();
  });

  it("une connexion refusée faute de vérification mène au même écran", async () => {
    const login = vi.fn().mockRejectedValue(new EmailNotVerifiedError("a@b.io"));
    const resendVerification = vi
      .fn()
      .mockRejectedValue(new ApiError("trop de tentatives : réessayez dans 5 min", 429));
    render(
      <LoginPage login={login} signup={vi.fn()} resendVerification={resendVerification} />,
    );
    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));

    expect(
      await screen.findByRole("region", { name: "Vérification de l'email" }),
    ).toHaveTextContent("Votre adresse n'est pas encore vérifiée.");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Renvoyer le lien" }));
    });
    expect(screen.getByRole("alert")).toHaveTextContent("réessayez dans 5 min");
  });

  it("une inscription connectée d'emblée ne montre pas l'écran", async () => {
    const signup = vi.fn().mockResolvedValue(null);
    render(<LoginPage login={vi.fn()} signup={signup} />);
    fireEvent.click(screen.getByRole("button", { name: "Créer un compte" }));
    fill("new@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Créer le compte" }));
    await waitFor(() => expect(signup).toHaveBeenCalled());
    expect(screen.queryByRole("region", { name: "Vérification de l'email" })).toBeNull();
  });
});

describe("<LoginPage /> — double authentification (#245)", () => {
  async function reachCodeStep(verifyMfa: (token: string, code: string) => Promise<void>) {
    const login = vi.fn().mockRejectedValue(new MfaRequiredError("u.1.mac"));
    render(<LoginPage login={login} signup={vi.fn()} verifyMfa={verifyMfa} />);
    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));
    await screen.findByRole("form", { name: "Vérification en deux étapes" });
  }

  it("demande le code puis le vérifie avec le défi", async () => {
    const verifyMfa = vi.fn().mockResolvedValue(undefined);
    await reachCodeStep(verifyMfa);
    fireEvent.change(screen.getByLabelText("Code de vérification"), {
      target: { value: "123456" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Vérifier" }));
    await waitFor(() => expect(verifyMfa).toHaveBeenCalledWith("u.1.mac", "123456"));
  });

  it("accepte un code de secours et affiche un code refusé", async () => {
    const verifyMfa = vi
      .fn()
      .mockRejectedValueOnce(new ApiError("code incorrect ou déjà utilisé", 401))
      .mockResolvedValue(undefined);
    await reachCodeStep(verifyMfa);
    fireEvent.click(screen.getByRole("button", { name: "Utiliser un code de secours" }));
    fireEvent.change(screen.getByLabelText("Code de secours"), {
      target: { value: "abcde-fghjk" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Vérifier" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("code incorrect ou déjà utilisé");
    expect(screen.getByLabelText("Code de secours")).toHaveValue("");
  });

  it("revient à la connexion quand le défi a expiré", async () => {
    const verifyMfa = vi.fn().mockRejectedValue(new MfaChallengeExpiredError());
    await reachCodeStep(verifyMfa);
    fireEvent.change(screen.getByLabelText("Code de vérification"), {
      target: { value: "123456" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Vérifier" }));
    expect(await screen.findByRole("form", { name: "Connexion" })).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent(/expiré/);
    expect(screen.getByLabelText("Mot de passe")).toHaveValue("");
  });

  it("sans verifyMfa, l'erreur s'affiche comme les autres", async () => {
    const login = vi.fn().mockRejectedValue(new MfaRequiredError("u.1.mac"));
    render(<LoginPage login={login} signup={vi.fn()} />);
    fill("a@b.io", "longenough");
    fireEvent.click(screen.getByRole("button", { name: "Se connecter" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("code de vérification requis");
  });
});
