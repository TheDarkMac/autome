// Écran d'autorisation OAuth des connecteurs (F8.6.c).
//
// Liste les providers déclarés + leur état de connexion pour l'utilisateur
// courant, et pilote le flow PKCE :
//   - « Connecter » → /authorize → persiste le challenge → redirige ;
//   - au retour (`?code&state`) → /callback → marque connecté ;
//   - « Déconnecter » → DELETE.
//
// Les dépendances effets de bord (fetch, redirection, location, storage)
// sont injectables pour les tests.

import { useCallback, useEffect, useState } from "react";

import {
  clearChallenge,
  completeAuthorize,
  fetchConnectedCredentials,
  fetchProviders,
  loadChallenge,
  parseCallbackParams,
  revokeCredential,
  saveChallenge,
  startAuthorize,
  type ConnectedCredential,
  type Provider,
} from "./connectors";

export interface ConnectorsPanelProps {
  accessToken: string;
  fetchImpl?: typeof fetch;
  /** Redirection navigateur vers le provider. Défaut : `location.assign`. */
  redirect?: (url: string) => void;
  /** Query string de callback à inspecter au montage. Défaut : `location.search`. */
  callbackSearch?: string;
  /** Storage du challenge PKCE. Défaut : `sessionStorage`. */
  storage?: Storage;
  /** Appelé après traitement du callback (pour nettoyer l'URL). */
  onCallbackHandled?: () => void;
}

type Load =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ok"; providers: Provider[]; connected: Map<string, ConnectedCredential> };

export function ConnectorsPanel({
  accessToken,
  fetchImpl = fetch,
  redirect = (url) => window.location.assign(url),
  callbackSearch = typeof window !== "undefined" ? window.location.search : "",
  storage = typeof sessionStorage !== "undefined" ? sessionStorage : undefined,
  onCallbackHandled,
}: ConnectorsPanelProps): JSX.Element {
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setLoad({ kind: "loading" });
    try {
      const [providers, connected] = await Promise.all([
        fetchProviders(accessToken, fetchImpl),
        fetchConnectedCredentials(accessToken, fetchImpl),
      ]);
      const map = new Map(connected.map((c) => [c.providerId, c]));
      setLoad({ kind: "ok", providers, connected: map });
    } catch (e) {
      setLoad({ kind: "error", message: (e as Error).message });
    }
  }, [accessToken, fetchImpl]);

  // Chargement initial + traitement éventuel du callback OAuth.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const cb = parseCallbackParams(callbackSearch);
      const challenge = loadChallenge(storage ?? null);
      if (cb && challenge && cb.state === challenge.state) {
        try {
          await completeAuthorize(
            challenge.provider,
            { code: cb.code, state: cb.state, codeVerifier: challenge.codeVerifier },
            accessToken,
            fetchImpl,
          );
          if (!cancelled) setNotice(`${challenge.provider} connecté.`);
        } catch (e) {
          if (!cancelled) setNotice(`Échec de la connexion : ${(e as Error).message}`);
        } finally {
          clearChallenge(storage ?? null);
          onCallbackHandled?.();
        }
      }
      if (!cancelled) await reload();
    })();
    return () => {
      cancelled = true;
    };
    // Volontairement une seule fois au montage (callback = événement ponctuel).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const connect = useCallback(
    async (provider: string) => {
      setBusy(provider);
      setNotice(null);
      try {
        const challenge = await startAuthorize(provider, accessToken, fetchImpl);
        saveChallenge(challenge, storage ?? null);
        redirect(challenge.authorizeUrl);
      } catch (e) {
        setNotice(`Impossible de démarrer la connexion : ${(e as Error).message}`);
        setBusy(null);
      }
    },
    [accessToken, fetchImpl, redirect, storage],
  );

  const disconnect = useCallback(
    async (provider: string) => {
      setBusy(provider);
      setNotice(null);
      try {
        await revokeCredential(provider, accessToken, fetchImpl);
        await reload();
        setNotice(`${provider} déconnecté.`);
      } catch (e) {
        setNotice(`Échec de la déconnexion : ${(e as Error).message}`);
      } finally {
        setBusy(null);
      }
    },
    [accessToken, fetchImpl, reload],
  );

  return (
    <section
      aria-label="Connecteurs"
      data-testid="connectors-panel"
      className="mx-auto w-full max-w-2xl p-6"
    >
      <header className="mb-4">
        <h2 className="text-lg font-semibold text-neutral-900">Connecteurs</h2>
        <p className="mt-1 text-sm text-neutral-500">
          Autorisez Autome à agir en votre nom sur les services tiers.
        </p>
      </header>

      {notice && (
        <p
          role="status"
          className="mb-4 rounded-md bg-sky-50 px-3 py-2 text-sm text-sky-800"
        >
          {notice}
        </p>
      )}

      <ConnectorsBody
        load={load}
        busy={busy}
        onConnect={connect}
        onDisconnect={disconnect}
      />
    </section>
  );
}

function ConnectorsBody({
  load,
  busy,
  onConnect,
  onDisconnect,
}: {
  load: Load;
  busy: string | null;
  onConnect: (p: string) => void;
  onDisconnect: (p: string) => void;
}): JSX.Element {
  if (load.kind === "loading") {
    return (
      <p role="status" className="text-sm text-neutral-500">
        Chargement des connecteurs…
      </p>
    );
  }
  if (load.kind === "error") {
    return (
      <p role="alert" className="text-sm text-rose-700">
        Erreur : {load.message}
      </p>
    );
  }
  if (load.providers.length === 0) {
    return (
      <p role="status" className="text-sm text-neutral-500">
        Aucun connecteur disponible.
      </p>
    );
  }
  return (
    <ul className="divide-y divide-neutral-100 rounded-lg border border-neutral-200">
      {load.providers.map((p) => (
        <ProviderRow
          key={p.providerId}
          provider={p}
          credential={load.connected.get(p.providerId) ?? null}
          busy={busy === p.providerId}
          onConnect={() => onConnect(p.providerId)}
          onDisconnect={() => onDisconnect(p.providerId)}
        />
      ))}
    </ul>
  );
}

function ProviderRow({
  provider,
  credential,
  busy,
  onConnect,
  onDisconnect,
}: {
  provider: Provider;
  credential: ConnectedCredential | null;
  busy: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
}): JSX.Element {
  const connected = credential !== null;
  return (
    <li
      data-testid={`connector-${provider.providerId}`}
      className="flex items-center justify-between gap-4 px-4 py-3"
    >
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="font-medium capitalize text-neutral-800">
            {provider.providerId}
          </span>
          {connected && (
            <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-[10px] font-medium text-emerald-800">
              connecté
            </span>
          )}
        </div>
        <p className="mt-0.5 truncate text-xs text-neutral-500">
          {provider.scopes.length > 0
            ? provider.scopes.join(", ")
            : "aucun scope déclaré"}
        </p>
      </div>
      {connected ? (
        <button
          type="button"
          disabled={busy}
          onClick={onDisconnect}
          className="shrink-0 rounded-md border border-neutral-200 px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50 disabled:opacity-50"
        >
          {busy ? "…" : "Déconnecter"}
        </button>
      ) : (
        <button
          type="button"
          disabled={busy}
          onClick={onConnect}
          className="shrink-0 rounded-md bg-neutral-900 px-3 py-1.5 text-sm font-medium text-white hover:bg-neutral-700 disabled:opacity-50"
        >
          {busy ? "…" : "Connecter"}
        </button>
      )}
    </li>
  );
}
