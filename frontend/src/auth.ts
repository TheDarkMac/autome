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
//   - POST /auth/signup   { email, password }      → TokenPair
//   - POST /auth/refresh  { refresh_token }        → TokenPair
//   - POST /auth/logout   { refresh_token } (+ Bearer access) → 204
//   - POST /auth/logout-all (Bearer access)        → 204
//   - POST /auth/password { current_password, new_password } (Bearer) → TokenPair
//   - GET  /auth/whoami   (Bearer access)          → "hello <sub>"
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
  return postCredentials("/auth/login", creds, fetchImpl);
}

export async function signup(
  creds: Credentials,
  fetchImpl: typeof fetch = fetch,
): Promise<TokenPair> {
  return postCredentials("/auth/signup", creds, fetchImpl);
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

async function postCredentials(
  path: string,
  creds: Credentials,
  fetchImpl: typeof fetch,
): Promise<TokenPair> {
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
  if (res.status === 409) {
    throw new ApiError("un compte existe déjà pour cet email", 409);
  }
  if (res.status === 429) {
    throw new ApiError(tooManyAttemptsMessage(res), 429);
  }
  if (!res.ok) {
    throw new ApiError(`POST ${path} a échoué (${res.status})`, res.status);
  }
  return parseTokenPair(await res.json());
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
