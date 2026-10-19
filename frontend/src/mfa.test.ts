import { describe, expect, it, vi } from "vitest";

import { login, MfaRequiredError } from "./auth";
import {
  disableMfa,
  enableMfa,
  fetchMfaStatus,
  MFA_REFUSALS,
  MfaChallengeExpiredError,
  regenerateRecoveryCodes,
  setupMfa,
  verifyMfa,
} from "./mfa";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function textResponse(body: string, status: number): Response {
  return new Response(body, { status });
}

const TOKENS = { access_token: "a.b.c", refresh_token: "d.e.f" };
const CODES = ["abcde-fghjk", "mnpqr-stvwx"];

describe("connexion avec second facteur (#245)", () => {
  it("un 202 du login lève MfaRequiredError avec le défi", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(
        jsonResponse({ mfa_required: true, mfa_token: "u.1.mac", expires_in: 300 }, 202),
      );
    const err = await login({ email: "a@b.io", password: "longenough" }, fetchImpl).catch(
      (e: unknown) => e,
    );
    expect(err).toBeInstanceOf(MfaRequiredError);
    expect((err as MfaRequiredError).mfaToken).toBe("u.1.mac");
  });

  it("un 202 sans défi est une réponse mal formée", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({ mfa_required: true }, 202));
    await expect(
      login({ email: "a@b.io", password: "longenough" }, fetchImpl),
    ).rejects.toMatchObject({ message: /défi manquant/ });
  });

  it("verifyMfa échange défi et code contre une paire", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(TOKENS));
    const pair = await verifyMfa("u.1.mac", " 123 456 ", fetchImpl);
    expect(pair).toEqual({ accessToken: "a.b.c", refreshToken: "d.e.f" });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(url).toMatch(/\/auth\/mfa\/verify$/);
    expect(JSON.parse(init.body)).toEqual({ mfa_token: "u.1.mac", code: "123 456" });
  });

  it("verifyMfa traduit les refus", async () => {
    const call = (res: Response) => verifyMfa("t", "123456", vi.fn().mockResolvedValue(res));
    await expect(call(textResponse(MFA_REFUSALS.INVALID_MFA_CHALLENGE, 400))).rejects.toBeInstanceOf(
      MfaChallengeExpiredError,
    );
    await expect(call(textResponse("invalid credentials", 401))).rejects.toMatchObject({
      status: 401,
      message: /incorrect ou déjà utilisé/,
    });
    await expect(call(textResponse("account disabled", 403))).rejects.toMatchObject({
      message: /suspendu/,
    });
    await expect(
      call(new Response("", { status: 429, headers: { "Retry-After": "120" } })),
    ).rejects.toMatchObject({ status: 429, message: /2 min/ });
    // Code vide : pas d'appel réseau.
    const fetchImpl = vi.fn();
    await expect(verifyMfa("t", "  ", fetchImpl)).rejects.toMatchObject({ status: 400 });
    expect(fetchImpl).not.toHaveBeenCalled();
  });
});

describe("gestion du second facteur (#245)", () => {
  it("lit l'état", async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(jsonResponse({ enabled: true, pending: false, recovery_codes_left: 7 }));
    expect(await fetchMfaStatus(fetchImpl)).toEqual({
      enabled: true,
      pending: false,
      recoveryCodesLeft: 7,
    });
    expect(fetchImpl.mock.calls[0]![0]).toMatch(/\/auth\/mfa$/);
  });

  it("enrôle, active, régénère et désactive", async () => {
    const setup = vi.fn().mockResolvedValue(
      jsonResponse({ secret: "JBSWY3DP", otpauth_uri: "otpauth://totp/x", qr_svg: "<svg/>" }),
    );
    expect(await setupMfa("pw-actuel", setup)).toEqual({
      secret: "JBSWY3DP",
      otpauthUri: "otpauth://totp/x",
      qrSvg: "<svg/>",
    });
    expect(JSON.parse(setup.mock.calls[0]![1].body)).toEqual({ current_password: "pw-actuel" });

    const enable = vi
      .fn()
      .mockResolvedValue(jsonResponse({ recovery_codes: CODES, session: TOKENS }));
    expect(await enableMfa("123456", enable)).toEqual({
      recoveryCodes: CODES,
      tokens: { accessToken: "a.b.c", refreshToken: "d.e.f" },
    });

    const regen = vi.fn().mockResolvedValue(jsonResponse({ recovery_codes: CODES }));
    expect(await regenerateRecoveryCodes("123456", regen)).toEqual(CODES);
    expect(regen.mock.calls[0]![0]).toMatch(/\/auth\/mfa\/recovery-codes$/);

    const disable = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    await disableMfa("pw-actuel", "abcde-fghjk", disable);
    expect(JSON.parse(disable.mock.calls[0]![1].body)).toEqual({
      current_password: "pw-actuel",
      code: "abcde-fghjk",
    });
  });

  it("traduit les refus des routes authentifiées", async () => {
    const respond = (res: Response) => vi.fn().mockResolvedValue(res);
    await expect(setupMfa("x", respond(textResponse("current password is incorrect", 403)))).rejects.toMatchObject({
      status: 403,
      message: "mot de passe actuel incorrect",
    });
    await expect(setupMfa("x", respond(textResponse(MFA_REFUSALS.MFA_ALREADY_ENABLED, 409)))).rejects.toMatchObject({
      status: 409,
      message: /déjà active/,
    });
    await expect(enableMfa("1", respond(textResponse(MFA_REFUSALS.INVALID_MFA_CODE, 403)))).rejects.toMatchObject({
      message: "code incorrect",
    });
    await expect(enableMfa("1", respond(textResponse(MFA_REFUSALS.MFA_SETUP_REQUIRED, 409)))).rejects.toMatchObject({
      message: /recommencez/,
    });
    await expect(
      disableMfa("x", "1", respond(textResponse(MFA_REFUSALS.MFA_NOT_ENABLED, 409))),
    ).rejects.toMatchObject({ message: /n'est pas active/ });
    await expect(regenerateRecoveryCodes("1", respond(textResponse("", 401)))).rejects.toMatchObject({
      status: 401,
    });
  });

  it("rejette des codes de secours mal formés", async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse({ recovery_codes: [1, 2] }));
    await expect(regenerateRecoveryCodes("123456", fetchImpl)).rejects.toMatchObject({
      message: /mal formés/,
    });
  });
});
