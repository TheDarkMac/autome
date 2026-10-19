import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";

import { ApiError } from "./api";
import { VerifyEmailPage } from "./VerifyEmailPage";

describe("<VerifyEmailPage />", () => {
  it("envoie le jeton une seule fois puis appelle onDone", async () => {
    const verifyEmail = vi.fn().mockResolvedValue(undefined);
    const onDone = vi.fn();
    render(
      <StrictMode>
        <VerifyEmailPage
          token="uid.1.mac"
          verifyEmail={verifyEmail}
          onDone={onDone}
          onCancel={vi.fn()}
        />
      </StrictMode>,
    );
    expect(screen.getByRole("status")).toHaveTextContent("Vérification en cours");
    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(verifyEmail).toHaveBeenCalledTimes(1);
    expect(verifyEmail).toHaveBeenCalledWith("uid.1.mac");
  });

  it("explique un lien refusé et permet de revenir à la connexion", async () => {
    const verifyEmail = vi
      .fn()
      .mockRejectedValue(new ApiError("ce lien de vérification est invalide ou expiré", 400));
    const onCancel = vi.fn();
    const onDone = vi.fn();
    render(
      <VerifyEmailPage
        token="bad"
        verifyEmail={verifyEmail}
        onDone={onDone}
        onCancel={onCancel}
      />,
    );
    expect(await screen.findByRole("alert")).toHaveTextContent("invalide ou expiré");
    fireEvent.click(screen.getByRole("button", { name: "Retour à la connexion" }));
    expect(onCancel).toHaveBeenCalledOnce();
    expect(onDone).not.toHaveBeenCalled();
  });
});
