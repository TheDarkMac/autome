// Hook de session (F8.6.b).
//
// Source de vérité de l'authentification côté SPA :
//   - charge la session depuis le `SessionStore` au montage ;
//   - expose `login` / `signup` (sans session si l'adresse doit d'abord
//     être vérifiée — #231) / `logout` (qui ferme aussi la session
//     côté serveur, au mieux — #184), `logoutAll` (toutes les sessions,
//     tous appareils — #186), `changePassword` (#196), `deleteAccount`
//     (#229), `acceptInvitation` (#241, connexion directe), `verifyMfa`
//     (seconde étape de la connexion) et `enableMfa` (#245) ;
//   - rafraîchit l'access token de manière proactive (timer réarmé à
//     `skewMs` avant l'expiration) et sur échec, déconnecte proprement.
//
// Les dépendances réseau (`login`/`signup`/`refresh`) et le store sont
// injectables pour les tests — aucun appel direct à `localStorage` ici.

import { useCallback, useEffect, useRef, useState } from "react";

import {
  acceptInvitation as apiAcceptInvitation,
  changePassword as apiChangePassword,
  deleteAccount as apiDeleteAccount,
  login as apiLogin,
  logout as apiLogout,
  logoutAll as apiLogoutAll,
  refresh as apiRefresh,
  signup as apiSignup,
  type Credentials,
  type TokenPair,
} from "./auth";
import { createAuthFetch, type AuthFetch } from "./authFetch";
import { enableMfa as apiEnableMfa, verifyMfa as apiVerifyMfa } from "./mfa";
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
  changePasswordImpl?: typeof apiChangePassword;
  deleteAccountImpl?: typeof apiDeleteAccount;
  acceptInvitationImpl?: typeof apiAcceptInvitation;
  verifyMfaImpl?: typeof apiVerifyMfa;
  enableMfaImpl?: typeof apiEnableMfa;
}

/** Inscription en attente de la vérification de l'adresse (#231). */
export interface PendingVerification {
  /** Adresse (normalisée par le serveur) à laquelle le lien est parti. */
  readonly email: string;
}

export interface UseSessionResult {
  session: Session | null;
  /** `true` tant que le chargement initial depuis le store n'est pas fait. */
  loading: boolean;
  authenticated: boolean;
  login: (creds: Credentials) => Promise<void>;
  /**
   * Crée le compte et ouvre la session ; renvoie l'adresse à vérifier (sans
   * session) quand le serveur l'exige (#231), `null` sinon.
   */
  signup: (creds: Credentials) => Promise<PendingVerification | null>;
  logout: () => void;
  /**
   * Ferme toutes les sessions de l'utilisateur côté serveur puis la session
   * locale. Rejette (session locale conservée) si le serveur n'a pas
   * confirmé : les autres appareils seraient encore connectés.
   */
  logoutAll: () => Promise<void>;
  /**
   * Change le mot de passe : les autres sessions sont fermées, la session
   * courante continue avec la paire renvoyée. Rejette sans rien changer en
   * local si le serveur refuse.
   */
  changePassword: (currentPassword: string, newPassword: string) => Promise<void>;
  /**
   * Supprime définitivement le compte (#229) puis la session locale. Rejette
   * sans rien changer en local si le serveur refuse.
   */
  deleteAccount: (currentPassword: string) => Promise<void>;
  /**
   * Accepte une invitation (#241) et ouvre la session du compte invité ; une
   * session déjà ouverte dans ce navigateur est d'abord fermée. Rejette sans
   * rien changer en local si le serveur refuse.
   */
  acceptInvitation: (token: string, password: string) => Promise<void>;
  /**
   * Seconde étape de la connexion (#245) : échange le défi reçu (cf.
   * `MfaRequiredError`) et un code contre une session. Rejette sans rien
   * changer en local si le serveur refuse.
   */
  verifyMfa: (mfaToken: string, code: string) => Promise<void>;
  /**
   * Active le second facteur par un premier code (#245) : les autres
   * sessions sont fermées, la courante continue avec la paire renvoyée.
   * Renvoie les codes de secours, à montrer une seule fois.
   */
  enableMfa: (code: string) => Promise<string[]>;
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
    changePasswordImpl = apiChangePassword,
    deleteAccountImpl = apiDeleteAccount,
    acceptInvitationImpl = apiAcceptInvitation,
    verifyMfaImpl = apiVerifyMfa,
    enableMfaImpl = apiEnableMfa,
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

  const changePassword = useCallback(
    async (currentPassword: string, newPassword: string) => {
      apply(await changePasswordImpl(currentPassword, newPassword, authFetch));
    },
    [apply, authFetch, changePasswordImpl],
  );

  const deleteAccount = useCallback(
    async (currentPassword: string) => {
      await deleteAccountImpl(currentPassword, authFetch);
      apply(null);
    },
    [apply, authFetch, deleteAccountImpl],
  );

  const acceptInvitation = useCallback(
    async (token: string, password: string) => {
      const pair = await acceptInvitationImpl(token, password);
      const previous = sessionRef.current;
      if (previous) {
        logoutImpl({
          accessToken: previous.accessToken,
          refreshToken: previous.refreshToken,
        }).catch(() => undefined);
      }
      apply(pair);
    },
    [apply, acceptInvitationImpl, logoutImpl],
  );

  const verifyMfa = useCallback(
    async (mfaToken: string, code: string) => {
      apply(await verifyMfaImpl(mfaToken, code));
    },
    [apply, verifyMfaImpl],
  );

  const enableMfa = useCallback(
    async (code: string) => {
      const enabled = await enableMfaImpl(code, authFetch);
      apply(enabled.tokens);
      return enabled.recoveryCodes;
    },
    [apply, authFetch, enableMfaImpl],
  );

  const login = useCallback(
    async (creds: Credentials) => {
      apply(await loginImpl(creds));
    },
    [apply, loginImpl],
  );

  const signup = useCallback(
    async (creds: Credentials): Promise<PendingVerification | null> => {
      const result = await signupImpl(creds);
      if (result.kind === "verificationRequired") return { email: result.email };
      apply(result.tokens);
      return null;
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
    changePassword,
    deleteAccount,
    acceptInvitation,
    verifyMfa,
    enableMfa,
    authFetch,
  };
}
