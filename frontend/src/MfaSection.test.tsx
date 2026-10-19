import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { ApiError } from "./api";
import { AccountPanel } from "./AccountPanel";
import type { MfaStatus } from "./mfa";
import { MfaSection, type MfaActions } from "./MfaSection";

const OFF: MfaStatus = { enabled: false, pending: false, recoveryCodesLeft: 0 };
const ON: MfaStatus = { enabled: true, pending: false, recoveryCodesLeft: 10 };
const CODES = Array.from({ length: 10 }, (_, i) => `code${i}-abcde`);

function actions(overrides: Partial<MfaActions> = {}): MfaActions {
  return {
    status: vi.fn().mockResolvedValue(OFF),
    setup: vi.fn().mockResolvedValue({
      secret: "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP",
      otpauthUri: "otpauth://totp/Autome:a%40b.io?secret=JBSW",
      qrSvg: '<svg xmlns="http://www.w3.org/2000/svg"></svg>',
    }),
    enable: vi.fn().mockResolvedValue(CODES),
    disable: vi.fn().mockResolvedValue(undefined),
    regenerate: vi.fn().mockResolvedValue(CODES),
    ...overrides,
  };
}

function type(label: string, value: string): void {
  fireEvent.change(screen.getByLabelText(label), { target: { value } });
}

describe("<MfaSection />", () => {
  it("active la double authentification : mot de passe, QR code, code, codes de secours", async () => {
    const api = actions();
    // Après « Terminé », l'état relu est actif.
    (api.status as ReturnType<typeof vi.fn>).mockResolvedValueOnce(OFF).mockResolvedValue(ON);
    render(<MfaSection actions={api} />);

    fireEvent.click(
      await screen.findByRole("button", { name: "Activer la double authentification" }),
    );
    type("Mot de passe actuel", "mon-mot-de-passe");
    fireEvent.click(screen.getByRole("button", { name: "Continuer" }));
    await waitFor(() => expect(api.setup).toHaveBeenCalledWith("mon-mot-de-passe"));

    const qr = await screen.findByAltText(/QR code/);
    expect(qr.getAttribute("src")).toMatch(/^data:image\/svg\+xml;charset=utf-8,/);
    expect(screen.getByTestId("mfa-secret")).toHaveTextContent(
      "JBSW Y3DP EHPK 3PXP JBSW Y3DP EHPK 3PXP",
    );
    type("Code de l'application", "123456");
    fireEvent.click(screen.getByRole("button", { name: "Activer" }));
    await waitFor(() => expect(api.enable).toHaveBeenCalledWith("123456"));

    const list = await screen.findByRole("list", { name: "Codes de secours" });
    expect(within(list).getAllByRole("listitem")).toHaveLength(10);
    expect(screen.getByRole("status")).toHaveTextContent(/autres appareils/);
    const done = screen.getByRole("button", { name: "Terminé" });
    expect(done).toBeDisabled();
    fireEvent.click(screen.getByLabelText("J'ai conservé mes codes de secours"));
    fireEvent.click(done);
    expect(await screen.findByText("Active")).toBeInTheDocument();
    expect(screen.getByText("10 codes de secours restants.")).toBeInTheDocument();
  });

  it("affiche les refus sans quitter l'étape", async () => {
    const api = actions({
      setup: vi.fn().mockRejectedValue(new ApiError("mot de passe actuel incorrect", 403)),
    });
    render(<MfaSection actions={api} />);
    fireEvent.click(
      await screen.findByRole("button", { name: "Activer la double authentification" }),
    );
    type("Mot de passe actuel", "faux");
    fireEvent.click(screen.getByRole("button", { name: "Continuer" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("mot de passe actuel incorrect");
    fireEvent.click(screen.getByRole("button", { name: "Annuler" }));
    expect(
      screen.getByRole("button", { name: "Activer la double authentification" }),
    ).toBeInTheDocument();
  });

  it("active : alerte sur les codes restants, régénère et désactive", async () => {
    const api = actions({
      status: vi.fn().mockResolvedValue({ ...ON, recoveryCodesLeft: 1 }),
    });
    render(<MfaSection actions={api} />);
    expect(await screen.findByText("1 code de secours restant.")).toHaveClass("text-amber-800");

    type("Code de l'application", "654321");
    fireEvent.click(screen.getByRole("button", { name: "Générer" }));
    await waitFor(() => expect(api.regenerate).toHaveBeenCalledWith("654321"));
    expect(await screen.findByRole("list", { name: "Codes de secours" })).toBeInTheDocument();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("désactive avec le mot de passe et un code", async () => {
    const api = actions({ status: vi.fn().mockResolvedValue(ON) });
    render(<MfaSection actions={api} />);
    await screen.findByText("Active");
    type("Mot de passe actuel", "mon-mot-de-passe");
    type("Code de l'application ou code de secours", "abcde-fghjk");
    fireEvent.click(
      screen.getByRole("button", { name: "Désactiver la double authentification" }),
    );
    await waitFor(() =>
      expect(api.disable).toHaveBeenCalledWith("mon-mot-de-passe", "abcde-fghjk"),
    );
    expect(
      await screen.findByRole("button", { name: "Activer la double authentification" }),
    ).toBeInTheDocument();
  });

  it("affiche une erreur de chargement", async () => {
    render(
      <MfaSection actions={actions({ status: vi.fn().mockRejectedValue(new Error("boom")) })} />,
    );
    expect(await screen.findByRole("alert")).toHaveTextContent("boom");
  });

  it("figure dans l'onglet Compte quand les actions sont fournies", async () => {
    const { rerender } = render(<AccountPanel changePassword={vi.fn()} mfa={actions()} />);
    expect(
      await screen.findByRole("heading", { name: "Double authentification" }),
    ).toBeInTheDocument();
    rerender(<AccountPanel changePassword={vi.fn()} />);
    expect(screen.queryByRole("heading", { name: "Double authentification" })).toBeNull();
  });
});
