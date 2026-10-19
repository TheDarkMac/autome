import { useCallback, useEffect, useMemo, useState } from "react";
import { apiBaseUrl, fetchOpenApi, type OpenApiSpec } from "./api";
import {
  canAdministerUsers,
  createApiClient,
  hasPermission,
  type Schemas,
} from "./apiClient";
import { fetchCatalog, STATIC_CATALOG, type CatalogItem } from "./catalog";
import { AcceptInvitationPage } from "./AcceptInvitationPage";
import { AccountPanel } from "./AccountPanel";
import { AdminPanel } from "./AdminPanel";
import {
  readInviteToken,
  readResetToken,
  readVerifyToken,
  requestPasswordReset as apiRequestPasswordReset,
  resendVerification as apiResendVerification,
  resetPassword as apiResetPassword,
  verifyEmail as apiVerifyEmail,
} from "./auth";
import { ConnectorsPanel } from "./ConnectorsPanel";
import type { CanvasNode, WireGraph } from "./graph";
import { LoginPage } from "./LoginPage";
import {
  disableMfa,
  fetchMfaStatus,
  regenerateRecoveryCodes,
  setupMfa,
} from "./mfa";
import type { MfaActions } from "./MfaSection";
import { NodeCanvas, type NodeCanvasController } from "./NodeCanvas";
import { NodePanel, type WebhookActions } from "./NodePanel";
import { Palette } from "./Palette";
import { ResetPasswordPage } from "./ResetPasswordPage";
import type { ParamValues } from "./paramSchema";
import { RunsList } from "./RunsList";
import { RunExecutionPanel } from "./RunExecutionPanel";
import {
  nodeExecutionStatus,
  type NodeExecStatus,
  type RunCheckpoint,
} from "./runs";
import { useRunPolling } from "./useRunPolling";
import { useSession, type UseSessionOptions } from "./useSession";
import { VerifyEmailPage } from "./VerifyEmailPage";
import {
  lastWorkflowKey,
  useWorkflowEditor,
  type UseWorkflowEditorOptions,
} from "./useWorkflowEditor";
import { WorkflowBar } from "./WorkflowBar";

type Status =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; spec: OpenApiSpec }
  | { kind: "error"; message: string };

type View = "edit" | "runs" | "connectors" | "account" | "admin";

export interface AppProps {
  /** Options de session (store/impls injectables pour les tests). */
  sessionOptions?: UseSessionOptions;
  /** Demande de lien de réinitialisation (#204), injectable pour les tests. */
  requestPasswordResetImpl?: typeof apiRequestPasswordReset;
  /** Réinitialisation par jeton (#204), injectable pour les tests. */
  resetPasswordImpl?: typeof apiResetPassword;
  /** Vérification de l'adresse par jeton (#231), injectable pour les tests. */
  verifyEmailImpl?: typeof apiVerifyEmail;
  /** Renvoi du lien de vérification (#231), injectable pour les tests. */
  resendVerificationImpl?: typeof apiResendVerification;
  /** Options du workflow ouvert (#225), injectables pour les tests. */
  editorOptions?: UseWorkflowEditorOptions;
}

export function App({
  sessionOptions,
  requestPasswordResetImpl = apiRequestPasswordReset,
  resetPasswordImpl = apiResetPassword,
  verifyEmailImpl = apiVerifyEmail,
  resendVerificationImpl = apiResendVerification,
  editorOptions,
}: AppProps = {}): JSX.Element {
  const {
    session,
    loading: sessionLoading,
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
  } = useSession(sessionOptions);
  const [logoutAllError, setLogoutAllError] = useState<string | null>(null);

  // Liens reçus par email — réinitialisation (#204), vérification de
  // l'adresse (#231), invitation (#241) : jeton lu une fois dans le fragment,
  // puis retiré de la barre d'adresse (et de l'historique).
  const [resetToken, setResetToken] = useState<string | null>(() =>
    readResetToken(window.location.hash),
  );
  const [verifyToken, setVerifyToken] = useState<string | null>(() =>
    readVerifyToken(window.location.hash),
  );
  const [inviteToken, setInviteToken] = useState<string | null>(() =>
    readInviteToken(window.location.hash),
  );
  const [loginNotice, setLoginNotice] = useState<string | null>(null);
  // Lien collé dans un onglet où l'app est déjà ouverte : seul le fragment
  // change, sans rechargement.
  useEffect(() => {
    const onHashChange = (): void => {
      const reset = readResetToken(window.location.hash);
      if (reset) setResetToken(reset);
      const verify = readVerifyToken(window.location.hash);
      if (verify) setVerifyToken(verify);
      const invite = readInviteToken(window.location.hash);
      if (invite) setInviteToken(invite);
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);
  useEffect(() => {
    if (resetToken || verifyToken || inviteToken) {
      window.history.replaceState(
        null,
        "",
        window.location.pathname + window.location.search,
      );
    }
  }, [resetToken, verifyToken, inviteToken]);
  const onVerifyDone = useCallback(() => {
    setVerifyToken(null);
    setLoginNotice("Adresse email vérifiée : vous pouvez vous connecter.");
  }, []);
  const onResetDone = useCallback(() => {
    setResetToken(null);
    setLoginNotice(
      "Mot de passe réinitialisé : connectez-vous avec le nouveau mot de passe.",
    );
    // Le serveur a fermé toutes les sessions, celle de cet appareil comprise.
    if (session) logout();
  }, [session, logout]);

  // Suppression du compte (#229) : `deleteAccount` ferme la session locale ;
  // le dernier workflow retenu pour ce compte dans ce navigateur est oublié.
  const onDeleteAccount = useCallback(
    async (currentPassword: string) => {
      const userId = session?.userId;
      await deleteAccount(currentPassword);
      if (userId) {
        try {
          window.localStorage.removeItem(lastWorkflowKey(userId));
        } catch {
          // Stockage indisponible : rien à oublier.
        }
      }
      setLoginNotice("Votre compte et vos données ont été supprimés.");
    },
    [deleteAccount, session],
  );

  // Ferme la session sur tous les appareils (#186) — action destructrice,
  // confirmée ; en cas d'échec, la session locale reste ouverte et l'erreur
  // est affichée (les autres appareils seraient encore connectés).
  const onLogoutAll = useCallback(async () => {
    if (!window.confirm("Fermer la session sur tous vos appareils ?")) return;
    setLogoutAllError(null);
    try {
      await logoutAll();
    } catch (e) {
      setLogoutAllError(
        `Échec de la déconnexion des appareils : ${(e as Error).message}`,
      );
    }
  }, [logoutAll]);

  // Client API typé adossé à l'`authFetch` (F8.7) — identité stable car
  // `authFetch` l'est ; les vues de runs y branchent leurs lectures.
  const api = useMemo(() => createApiClient(authFetch), [authFetch]);
  // Double authentification (#245) : l'activation renouvelle la session.
  const mfaActions = useMemo<MfaActions>(
    () => ({
      status: () => fetchMfaStatus(authFetch),
      setup: (password) => setupMfa(password, authFetch),
      enable: enableMfa,
      disable: (password, code) => disableMfa(password, code, authFetch),
      regenerate: (code) => regenerateRecoveryCodes(code, authFetch),
    }),
    [authFetch, enableMfa],
  );
  // Identité et droits de l'utilisateur (#208) : l'onglet Administration
  // n'apparaît qu'avec `users.admin`. Un échec laisse simplement l'onglet
  // masqué (le serveur refuserait de toute façon).
  const userId = session?.userId ?? null;
  const [me, setMe] = useState<Schemas["MeResponse"] | null>(null);
  useEffect(() => {
    setMe(null);
    if (!userId) return;
    let cancelled = false;
    api.me().then(
      (m) => {
        if (!cancelled && Array.isArray(m?.permissions)) setMe(m);
      },
      () => undefined,
    );
    return () => {
      cancelled = true;
    };
  }, [api, userId]);
  const isAdmin = me !== null && canAdministerUsers(me.permissions);
  // Droits d'édition et de lancement (#225) : tant que `me` n'est pas
  // chargé, on n'interdit rien (le serveur tranche).
  const canWrite = me === null || hasPermission(me.permissions, "workflows.write");
  const canRun = me === null || hasPermission(me.permissions, "runs.execute");
  // Workflow ouvert dans l'éditeur, enregistré côté serveur (#225).
  const editor = useWorkflowEditor(api, userId, editorOptions);
  const workflowId = editor.current.id;
  const persisted = editor.current.persisted;
  // URL des nœuds webhook du workflow courant (#202) — relues après chaque
  // enregistrement ; aucun appel tant que le serveur ne connaît pas l'id.
  const revision = editor.revision;
  const webhooks = useMemo<WebhookActions>(
    () => ({
      list: () =>
        persisted
          ? api.listWebhooks(workflowId)
          : Promise.resolve({ items: [] }),
      rotate: (nodeId) => api.rotateWebhook(workflowId, nodeId),
    }),
    // `revision` force une relecture des URL après chaque enregistrement.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [api, workflowId, persisted, revision],
  );
  const [status, setStatus] = useState<Status>({ kind: "idle" });
  const [catalog, setCatalog] = useState<readonly CatalogItem[]>(STATIC_CATALOG);
  const [catalogLoading, setCatalogLoading] = useState(true);
  const [selectedNode, setSelectedNode] = useState<CanvasNode | null>(null);
  const [controller, setController] = useState<NodeCanvasController | null>(null);
  const [view, setView] = useState<View>("edit");
  const graph = editor.graph;
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);
  // Autre workflow ouvert : sa sélection et son run ne valent plus.
  useEffect(() => {
    setSelectedNode(null);
    setSelectedRunId(null);
  }, [editor.canvas.key]);
  const onRunStarted = useCallback((runId: string) => {
    setSelectedRunId(runId);
    setView("runs");
  }, []);
  // Droits retirés (ou autre compte) pendant que l'onglet est ouvert.
  useEffect(() => {
    if (view === "admin" && !isAdmin) setView("edit");
  }, [view, isAdmin]);

  // Polling du run sélectionné (F8.5.d). Inactif quand on est en mode édition.
  const { checkpoint, error: pollError } = useRunPolling(
    view === "runs" ? selectedRunId : null,
    { fetchImpl: api.getRun },
  );

  useEffect(() => {
    let cancelled = false;
    setStatus({ kind: "loading" });
    fetchOpenApi()
      .then((spec) => {
        if (!cancelled) setStatus({ kind: "ok", spec });
      })
      .catch((e: Error) => {
        if (!cancelled) setStatus({ kind: "error", message: e.message });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setCatalogLoading(true);
    fetchCatalog().then((items) => {
      if (cancelled) return;
      setCatalog(items);
      setCatalogLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const handlePanelChange = useCallback(
    (nodeId: string, params: ParamValues) => {
      controller?.patchNodeParams(nodeId, params);
      setSelectedNode((prev) =>
        prev && prev.id === nodeId
          ? { ...prev, data: { ...prev.data, params } }
          : prev,
      );
    },
    [controller],
  );

  const executionStatuses = useMemo(
    () =>
      view === "runs" && checkpoint
        ? computeStatuses(checkpoint, graph)
        : undefined,
    [view, checkpoint, graph],
  );

  // Garde d'authentification (F8.6.b) — on attend le chargement du store
  // avant de décider, pour éviter un flash de la page de login.
  if (sessionLoading) {
    return (
      <main className="flex h-screen items-center justify-center bg-neutral-50">
        <p role="status" className="text-sm text-neutral-500">
          Chargement…
        </p>
      </main>
    );
  }
  if (resetToken) {
    return (
      <ResetPasswordPage
        token={resetToken}
        resetPassword={(token, next) => resetPasswordImpl(token, next)}
        onDone={onResetDone}
        onCancel={() => setResetToken(null)}
      />
    );
  }
  if (inviteToken) {
    return (
      <AcceptInvitationPage
        token={inviteToken}
        acceptInvitation={acceptInvitation}
        onDone={() => setInviteToken(null)}
        onCancel={() => setInviteToken(null)}
      />
    );
  }
  if (verifyToken) {
    return (
      <VerifyEmailPage
        token={verifyToken}
        verifyEmail={verifyEmailImpl}
        onDone={onVerifyDone}
        onCancel={() => setVerifyToken(null)}
      />
    );
  }
  if (!session) {
    return (
      <LoginPage
        login={login}
        signup={signup}
        verifyMfa={verifyMfa}
        resendVerification={(email) => resendVerificationImpl(email)}
        requestPasswordReset={(email) => requestPasswordResetImpl(email)}
        notice={loginNotice}
      />
    );
  }

  return (
    <main className="flex h-screen flex-col bg-neutral-50">
      <header className="flex items-start justify-between border-b border-neutral-200 bg-white px-6 py-4 sm:px-10">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Autome</h1>
          <p className="mt-1 text-sm text-neutral-500">
            Éditeur visuel de workflows — F8.6 authentification & connecteurs.
          </p>
        </div>
        <div className="flex items-center gap-3 text-sm">
          <span className="text-neutral-500" data-testid="current-user">
            {me?.email ?? session.userId}
          </span>
          <button
            type="button"
            onClick={logout}
            className="rounded-md border border-neutral-200 px-3 py-1.5 text-neutral-700 hover:bg-neutral-50"
          >
            Se déconnecter
          </button>
          <button
            type="button"
            onClick={() => void onLogoutAll()}
            className="rounded-md border border-neutral-200 px-3 py-1.5 text-neutral-700 hover:bg-neutral-50"
          >
            Déconnecter tous les appareils
          </button>
          {logoutAllError && (
            <span className="text-rose-700" role="alert">
              {logoutAllError}
            </span>
          )}
        </div>
      </header>

      <section
        aria-labelledby="backend-status"
        className="border-b border-neutral-200 bg-white px-6 py-3 text-sm sm:px-10"
      >
        <span id="backend-status" className="font-medium">
          Backend
        </span>{" "}
        <span className="text-neutral-500">
          ({apiBaseUrl()}/openapi.json)
        </span>{" "}
        <BackendStatus status={status} />
      </section>

      <ViewTabs view={view} onChange={setView} showAdmin={isAdmin} />

      {view === "admin" && me && isAdmin ? (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <AdminPanel
            api={api}
            currentUserId={me.id}
            superAdmin={me.permissions.includes("*")}
          />
        </div>
      ) : view === "account" ? (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <AccountPanel
            {...(me ? { email: me.email } : {})}
            changePassword={changePassword}
            deleteAccount={onDeleteAccount}
            mfa={mfaActions}
          />
        </div>
      ) : view === "connectors" ? (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <ConnectorsPanel accessToken={session.accessToken} />
        </div>
      ) : (
        <>
          <WorkflowBar
            editor={editor}
            canWrite={canWrite}
            canRun={canRun}
            onRunStarted={onRunStarted}
          />
          <div className="flex min-h-0 flex-1">
            {view === "edit" ? (
              <Palette items={catalog} loading={catalogLoading} />
            ) : persisted ? (
              <RunsList
                workflowId={workflowId}
                selectedRunId={selectedRunId}
                onSelectRun={setSelectedRunId}
                refreshToken={checkpoint?.status}
                fetchImpl={api.listRuns}
              />
            ) : (
              <aside
                aria-label="Liste des runs"
                className="w-80 shrink-0 border-r border-neutral-200 bg-white px-4 py-3 text-sm text-neutral-500"
              >
                Enregistrez le workflow pour voir ses exécutions.
              </aside>
            )}
            <section className="flex min-h-0 flex-1">
              <NodeCanvas
                key={editor.canvas.key}
                initialNodes={editor.canvas.nodes}
                initialEdges={editor.canvas.edges}
                onChange={editor.onGraphChange}
                onSelectionChange={setSelectedNode}
                onReady={setController}
                executionStatuses={executionStatuses}
                workflowId={workflowId}
                workflowName={editor.current.name}
              />
            </section>
            {view === "edit" ? (
              <NodePanel
                node={selectedNode}
                onChange={handlePanelChange}
                onClose={() => setSelectedNode(null)}
                webhooks={webhooks}
              />
            ) : (
              <RunExecutionPanel
                checkpoint={pollError ? null : checkpoint}
                node={selectedNode}
                edges={graph?.edges ?? []}
                onClose={() => setSelectedNode(null)}
              />
            )}
          </div>
        </>
      )}
    </main>
  );
}

function computeStatuses(
  ck: RunCheckpoint,
  graph: WireGraph | null,
): Readonly<Record<string, NodeExecStatus>> {
  const ids = new Set<string>(graph?.nodes.map((n) => n.id) ?? []);
  for (const id of Object.keys(ck.outputs)) ids.add(id);
  if (ck.next_node) ids.add(ck.next_node);
  const out: Record<string, NodeExecStatus> = {};
  for (const id of ids) out[id] = nodeExecutionStatus(ck, id);
  return out;
}

function ViewTabs({
  view,
  onChange,
  showAdmin,
}: {
  view: View;
  onChange: (v: View) => void;
  showAdmin: boolean;
}): JSX.Element {
  return (
    <nav
      role="tablist"
      aria-label="Vues"
      className="flex gap-1 border-b border-neutral-200 bg-white px-6 sm:px-10"
    >
      <ViewTab
        active={view === "edit"}
        label="Éditeur"
        onClick={() => onChange("edit")}
      />
      <ViewTab
        active={view === "runs"}
        label="Exécutions"
        onClick={() => onChange("runs")}
      />
      <ViewTab
        active={view === "connectors"}
        label="Connecteurs"
        onClick={() => onChange("connectors")}
      />
      <ViewTab
        active={view === "account"}
        label="Compte"
        onClick={() => onChange("account")}
      />
      {showAdmin && (
        <ViewTab
          active={view === "admin"}
          label="Administration"
          onClick={() => onChange("admin")}
        />
      )}
    </nav>
  );
}

function ViewTab({
  active,
  label,
  onClick,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
}): JSX.Element {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className={`-mb-px border-b-2 px-3 py-2 text-sm transition ${
        active
          ? "border-sky-500 text-sky-700"
          : "border-transparent text-neutral-500 hover:text-neutral-700"
      }`}
    >
      {label}
    </button>
  );
}

function BackendStatus({ status }: { status: Status }): JSX.Element {
  switch (status.kind) {
    case "idle":
    case "loading":
      return (
        <span className="text-neutral-700" role="status">
          chargement…
        </span>
      );
    case "ok": {
      const paths = Object.keys(status.spec.paths).length;
      return (
        <span className="text-emerald-700" role="status">
          OK · {status.spec.info.title} v{status.spec.info.version} ({paths}{" "}
          endpoint{paths > 1 ? "s" : ""})
        </span>
      );
    }
    case "error":
      return (
        <span className="text-rose-700" role="alert">
          erreur : {status.message}
        </span>
      );
  }
}
