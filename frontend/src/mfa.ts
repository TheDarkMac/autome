// Second facteur TOTP (#245) : client des routes `/auth/mfa*`.
//
// Contrat HTTP :
//   - POST /auth/mfa/verify   { mfa_token, code }            → TokenPair (public)
//   - GET  /auth/mfa          (Bearer)                       → { enabled, pending, recovery_codes_left }
//   - POST /auth/mfa/setup    { current_password } (Bearer)  → { secret, otpauth_uri, qr_svg }
//   - POST /auth/mfa/enable   { code } (Bearer)              → { recovery_codes, session }
//   - POST /auth/mfa/disable  { current_password, code } (Bearer) → 204
//   - POST /auth/mfa/recovery-codes { code } (Bearer)        → { recovery_codes }
//
// Sur les routes authentifiées, un mot de passe ou un code faux répond 403
// (pas 401 : la session est valide). `fetchImpl` doit alors porter le Bearer
// (l'`authFetch` de la session).

import { ApiError, apiBaseUrl } from "./api";
import {
  parseTokenPair,
  postJson,
  tooManyAttemptsMessage,
  type TokenPair,
} from "./auth";

/** État du second facteur du compte courant. */
export interface MfaStatus {
  readonly enabled: boolean;
  /** Enrôlement commencé, pas encore confirmé par un code. */
  readonly pending: boolean;
  readonly recoveryCodesLeft: number;
}

/** Enrôlement en attente : à scanner, puis confirmer par un code. */
export interface MfaSetup {
  /** Secret en base32 (saisie manuelle). */
  readonly secret: string;
  readonly otpauthUri: string;
  /** QR code SVG de l'URI, produit par le serveur. */
  readonly qrSvg: string;
}

/** MFA activée : codes de secours (montrés une fois) et paire neuve. */
export interface MfaEnabled {
  readonly recoveryCodes: string[];
  readonly tokens: TokenPair;
}

/** Corps des refus du serveur (cf. `crates/api/src/mfa.rs`). */
const INVALID_MFA_CHALLENGE = "invalid or expired mfa challenge";
const INVALID_MFA_CODE = "invalid mfa code";
const MFA_ALREADY_ENABLED = "mfa already enabled";
const MFA_SETUP_REQUIRED = "mfa setup required";
const MFA_NOT_ENABLED = "mfa not enabled";
const ACCOUNT_DISABLED = "account disabled";

/** Défi expiré ou périmé : il faut recommencer la connexion. */
export class MfaChallengeExpiredError extends ApiError {
  constructor() {
    super("la vérification a expiré : reconnectez-vous", 400);
    this.name = "MfaChallengeExpiredError";
  }
}

/**
 * Seconde étape de la connexion : échange le défi de `/auth/login` et un
 * code (application ou code de secours) contre une paire de jetons.
 */
export async function verifyMfa(
  mfaToken: string,
  code: string,
  fetchImpl: typeof fetch = fetch,
): Promise<TokenPair> {
  const trimmed = code.trim();
  if (trimmed === "") throw new ApiError("saisissez un code", 400);
  const res = await postJson("/auth/mfa/verify", { mfa_token: mfaToken, code: trimmed }, fetchImpl);
  if (res.status === 400) throw new MfaChallengeExpiredError();
  if (res.status === 401) {
    throw new ApiError("code incorrect ou déjà utilisé", 401);
  }
  if (res.status === 403) {
    const reason = await res.text().catch(() => "");
    throw new ApiError(
      reason === ACCOUNT_DISABLED
        ? "ce compte est suspendu : contactez un administrateur"
        : "votre adresse email n'est pas encore vérifiée : ouvrez le lien reçu par email",
      403,
    );
  }
  if (res.status === 429) throw new ApiError(tooManyAttemptsMessage(res), 429);
  if (!res.ok) {
    throw new ApiError(`POST /auth/mfa/verify a échoué (${res.status})`, res.status);
  }
  return parseTokenPair(await res.json());
}

/** État du second facteur du compte courant. */
export async function fetchMfaStatus(fetchImpl: typeof fetch): Promise<MfaStatus> {
  let res: Response;
  try {
    res = await fetchImpl(`${apiBaseUrl()}/auth/mfa`, {
      headers: { Accept: "application/json" },
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (!res.ok) throw new ApiError(`GET /auth/mfa a échoué (${res.status})`, res.status);
  const v = ((await res.json()) ?? {}) as Record<string, unknown>;
  return {
    enabled: v["enabled"] === true,
    pending: v["pending"] === true,
    recoveryCodesLeft:
      typeof v["recovery_codes_left"] === "number" ? v["recovery_codes_left"] : 0,
  };
}

/** Commence l'enrôlement (mot de passe actuel exigé). */
export async function setupMfa(
  currentPassword: string,
  fetchImpl: typeof fetch,
): Promise<MfaSetup> {
  const res = await postJson("/auth/mfa/setup", { current_password: currentPassword }, fetchImpl);
  await throwOnRefusal(res, "/auth/mfa/setup", "mot de passe actuel incorrect");
  const v = ((await res.json()) ?? {}) as Record<string, unknown>;
  const secret = typeof v["secret"] === "string" ? v["secret"] : "";
  const otpauthUri = typeof v["otpauth_uri"] === "string" ? v["otpauth_uri"] : "";
  const qrSvg = typeof v["qr_svg"] === "string" ? v["qr_svg"] : "";
  if (!secret || !otpauthUri) throw new ApiError("réponse d'enrôlement mal formée");
  return { secret, otpauthUri, qrSvg };
}

/**
 * Confirme l'enrôlement par un premier code : la MFA devient active, les
 * autres sessions sont fermées, la paire renvoyée remplace la courante.
 */
export async function enableMfa(code: string, fetchImpl: typeof fetch): Promise<MfaEnabled> {
  const res = await postJson("/auth/mfa/enable", { code: code.trim() }, fetchImpl);
  await throwOnRefusal(res, "/auth/mfa/enable", "code incorrect");
  const v = ((await res.json()) ?? {}) as Record<string, unknown>;
  return {
    recoveryCodes: parseCodes(v["recovery_codes"]),
    tokens: parseTokenPair(v["session"]),
  };
}

/** Désactive la MFA (mot de passe actuel et code exigés). */
export async function disableMfa(
  currentPassword: string,
  code: string,
  fetchImpl: typeof fetch,
): Promise<void> {
  const res = await postJson(
    "/auth/mfa/disable",
    { current_password: currentPassword, code: code.trim() },
    fetchImpl,
  );
  await throwOnRefusal(res, "/auth/mfa/disable", "mot de passe actuel ou code incorrect");
}

/** Remplace les codes de secours (code exigé) ; les anciens ne valent plus rien. */
export async function regenerateRecoveryCodes(
  code: string,
  fetchImpl: typeof fetch,
): Promise<string[]> {
  const res = await postJson("/auth/mfa/recovery-codes", { code: code.trim() }, fetchImpl);
  await throwOnRefusal(res, "/auth/mfa/recovery-codes", "code incorrect");
  const v = ((await res.json()) ?? {}) as Record<string, unknown>;
  return parseCodes(v["recovery_codes"]);
}

/** Traduit les refus des routes authentifiées ; ne fait rien si `res.ok`. */
async function throwOnRefusal(res: Response, path: string, forbidden: string): Promise<void> {
  if (res.ok) return;
  const reason = await res.text().catch(() => "");
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (res.status === 403) {
    throw new ApiError(reason === INVALID_MFA_CODE ? "code incorrect" : forbidden, 403);
  }
  if (res.status === 409) {
    const message =
      reason === MFA_ALREADY_ENABLED
        ? "la double authentification est déjà active"
        : reason === MFA_SETUP_REQUIRED
          ? "l'enrôlement a changé : recommencez depuis le début"
          : reason === MFA_NOT_ENABLED
            ? "la double authentification n'est pas active"
            : "requête en conflit : rechargez la page";
    throw new ApiError(message, 409);
  }
  if (res.status === 429) throw new ApiError(tooManyAttemptsMessage(res), 429);
  throw new ApiError(`POST ${path} a échoué (${res.status})`, res.status);
}

function parseCodes(raw: unknown): string[] {
  if (!Array.isArray(raw) || !raw.every((c) => typeof c === "string")) {
    throw new ApiError("codes de secours mal formés");
  }
  return raw as string[];
}

/** Corps des refus du serveur, exposés pour les tests de contrat. */
export const MFA_REFUSALS = {
  INVALID_MFA_CHALLENGE,
  INVALID_MFA_CODE,
  MFA_ALREADY_ENABLED,
  MFA_SETUP_REQUIRED,
  MFA_NOT_ENABLED,
} as const;
