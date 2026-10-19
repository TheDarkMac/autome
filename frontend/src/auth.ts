// Client d'authentification du frontend Autome (F8.6.a + F8.6.b).
//
// Le backend expose un middleware JWT (`Authorization: Bearer <token>`,
// cf. `crates/api/src/auth.rs`) et émet une paire access/refresh
// (`autome_core::TokenService`). Côté SPA on n'a accès qu'aux jetons
// signés : leur payload est lisible (base64url) mais NON vérifiable —
// on s'en sert uniquement pour connaître `sub`/`exp` et planifier le
// refresh ; la confiance reste côté backend qui re-vérifie la signature.
//
// Contrat HTTP attendu (frontend-first, comme `runs.ts`/`credentials.ts`) :
//   - POST /auth/login    { email, password }      → TokenPair
//   - POST /auth/signup   { email, password }      → 201 TokenPair, ou 202
//     { email, verification_required } quand l'adresse doit être vérifiée (#231)
//   - POST /auth/refresh  { refresh_token }        → TokenPair
//   - POST /auth/logout   { refresh_token } (+ Bearer access) → 204
//   - POST /auth/logout-all (Bearer access)        → 204
//   - POST /auth/password { current_password, new_password } (Bearer) → TokenPair
//   - POST /auth/password/forgot { email }          → 202 (compte existant ou non)
//   - POST /auth/password/reset  { token, new_password } → 204
//   - POST /auth/email/verify    { token }          → 204 (#231)
//   - POST /auth/email/resend    { email }          → 202 (compte existant ou non)
//   - GET  /auth/whoami   (Bearer access)          → "hello <sub>"
//   - GET  /auth/me       (Bearer access)          → { id, email, role, permissions } (cf. apiClient)
//
// `fetchImpl` est injectable partout pour les tests.

import { ApiError, apiBaseUrl } from "./api";

/** Paire de jetons renvoyée par login/signup/refresh. */
export interface TokenPair {
  readonly accessToken: string;
  readonly refreshToken: string;
}

/** Claims lisibles d'un JWT Autome (cf. `autome_core::Claims`). */
export interface JwtClaims {
  readonly sub: string;
  readonly iat: number;
  readonly exp: number;
  readonly jti: string;
  readonly kind: "access" | "refresh";
}

/** Issue d'une inscription : connecté, ou adresse à vérifier d'abord (#231). */
export type SignupResult =
  | { readonly kind: "signedIn"; readonly tokens: TokenPair }
  | { readonly kind: "verificationRequired"; readonly email: string };

/**
 * Connexion refusée (403) parce que l'adresse n'est pas encore vérifiée
 * (#231) : l'interface propose alors de renvoyer le lien.
 */
export class EmailNotVerifiedError extends ApiError {
  constructor(public readonly email: string) {
    super(
      "votre adresse email n'est pas encore vérifiée : ouvrez le lien reçu par email",
      403,
    );
    this.name = "EmailNotVerifiedError";
  }
}

/** Identifiants saisis dans le formulaire login/signup. */
export interface Credentials {
  readonly email: string;
  readonly password: string;
}

// ─────────────────────────── appels réseau ───────────────────────────

export async function login(
  creds: Credentials,
  fetchImpl: typeof fetch = fetch,
): Promise<TokenPair> {
  const res = await postCredentials("/auth/login", creds, fetchImpl);
  return parseTokenPair(await res.json());
}

/**
 * Crée un compte. Si le serveur exige la vérification de l'adresse (202), il
 * n'émet aucun jeton : un lien part vers `email` et la connexion attendra.
 */
export async function signup(
  creds: Credentials,
  fetchImpl: typeof fetch = fetch,
): Promise<SignupResult> {
  const res = await postCredentials("/auth/signup", creds, fetchImpl);
  if (res.status === 202) {
    const body = ((await res.json().catch(() => null)) ?? {}) as Record<string, unknown>;
    const email = typeof body["email"] === "string" ? body["email"] : creds.email.trim();
    return { kind: "verificationRequired", email };
  }
  return { kind: "signedIn", tokens: parseTokenPair(await res.json()) };
}

/** Échange le refresh token contre une nouvelle paire (rotation). */
export async function refresh(
  refreshToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<TokenPair> {
  const res = await postJson(
    "/auth/refresh",
    { refresh_token: refreshToken },
    fetchImpl,
  );
  if (res.status === 401) {
    throw new ApiError("refresh token invalide ou expiré", 401);
  }
  if (!res.ok) {
    throw new ApiError(`POST /auth/refresh a échoué (${res.status})`, res.status);
  }
  return parseTokenPair(await res.json());
}

/**
 * Ferme la session côté serveur (#184) : révoque le refresh token et, passé
 * en Bearer, l'access token. Sans cet appel, vider le stockage local laisse
 * les jetons valides jusqu'à leur expiration. Un 401 (refresh déjà expiré)
 * n'est pas une erreur : il n'y a plus de session à fermer.
 */
export async function logout(
  tokens: TokenPair,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  const res = await postJson(
    "/auth/logout",
    { refresh_token: tokens.refreshToken },
    fetchImpl,
    { Authorization: `Bearer ${tokens.accessToken}` },
  );
  if (!res.ok && res.status !== 401) {
    throw new ApiError(`POST /auth/logout a échoué (${res.status})`, res.status);
  }
}

/**
 * Ferme toutes les sessions de l'utilisateur, sur tous ses appareils (#186) :
 * tous ses jetons, y compris ceux de la session courante, sont refusés
 * ensuite. `fetchImpl` doit porter le Bearer (l'`authFetch` de la session).
 */
export async function logoutAll(fetchImpl: typeof fetch): Promise<void> {
  let res: Response;
  try {
    res = await fetchImpl(`${apiBaseUrl()}/auth/logout-all`, {
      method: "POST",
      headers: { Accept: "application/json" },
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/logout-all a échoué (${res.status})`,
      res.status,
    );
  }
}

/**
 * Change le mot de passe (#196) : toutes les sessions de l'utilisateur sont
 * fermées côté serveur, et la nouvelle paire renvoyée remplace celle de
 * l'appareil courant. `fetchImpl` doit porter le Bearer (`authFetch`).
 */
export async function changePassword(
  currentPassword: string,
  newPassword: string,
  fetchImpl: typeof fetch,
): Promise<TokenPair> {
  if (newPassword.length < MIN_PASSWORD_LENGTH) {
    throw new ApiError(
      `le nouveau mot de passe doit faire au moins ${MIN_PASSWORD_LENGTH} caractères`,
      400,
    );
  }
  const res = await postJson(
    "/auth/password",
    { current_password: currentPassword, new_password: newPassword },
    fetchImpl,
  );
  if (res.status === 403) {
    throw new ApiError("mot de passe actuel incorrect", 403);
  }
  if (res.status === 400) {
    throw new ApiError(
      (await res.text()) || "nouveau mot de passe refusé",
      400,
    );
  }
  if (res.status === 409) {
    throw new ApiError(
      "le mot de passe vient d'être changé ailleurs : reconnectez-vous",
      409,
    );
  }
  if (res.status === 429) {
    throw new ApiError(tooManyAttemptsMessage(res), 429);
  }
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/password a échoué (${res.status})`,
      res.status,
    );
  }
  return parseTokenPair(await res.json());
}

/**
 * Supprime définitivement le compte et ses données (#229, RGPD) après
 * vérification du mot de passe actuel ; toutes les sessions sont fermées
 * côté serveur. `fetchImpl` doit porter le Bearer (`authFetch`).
 */
export async function deleteAccount(
  currentPassword: string,
  fetchImpl: typeof fetch,
): Promise<void> {
  let res: Response;
  try {
    res = await fetchImpl(`${apiBaseUrl()}/auth/me`, {
      method: "DELETE",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify({ current_password: currentPassword }),
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (res.status === 403) {
    throw new ApiError("mot de passe actuel incorrect", 403);
  }
  if (res.status === 409) {
    throw new ApiError(
      "un compte administrateur ne peut pas être supprimé : un autre administrateur doit d'abord lui retirer ce rôle",
      409,
    );
  }
  if (res.status === 429) {
    throw new ApiError(tooManyAttemptsMessage(res), 429);
  }
  if (res.status === 503) {
    throw new ApiError(
      "la suppression de compte n'est pas disponible sur ce serveur",
      503,
    );
  }
  if (!res.ok) {
    throw new ApiError(`DELETE /auth/me a échoué (${res.status})`, res.status);
  }
}

/**
 * Demande un lien de réinitialisation du mot de passe (#204). Le serveur
 * répond 202 que le compte existe ou non : la réussite ne dit pas qu'un
 * email est parti, seulement que la demande est prise en compte.
 */
export async function requestPasswordReset(
  email: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  const trimmed = email.trim();
  if (!EMAIL_RE.test(trimmed)) throw new ApiError("Email invalide.", 400);
  const res = await postJson("/auth/password/forgot", { email: trimmed }, fetchImpl);
  if (res.status === 400) throw new ApiError("Email invalide.", 400);
  if (res.status === 429) throw new ApiError(tooManyAttemptsMessage(res), 429);
  if (res.status === 503) {
    throw new ApiError(
      "la réinitialisation par email n'est pas disponible sur ce serveur : contactez un administrateur",
      503,
    );
  }
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/password/forgot a échoué (${res.status})`,
      res.status,
    );
  }
}

/**
 * Choisit un nouveau mot de passe avec le jeton reçu par email (#204). Toutes
 * les sessions du compte sont fermées : il faut ensuite se connecter.
 */
export async function resetPassword(
  token: string,
  newPassword: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  if (newPassword.length < MIN_PASSWORD_LENGTH) {
    throw new ApiError(
      `le nouveau mot de passe doit faire au moins ${MIN_PASSWORD_LENGTH} caractères`,
      400,
    );
  }
  const res = await postJson(
    "/auth/password/reset",
    { token, new_password: newPassword },
    fetchImpl,
  );
  if (res.status === 400) {
    const reason = await res.text();
    throw new ApiError(
      reason === INVALID_RESET_LINK
        ? "ce lien de réinitialisation est invalide, expiré ou déjà utilisé : demandez-en un nouveau"
        : reason || "nouveau mot de passe refusé",
      400,
    );
  }
  if (res.status === 503) {
    throw new ApiError(
      "la réinitialisation par email n'est pas disponible sur ce serveur",
      503,
    );
  }
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/password/reset a échoué (${res.status})`,
      res.status,
    );
  }
}

/** Corps de la réponse 400 du serveur pour un jeton refusé. */
const INVALID_RESET_LINK = "invalid or expired reset link";

/**
 * Renvoie un lien de vérification de l'adresse (#231). Le serveur répond 202
 * que le compte existe ou non (et rien n'est envoyé s'il est déjà vérifié).
 */
export async function resendVerification(
  email: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  const trimmed = email.trim();
  if (!EMAIL_RE.test(trimmed)) throw new ApiError("Email invalide.", 400);
  const res = await postJson("/auth/email/resend", { email: trimmed }, fetchImpl);
  if (res.status === 400) throw new ApiError("Email invalide.", 400);
  if (res.status === 429) throw new ApiError(tooManyAttemptsMessage(res), 429);
  if (res.status === 503) {
    throw new ApiError(
      "la vérification de l'email n'est pas activée sur ce serveur",
      503,
    );
  }
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/email/resend a échoué (${res.status})`,
      res.status,
    );
  }
}

/** Consomme le lien de vérification reçu par email (#231). */
export async function verifyEmail(
  token: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  const res = await postJson("/auth/email/verify", { token }, fetchImpl);
  if (res.status === 400) {
    throw new ApiError(
      "ce lien de vérification est invalide ou expiré : connectez-vous pour en recevoir un nouveau",
      400,
    );
  }
  if (res.status === 503) {
    throw new ApiError(
      "la vérification de l'email n'est pas activée sur ce serveur",
      503,
    );
  }
  if (!res.ok) {
    throw new ApiError(
      `POST /auth/email/verify a échoué (${res.status})`,
      res.status,
    );
  }
}

/** Préfixe du fragment des liens de vérification (`…/#verify=<jeton>`). */
export const VERIFY_HASH_PREFIX = "#verify=";

/** Jeton de vérification porté par le fragment de l'URL, ou `null`. */
export function readVerifyToken(hash: string): string | null {
  return readHashToken(hash, VERIFY_HASH_PREFIX);
}

function readHashToken(hash: string, prefix: string): string | null {
  if (!hash.startsWith(prefix)) return null;
  const token = hash.slice(prefix.length).trim();
  return token.length > 0 ? token : null;
}

/** Préfixe du fragment des liens envoyés par email (`…/#reset=<jeton>`). */
export const RESET_HASH_PREFIX = "#reset=";

/**
 * Jeton de réinitialisation porté par le fragment de l'URL, ou `null`. Le
 * jeton voyage dans le fragment pour ne jamais atteindre les journaux du
 * serveur qui sert le frontend.
 */
export function readResetToken(hash: string): string | null {
  return readHashToken(hash, RESET_HASH_PREFIX);
}

/** Récupère le `sub` de l'utilisateur courant via `/auth/whoami`. */
export async function whoami(
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<string> {
  let res: Response;
  try {
    res = await fetchImpl(`${apiBaseUrl()}/auth/whoami`, {
      headers: { Authorization: `Bearer ${accessToken}`, Accept: "text/plain" },
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (!res.ok) {
    throw new ApiError(`GET /auth/whoami a échoué (${res.status})`, res.status);
  }
  const text = (await res.text()).trim();
  // Le handler de démo renvoie « hello <sub> » ; on tolère aussi un
  // identifiant brut au cas où le backend évoluerait.
  const m = /^hello\s+(.+)$/i.exec(text);
  return m?.[1] ?? text;
}

/** Corps de la réponse 403 du login pour une adresse non vérifiée (#231). */
const EMAIL_NOT_VERIFIED = "email not verified";

/** POST login/signup ; renvoie la réponse réussie, lève sinon. */
async function postCredentials(
  path: string,
  creds: Credentials,
  fetchImpl: typeof fetch,
): Promise<Response> {
  const errors = validateCredentials(creds);
  if (errors.length > 0) {
    throw new ApiError(errors.join(" "), 400);
  }
  const res = await postJson(
    path,
    { email: creds.email.trim(), password: creds.password },
    fetchImpl,
  );
  if (res.status === 401) {
    throw new ApiError("email ou mot de passe incorrect", 401);
  }
  if (res.status === 403) {
    if ((await res.text().catch(() => "")) === EMAIL_NOT_VERIFIED) {
      throw new EmailNotVerifiedError(creds.email.trim());
    }
    throw new ApiError("ce compte est suspendu : contactez un administrateur", 403);
  }
  if (res.status === 409) {
    throw new ApiError("un compte existe déjà pour cet email", 409);
  }
  if (res.status === 429) {
    throw new ApiError(tooManyAttemptsMessage(res), 429);
  }
  if (!res.ok) {
    throw new ApiError(`POST ${path} a échoué (${res.status})`, res.status);
  }
  return res;
}

/**
 * Message d'un 429 (anti-bruteforce par compte ou par adresse IP) : délai
 * tiré de `Retry-After`, arrondi à la minute supérieure.
 */
function tooManyAttemptsMessage(res: Response): string {
  const seconds = Number(res.headers.get("Retry-After"));
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return "trop de tentatives : réessayez plus tard";
  }
  const minutes = Math.ceil(seconds / 60);
  return `trop de tentatives : réessayez dans ${minutes} min`;
}

async function postJson(
  path: string,
  body: unknown,
  fetchImpl: typeof fetch,
  extraHeaders: Record<string, string> = {},
): Promise<Response> {
  try {
    return await fetchImpl(`${apiBaseUrl()}${path}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        ...extraHeaders,
      },
      body: JSON.stringify(body),
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
}

function parseTokenPair(raw: unknown): TokenPair {
  const v = (raw ?? {}) as Record<string, unknown>;
  const accessToken =
    typeof v["access_token"] === "string" ? v["access_token"] : null;
  const refreshToken =
    typeof v["refresh_token"] === "string" ? v["refresh_token"] : null;
  if (!accessToken || !refreshToken) {
    throw new ApiError("réponse d'authentification mal formée");
  }
  return { accessToken, refreshToken };
}

// ─────────────────────── validation (pure) ───────────────────────

// Volontairement permissif : la vraie politique de mot de passe vit côté
// backend. On bloque seulement les saisies évidemment invalides pour
// éviter un aller-retour réseau inutile.
const EMAIL_RE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
export const MIN_PASSWORD_LENGTH = 8;

/** Erreurs de saisie d'un changement de mot de passe, sans aller-retour réseau. */
export function validatePasswordChange(
  current: string,
  next: string,
  confirm: string,
): string | null {
  if (current.length === 0) return "saisissez votre mot de passe actuel";
  if (next.length < MIN_PASSWORD_LENGTH) {
    return `le nouveau mot de passe doit faire au moins ${MIN_PASSWORD_LENGTH} caractères`;
  }
  if (next === current) return "le nouveau mot de passe doit différer de l'actuel";
  if (next !== confirm) return "la confirmation ne correspond pas";
  return null;
}

/** Erreurs de saisie d'une réinitialisation, sans aller-retour réseau. */
export function validatePasswordReset(next: string, confirm: string): string | null {
  if (next.length < MIN_PASSWORD_LENGTH) {
    return `le nouveau mot de passe doit faire au moins ${MIN_PASSWORD_LENGTH} caractères`;
  }
  if (next !== confirm) return "la confirmation ne correspond pas";
  return null;
}

/** Liste des erreurs de saisie (vide ⇒ OK). Pur et testable. */
export function validateCredentials(creds: Credentials): string[] {
  const errors: string[] = [];
  if (!EMAIL_RE.test(creds.email.trim())) {
    errors.push("Email invalide.");
  }
  if (creds.password.length < MIN_PASSWORD_LENGTH) {
    errors.push(
      `Le mot de passe doit faire au moins ${MIN_PASSWORD_LENGTH} caractères.`,
    );
  }
  return errors;
}

// ─────────────────────── décodage JWT (pur) ───────────────────────

/**
 * Décode le payload d'un JWT **sans vérifier la signature** — usage
 * strictement local (connaître `sub`/`exp`). Renvoie `null` si le jeton
 * est malformé ou si le payload n'a pas la forme attendue.
 */
export function decodeJwt(token: string): JwtClaims | null {
  const parts = token.split(".");
  if (parts.length !== 3) return null;
  const payload = parts[1];
  if (!payload) return null;
  let json: unknown;
  try {
    json = JSON.parse(base64UrlDecode(payload));
  } catch {
    return null;
  }
  if (typeof json !== "object" || json === null) return null;
  const v = json as Record<string, unknown>;
  const sub = typeof v["sub"] === "string" ? v["sub"] : null;
  const exp = typeof v["exp"] === "number" ? v["exp"] : null;
  if (sub === null || exp === null) return null;
  const kind = v["kind"] === "refresh" ? "refresh" : "access";
  return {
    sub,
    exp,
    iat: typeof v["iat"] === "number" ? v["iat"] : 0,
    jti: typeof v["jti"] === "string" ? v["jti"] : "",
    kind,
  };
}

/** Instant d'expiration (ms epoch) du jeton, ou `null` si indécodable. */
export function tokenExpiresAtMs(token: string): number | null {
  const claims = decodeJwt(token);
  return claims ? claims.exp * 1000 : null;
}

/**
 * `true` si le jeton est expiré (ou indécodable). `skewMs` permet de
 * considérer comme expiré un jeton qui le sera bientôt — utile pour
 * déclencher un refresh proactif.
 */
export function isTokenExpired(
  token: string,
  nowMs: number = Date.now(),
  skewMs = 0,
): boolean {
  const expMs = tokenExpiresAtMs(token);
  if (expMs === null) return true;
  return nowMs + skewMs >= expMs;
}

function base64UrlDecode(input: string): string {
  const padded = input.replace(/-/g, "+").replace(/_/g, "/");
  const pad = padded.length % 4 === 0 ? "" : "=".repeat(4 - (padded.length % 4));
  const bytes = atob(padded + pad);
  // Décode l'UTF-8 (atob renvoie du Latin-1) pour gérer un `sub` non-ASCII.
  try {
    return decodeURIComponent(
      bytes
        .split("")
        .map((c) => `%${c.charCodeAt(0).toString(16).padStart(2, "0")}`)
        .join(""),
    );
  } catch {
    return bytes;
  }
}
