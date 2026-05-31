// Hook de session (F8.6.b).
//
// Source de vérité de l'authentification côté SPA :
//   - charge la session depuis le `SessionStore` au montage ;
//   - expose `login` / `signup` / `logout` ;
//   - rafraîchit l'access token de manière proactive (timer réarmé à
//     `skewMs` avant l'expiration) et sur échec, déconnecte proprement.
//
// Les dépendances réseau (`login`/`signup`/`refresh`) et le store sont
// injectables pour les tests — aucun appel direct à `localStorage` ici.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  login as apiLogin,
  refresh as apiRefresh,
  signup as apiSignup,
  type Credentials,
  type TokenPair,
} from "./auth";
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
}

export interface UseSessionResult {
  session: Session | null;
  /** `true` tant que le chargement initial depuis le store n'est pas fait. */
  loading: boolean;
  authenticated: boolean;
  login: (creds: Credentials) => Promise<void>;
  signup: (creds: Credentials) => Promise<void>;
  logout: () => void;
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

  // Chargement initial.
  useEffect(() => {
    const stored = storeRef.current.read();
    setSession(stored ? sessionFromTokens(stored) : null);
    setLoading(false);
  }, []);

  const logout = useCallback(() => apply(null), [apply]);

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
  const refreshRef = useRef(refreshImpl);
  useEffect(() => {
    refreshRef.current = refreshImpl;
  }, [refreshImpl]);

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
  };
}
