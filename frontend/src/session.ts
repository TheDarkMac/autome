// Persistance de session (F8.6.b).
//
// On stocke la paire access/refresh dans un `SessionStore` injectable
// (par défaut `localStorage`). Le refresh token est sensible : le garder
// en `localStorage` est un compromis pragmatique pour une SPA sans cookie
// httpOnly côté backend — à durcir si le backend expose un cookie de
// refresh (TODO F8.7.c). Aucun secret n'est loggé.
//
// La logique de rafraîchissement proactif vit dans `useSession` ; ce
// module se limite au (dé)stockage + dérivation pure d'un `Session`.

import { decodeJwt, tokenExpiresAtMs, type TokenPair } from "./auth";

/** Session courante dérivée de la paire de jetons. */
export interface Session {
  readonly accessToken: string;
  readonly refreshToken: string;
  /** `sub` extrait de l'access token. */
  readonly userId: string;
  /** Expiration de l'access token (ms epoch), ou `null` si indécodable. */
  readonly accessExpiresAtMs: number | null;
}

/** Abstraction de stockage — implémentée par `localStorage` en prod. */
export interface SessionStore {
  read(): TokenPair | null;
  write(pair: TokenPair): void;
  clear(): void;
}

export const SESSION_STORAGE_KEY = "autome.session";

/**
 * Store adossé à un `Storage` web (localStorage par défaut). Tolère
 * l'absence de storage (SSR/tests sans jsdom) et les payloads corrompus
 * en renvoyant `null`.
 */
export function webSessionStore(
  storage: Storage | null = typeof localStorage !== "undefined"
    ? localStorage
    : null,
): SessionStore {
  return {
    read(): TokenPair | null {
      if (!storage) return null;
      const raw = storage.getItem(SESSION_STORAGE_KEY);
      if (!raw) return null;
      try {
        const v = JSON.parse(raw) as Record<string, unknown>;
        const accessToken =
          typeof v["accessToken"] === "string" ? v["accessToken"] : null;
        const refreshToken =
          typeof v["refreshToken"] === "string" ? v["refreshToken"] : null;
        return accessToken && refreshToken
          ? { accessToken, refreshToken }
          : null;
      } catch {
        return null;
      }
    },
    write(pair: TokenPair): void {
      storage?.setItem(SESSION_STORAGE_KEY, JSON.stringify(pair));
    },
    clear(): void {
      storage?.removeItem(SESSION_STORAGE_KEY);
    },
  };
}

/** Store en mémoire — pratique pour les tests. */
export function memorySessionStore(initial: TokenPair | null = null): SessionStore {
  let current = initial;
  return {
    read: () => current,
    write: (pair) => {
      current = pair;
    },
    clear: () => {
      current = null;
    },
  };
}

/**
 * Dérive un `Session` complet d'une paire de jetons. Renvoie `null` si
 * l'access token n'expose pas de `sub` exploitable (jeton corrompu).
 */
export function sessionFromTokens(pair: TokenPair): Session | null {
  const claims = decodeJwt(pair.accessToken);
  if (!claims) return null;
  return {
    accessToken: pair.accessToken,
    refreshToken: pair.refreshToken,
    userId: claims.sub,
    accessExpiresAtMs: tokenExpiresAtMs(pair.accessToken),
  };
}

/**
 * Délai (ms) avant le prochain refresh proactif, en visant `skewMs`
 * avant l'expiration. Bornage : jamais négatif, jamais au-delà de
 * `maxMs` (pour réarmer périodiquement même sur un jeton longue durée
 * ou indécodable).
 */
export function msUntilRefresh(
  session: Session,
  nowMs: number,
  skewMs: number,
  maxMs: number,
): number {
  if (session.accessExpiresAtMs === null) return maxMs;
  const target = session.accessExpiresAtMs - skewMs;
  return Math.max(0, Math.min(maxMs, target - nowMs));
}
