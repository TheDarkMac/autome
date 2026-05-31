// Intercepteur d'authentification (F8.7.c).
//
// `createAuthFetch` enveloppe un `fetch` standard pour, à chaque requête :
//   1. injecter `Authorization: Bearer <access token>` (si une session existe) ;
//   2. sur une réponse **401**, tenter **un** refresh du token puis **rejouer
//      une seule fois** la requête d'origine avec le nouveau token ;
//   3. si le refresh échoue (ou qu'aucun refresh token n'est disponible),
//      signaler la perte de session (`onUnauthenticated`) et renvoyer le 401
//      d'origine au caller — pas de boucle infinie de retry.
//
// Le refresh est **single-flight** : si plusieurs requêtes prennent un 401 en
// même temps, un seul appel `/auth/refresh` part et toutes attendent son
// résultat. Le caller reste libre d'injecter `fetchImpl`/`refreshImpl` (tests).

import { refresh as apiRefresh, type TokenPair } from "./auth";

/** Dépendances de l'intercepteur — toutes injectables. */
export interface AuthFetchDeps {
  /** Paire de jetons courante (lue à chaque appel), ou `null` si déconnecté. */
  getTokens: () => TokenPair | null;
  /** Appelé avec la nouvelle paire après un refresh réussi (→ persistance). */
  onTokens: (pair: TokenPair) => void;
  /** Appelé quand l'authentification est définitivement perdue. */
  onUnauthenticated: () => void;
  /** Échange du refresh token (injectable pour les tests). */
  refreshImpl?: typeof apiRefresh;
  /** `fetch` sous-jacent (injectable pour les tests). */
  fetchImpl?: typeof fetch;
}

/** Signature compatible `fetch`, enrichie de l'auth. */
export type AuthFetch = (
  input: RequestInfo | URL,
  init?: RequestInit,
) => Promise<Response>;

export function createAuthFetch(deps: AuthFetchDeps): AuthFetch {
  const fetchImpl = deps.fetchImpl ?? fetch;
  const refreshImpl = deps.refreshImpl ?? apiRefresh;

  // Promesse de refresh en cours, partagée par tous les appelants (single-flight).
  let inflight: Promise<TokenPair | null> | null = null;

  function refreshOnce(): Promise<TokenPair | null> {
    if (inflight) return inflight;
    const tokens = deps.getTokens();
    if (!tokens) return Promise.resolve(null);
    inflight = (async () => {
      try {
        const pair = await refreshImpl(tokens.refreshToken, fetchImpl);
        deps.onTokens(pair);
        return pair;
      } catch {
        deps.onUnauthenticated();
        return null;
      } finally {
        inflight = null;
      }
    })();
    return inflight;
  }

  return async (input, init = {}) => {
    const tokens = deps.getTokens();
    const first = await fetchImpl(input, withBearer(init, tokens?.accessToken));
    if (first.status !== 401) return first;

    // 401 → un seul cycle refresh + retry.
    const refreshed = await refreshOnce();
    if (!refreshed) return first; // refresh impossible : on rend le 401 tel quel.
    return fetchImpl(input, withBearer(init, refreshed.accessToken));
  };
}

/**
 * Renvoie une copie de `init` avec l'en-tête `Authorization: Bearer <token>`.
 * Sans token, `init` est renvoyé inchangé (aucun en-tête d'auth ajouté). Les
 * en-têtes déjà présents sont préservés ; un `Authorization` explicite du
 * caller a la priorité.
 */
function withBearer(init: RequestInit, accessToken: string | undefined): RequestInit {
  if (!accessToken) return init;
  const headers = new Headers(init.headers);
  if (!headers.has("Authorization")) {
    headers.set("Authorization", `Bearer ${accessToken}`);
  }
  return { ...init, headers };
}
