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
  if (!res.ok) {
    throw new ApiError(`POST ${path} a échoué (${res.status})`, res.status);
  }
  return parseTokenPair(await res.json());
}

async function postJson(
  path: string,
  body: unknown,
  fetchImpl: typeof fetch,
): Promise<Response> {
  try {
    return await fetchImpl(`${apiBaseUrl()}${path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
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
