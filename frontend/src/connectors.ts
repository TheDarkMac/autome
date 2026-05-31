// Client OAuth des connecteurs (F8.6.c).
//
// Flow PKCE piloté par la SPA (cf. `crates/api/src/credentials.rs`) :
//   1. POST /credentials/{provider}/authorize → { authorize_url, state, code_verifier }
//   2. on persiste { state, code_verifier, provider } puis on redirige le
//      navigateur vers `authorize_url` ;
//   3. le provider renvoie sur `?code=&state=` → POST /credentials/{provider}/callback
//      avec le `code_verifier` conservé ;
//   4. le backend persiste les tokens chiffrés et renvoie un `CredentialDto`.
//
// Tous les appels exigent le Bearer access token. `fetchImpl`, le store de
// challenge et la redirection sont injectables pour les tests.

import { ApiError, apiBaseUrl } from "./api";

/** Provider OAuth déclaré côté backend. */
export interface Provider {
  readonly providerId: string;
  readonly scopes: readonly string[];
}

/** Credential connecté de l'utilisateur courant (jamais le token brut). */
export interface ConnectedCredential {
  readonly providerId: string;
  readonly hasRefreshToken: boolean;
  readonly scope: string | null;
  readonly expiresAtMs: number | null;
}

/** Challenge PKCE renvoyé par `/authorize`, à conserver jusqu'au callback. */
export interface AuthorizeChallenge {
  readonly provider: string;
  readonly authorizeUrl: string;
  readonly state: string;
  readonly codeVerifier: string;
}

function authHeaders(accessToken: string): HeadersInit {
  return { Authorization: `Bearer ${accessToken}`, Accept: "application/json" };
}

async function getJson(
  path: string,
  accessToken: string,
  fetchImpl: typeof fetch,
): Promise<unknown> {
  let res: Response;
  try {
    res = await fetchImpl(`${apiBaseUrl()}${path}`, {
      headers: authHeaders(accessToken),
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (!res.ok) throw new ApiError(`GET ${path} a échoué (${res.status})`, res.status);
  return res.json();
}

export async function fetchProviders(
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<Provider[]> {
  const body = (await getJson("/credentials/providers", accessToken, fetchImpl)) as {
    items?: unknown;
  };
  const items = Array.isArray(body.items) ? body.items : [];
  const out: Provider[] = [];
  for (const it of items) {
    if (typeof it !== "object" || it === null) continue;
    const v = it as Record<string, unknown>;
    const providerId =
      typeof v["provider_id"] === "string" ? v["provider_id"] : null;
    if (!providerId) continue;
    const scopes = Array.isArray(v["scopes"])
      ? (v["scopes"] as unknown[]).filter((s): s is string => typeof s === "string")
      : [];
    out.push({ providerId, scopes });
  }
  return out;
}

export async function fetchConnectedCredentials(
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<ConnectedCredential[]> {
  const body = (await getJson("/credentials", accessToken, fetchImpl)) as {
    items?: unknown;
  };
  const items = Array.isArray(body.items) ? body.items : [];
  const out: ConnectedCredential[] = [];
  for (const it of items) {
    if (typeof it !== "object" || it === null) continue;
    const v = it as Record<string, unknown>;
    const providerId =
      typeof v["provider_id"] === "string" ? v["provider_id"] : null;
    if (!providerId) continue;
    out.push({
      providerId,
      hasRefreshToken: v["has_refresh_token"] === true,
      scope: typeof v["scope"] === "string" ? v["scope"] : null,
      expiresAtMs:
        typeof v["expires_at_ms"] === "number" ? v["expires_at_ms"] : null,
    });
  }
  return out;
}

/** Démarre le flow OAuth : renvoie le challenge (URL + state + verifier). */
export async function startAuthorize(
  provider: string,
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<AuthorizeChallenge> {
  let res: Response;
  try {
    res = await fetchImpl(
      `${apiBaseUrl()}/credentials/${encodeURIComponent(provider)}/authorize`,
      { method: "POST", headers: authHeaders(accessToken) },
    );
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (!res.ok) {
    throw new ApiError(
      `POST /credentials/${provider}/authorize a échoué (${res.status})`,
      res.status,
    );
  }
  const v = (await res.json()) as Record<string, unknown>;
  const authorizeUrl =
    typeof v["authorize_url"] === "string" ? v["authorize_url"] : null;
  const state = typeof v["state"] === "string" ? v["state"] : null;
  const codeVerifier =
    typeof v["code_verifier"] === "string" ? v["code_verifier"] : null;
  if (!authorizeUrl || !state || !codeVerifier) {
    throw new ApiError("réponse /authorize mal formée");
  }
  return { provider, authorizeUrl, state, codeVerifier };
}

/** Échange `code` contre des tokens persistés ; renvoie le credential. */
export async function completeAuthorize(
  provider: string,
  body: { code: string; state: string; codeVerifier: string },
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<ConnectedCredential> {
  let res: Response;
  try {
    res = await fetchImpl(
      `${apiBaseUrl()}/credentials/${encodeURIComponent(provider)}/callback`,
      {
        method: "POST",
        headers: { ...authHeaders(accessToken), "Content-Type": "application/json" },
        body: JSON.stringify({
          code: body.code,
          state: body.state,
          code_verifier: body.codeVerifier,
        }),
      },
    );
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  if (res.status === 502) throw new ApiError("le provider OAuth a refusé l'échange", 502);
  if (!res.ok) {
    throw new ApiError(
      `POST /credentials/${provider}/callback a échoué (${res.status})`,
      res.status,
    );
  }
  const v = (await res.json()) as Record<string, unknown>;
  return {
    providerId:
      typeof v["provider_id"] === "string" ? v["provider_id"] : provider,
    hasRefreshToken: v["has_refresh_token"] === true,
    scope: typeof v["scope"] === "string" ? v["scope"] : null,
    expiresAtMs:
      typeof v["expires_at_ms"] === "number" ? v["expires_at_ms"] : null,
  };
}

export async function revokeCredential(
  provider: string,
  accessToken: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  let res: Response;
  try {
    res = await fetchImpl(
      `${apiBaseUrl()}/credentials/${encodeURIComponent(provider)}`,
      { method: "DELETE", headers: authHeaders(accessToken) },
    );
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (res.status === 401) throw new ApiError("session expirée", 401);
  // 204 ou 200 acceptés ; 404 = déjà absent → idempotent.
  if (!res.ok && res.status !== 404) {
    throw new ApiError(
      `DELETE /credentials/${provider} a échoué (${res.status})`,
      res.status,
    );
  }
}

// ─────────────── persistance du challenge PKCE (entre redirections) ───────────────

const CHALLENGE_KEY = "autome.oauth.challenge";

/** Persiste le challenge avant la redirection vers le provider. */
export function saveChallenge(
  challenge: AuthorizeChallenge,
  storage: Storage | null = typeof sessionStorage !== "undefined"
    ? sessionStorage
    : null,
): void {
  storage?.setItem(CHALLENGE_KEY, JSON.stringify(challenge));
}

/** Relit le challenge au retour du provider ; `null` si absent/corrompu. */
export function loadChallenge(
  storage: Storage | null = typeof sessionStorage !== "undefined"
    ? sessionStorage
    : null,
): AuthorizeChallenge | null {
  const raw = storage?.getItem(CHALLENGE_KEY);
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Record<string, unknown>;
    if (
      typeof v["provider"] === "string" &&
      typeof v["authorizeUrl"] === "string" &&
      typeof v["state"] === "string" &&
      typeof v["codeVerifier"] === "string"
    ) {
      return v as unknown as AuthorizeChallenge;
    }
    return null;
  } catch {
    return null;
  }
}

export function clearChallenge(
  storage: Storage | null = typeof sessionStorage !== "undefined"
    ? sessionStorage
    : null,
): void {
  storage?.removeItem(CHALLENGE_KEY);
}

/** Extrait `{ code, state }` d'une query string de callback OAuth. */
export function parseCallbackParams(
  search: string,
): { code: string; state: string } | null {
  const params = new URLSearchParams(
    search.startsWith("?") ? search.slice(1) : search,
  );
  const code = params.get("code");
  const state = params.get("state");
  return code && state ? { code, state } : null;
}
