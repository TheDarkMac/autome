// Hook de session (F8.6.b).
//
// Source de vérité de l'authentification côté SPA :
//   - charge la session depuis le `SessionStore` au montage ;
//   - expose `login` / `signup` / `logout` (qui ferme aussi la session
//     côté serveur, au mieux — #184) et `logoutAll` (toutes les sessions,
//     tous appareils — #186) ;
//   - rafraîchit l'access token de manière proactive (timer réarmé à
//     `skewMs` avant l'expiration) et sur échec, déconnecte proprement.
//
// Les dépendances réseau (`login`/`signup`/`refresh`) et le store sont
// injectables pour les tests — aucun appel direct à `localStorage` ici.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  login as apiLogin,
  logout as apiLogout,
  logoutAll as apiLogoutAll,
  refresh as apiRefresh,
  signup as apiSignup,
  type Credentials,
  type TokenPair,
} from "./auth";
import { createAuthFetch, type AuthFetch } from "./authFetch";
import {
  msUntilRefresh,
  sessionFromTokens,
  webSessionStore,
  type Session,
  type SessionStore,
} from "./session";

export interface UseSessionOptions {
  store?: SessionStore;
  /** Refresh proactif `skewMs` avant l'expiration de l'access token. */
  skewMs?: number;
  /** Borne haute du timer de refresh (réarmement périodique). */
  maxRefreshDelayMs?: number;
  loginImpl?: typeof apiLogin;
  signupImpl?: typeof apiSignup;
  refreshImpl?: typeof apiRefresh;
  logoutImpl?: typeof apiLogout;
  logoutAllImpl?: typeof apiLogoutAll;
}

export interface UseSessionResult {
  session: Session | null;
  /** `true` tant que le chargement initial depuis le store n'est pas fait. */
  loading: boolean;
  authenticated: boolean;
  login: (creds: Credentials) => Promise<void>;
  signup: (creds: Credentials) => Promise<void>;
  logout: () => void;
  /**
   * Ferme toutes les sessions de l'utilisateur côté serveur puis la session
   * locale. Rejette (session locale conservée) si le serveur n'a pas
   * confirmé : les autres appareils seraient encore connectés.
   */
  logoutAll: () => Promise<void>;
  /**
   * `fetch` authentifié (F8.7.c) : injecte le Bearer courant et, sur 401,
   * rafraîchit le token puis rejoue la requête. Identité **stable** — sûr à
   * passer à `useMemo`/`useEffect` ou à `createApiClient`.
   */
  authFetch: AuthFetch;
}

const DEFAULT_SKEW_MS = 60_000; // refresh 1 min avant l'exp
const DEFAULT_MAX_DELAY_MS = 5 * 60_000; // réarme au moins toutes les 5 min

export function useSession(options: UseSessionOptions = {}): UseSessionResult {
  const {
    skewMs = DEFAULT_SKEW_MS,
    maxRefreshDelayMs = DEFAULT_MAX_DELAY_MS,
    loginImpl = apiLogin,
    signupImpl = apiSignup,
    refreshImpl = apiRefresh,
    logoutImpl = apiLogout,
    logoutAllImpl = apiLogoutAll,
  } = options;

  // Le store ne doit pas changer entre les rendus : on le fige au montage.
  const storeRef = useRef<SessionStore>(options.store ?? webSessionStore());

  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);

  const apply = useCallback((pair: TokenPair | null) => {
    const next = pair ? sessionFromTokens(pair) : null;
    if (pair && next) {
      storeRef.current.write(pair);
    } else {
      storeRef.current.clear();
    }
    setSession(next);
  }, []);

  // Réf. synchrone vers la session/refresh courants — lues par `authFetch`
  // et par l'effet de refresh proactif sans relancer leurs closures.
  const sessionRef = useRef(session);
  sessionRef.current = session;
  const refreshRef = useRef(refreshImpl);
  useEffect(() => {
    refreshRef.current = refreshImpl;
  }, [refreshImpl]);

  // `authFetch` (F8.7.c) — construit une seule fois pour une identité stable.
  const authFetchRef = useRef<AuthFetch | null>(null);
  if (authFetchRef.current === null) {
    authFetchRef.current = createAuthFetch({
      getTokens: () => {
        const s = sessionRef.current;
        return s
          ? { accessToken: s.accessToken, refreshToken: s.refreshToken }
          : null;
      },
      onTokens: (pair) => apply(pair),
      onUnauthenticated: () => apply(null),
      refreshImpl: (rt, fi) => refreshRef.current(rt, fi),
    });
  }
  const authFetch = authFetchRef.current;

  // Chargement initial.
  useEffect(() => {
    const stored = storeRef.current.read();
    setSession(stored ? sessionFromTokens(stored) : null);
    setLoading(false);
  }, []);

  // La session locale est vidée tout de suite (l'UI n'attend pas le
  // réseau) ; la révocation serveur part en arrière-plan. Si elle échoue
  // (backend injoignable), les jetons expirent d'eux-mêmes : access 15 min,
  // refresh 7 j — rien de plus à faire côté client.
  const logout = useCallback(() => {
    const current = sessionRef.current;
    apply(null);
    if (current) {
      logoutImpl({
        accessToken: current.accessToken,
        refreshToken: current.refreshToken,
      }).catch(() => undefined);
    }
  }, [apply, logoutImpl]);

  const logoutAll = useCallback(async () => {
    await logoutAllImpl(authFetch);
    apply(null);
  }, [apply, authFetch, logoutAllImpl]);

  const login = useCallback(
    async (creds: Credentials) => {
      apply(await loginImpl(creds));
    },
    [apply, loginImpl],
  );

  const signup = useCallback(
    async (creds: Credentials) => {
      apply(await signupImpl(creds));
    },
    [apply, signupImpl],
  );

  // Refresh proactif. Re-planifié à chaque changement de session.
  useEffect(() => {
    if (!session) return;
    let cancelled = false;
    const delay = msUntilRefresh(session, Date.now(), skewMs, maxRefreshDelayMs);
    const timer = setTimeout(async () => {
      try {
        const pair = await refreshRef.current(session.refreshToken);
        if (!cancelled) apply(pair);
      } catch {
        // Refresh impossible (token révoqué/expiré) → déconnexion propre.
        if (!cancelled) apply(null);
      }
    }, delay);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [session, skewMs, maxRefreshDelayMs, apply]);

  return {
    session,
    loading,
    authenticated: session !== null,
    login,
    signup,
    logout,
    logoutAll,
    authFetch,
  };
}
