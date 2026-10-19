// Onglet « Administration » (#208) : comptes, rôles, suspension, sessions.
//
// Visible des seuls détenteurs de `users.admin` (ou `*`). Le serveur reste
// seul juge : le panneau grise ce qu'il refuserait (son propre compte, le
// rôle `admin` et les super-admins sans `*`) et traduit ses refus en clair.
// Suspendre un compte ou fermer ses sessions est confirmé, comme la
// déconnexion de tous ses appareils ; la suppression définitive d'un compte
// et de ses données (#229) aussi, ainsi que la vérification d'une adresse à
// la place de son titulaire (#231). Un administrateur invite aussi de
// nouvelles personnes (#241) : le compte apparaît « invitation en attente »
// jusqu'à ce qu'elle choisisse son mot de passe ; l'invitation peut être
// renvoyée ou annulée (suppression du compte). Un super-admin voit en plus
// les workflows de tous les comptes (#243), filtrables par propriétaire depuis
// chaque ligne. Le client API est injecté pour les tests.

import { useCallback, useEffect, useState, type FormEvent } from "react";

import { AdminWorkflows, type OwnerFilter } from "./AdminWorkflows";
import { ApiError } from "./api";
import type { ApiClient, AssignableRole, Schemas } from "./apiClient";
import { Pager } from "./Pager";

type AdminUser = Schemas["AdminUser"];

export type AdminApi = Pick<
  ApiClient,
  | "listUsers"
  | "setUserRole"
  | "disableUser"
  | "enableUser"
  | "logoutUser"
  | "deleteUser"
  | "verifyUserEmail"
  | "inviteUser"
  | "resendInvitation"
  | "resetUserMfa"
  | "listAllWorkflows"
>;

export interface AdminPanelProps {
  api: AdminApi;
  /** Identifiant de l'administrateur connecté (actions sur soi grisées). */
  currentUserId: string;
  /**
   * L'administrateur détient `*` : rôle `admin` et super-admins gérables,
   * workflows de tous les comptes visibles (#243).
   */
  superAdmin: boolean;
  pageSize?: number;
  /** Confirmation des actions destructrices. Défaut : `window.confirm`. */
  confirm?: (message: string) => boolean;
}

const ROLE_LABELS: Record<string, string> = {
  admin: "Administrateur",
  editor: "Éditeur",
  viewer: "Lecteur",
  custom: "Personnalisé",
};

const ASSIGNABLE: readonly AssignableRole[] = ["admin", "editor", "viewer"];

type Load =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ok"; items: AdminUser[]; total: number };

/** Message lisible d'un refus d'invitation (#241). */
function describeInvite(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.status) {
      case 400:
        return "email invalide";
      case 403:
        return "seul un super-administrateur peut inviter un administrateur";
      case 409:
        return "un compte existe déjà pour cet email";
      case 503:
        return "invitations indisponibles : aucun envoi d'email n'est configuré sur le serveur";
    }
  }
  return (e as Error).message;
}

/** Message lisible d'un refus de renvoi d'invitation (#241). */
function describeResend(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.status) {
      case 409:
        return "compte déjà activé ou suspendu : rien à renvoyer";
      case 502:
        return "l'email n'a pas pu être envoyé (l'ancien lien n'est plus valable) : réessayez";
      case 503:
        return "invitations indisponibles : aucun envoi d'email n'est configuré sur le serveur";
    }
  }
  return describe(e);
}

/** Message lisible d'un refus du serveur. */
function describe(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.status) {
      case 403:
        return "action réservée aux super-administrateurs";
      case 404:
        return "compte introuvable";
      case 409:
        return "action impossible sur votre propre compte";
    }
  }
  return (e as Error).message;
}

export function AdminPanel({
  api,
  currentUserId,
  superAdmin,
  pageSize = 50,
  confirm = (message) => window.confirm(message),
}: AdminPanelProps): JSX.Element {
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0);
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ kind: "ok" | "error"; text: string } | null>(
    null,
  );
  const [workflowOwner, setWorkflowOwner] = useState<OwnerFilter | null>(null);

  /** Filtre la liste des workflows sur `user` et la fait défiler à l'écran. */
  function showWorkflowsOf(user: AdminUser): void {
    setWorkflowOwner({ id: user.id, email: user.email });
    document.getElementById("admin-workflows")?.scrollIntoView?.({ behavior: "smooth" });
  }

  const reload = useCallback(async () => {
    setLoad({ kind: "loading" });
    try {
      const page = await api.listUsers({ q: query, offset, limit: pageSize });
      setLoad({ kind: "ok", items: page.items, total: page.total });
    } catch (e) {
      setLoad({ kind: "error", message: describe(e) });
    }
  }, [api, query, offset, pageSize]);

  useEffect(() => {
    void reload();
  }, [reload]);

  function onSearch(e: FormEvent<HTMLFormElement>): void {
    e.preventDefault();
    setOffset(0);
    setQuery(search.trim());
  }

  /** Exécute une action sur `user` ; la ligne est remplacée par la réponse. */
  async function runAction(
    user: AdminUser,
    run: () => Promise<AdminUser | void>,
    success: string,
    explain: (e: unknown) => string = describe,
  ): Promise<void> {
    setBusy(user.id);
    setNotice(null);
    try {
      const updated = await run();
      if (updated) {
        setLoad((l) =>
          l.kind === "ok"
            ? { ...l, items: l.items.map((u) => (u.id === updated.id ? updated : u)) }
            : l,
        );
      }
      setNotice({ kind: "ok", text: success });
    } catch (e) {
      setNotice({ kind: "error", text: `${user.email} : ${explain(e)}` });
    } finally {
      setBusy(null);
    }
  }

  function onRole(user: AdminUser, role: AssignableRole): void {
    void runAction(
      user,
      () => api.setUserRole(user.id, role),
      `${user.email} : rôle ${ROLE_LABELS[role]}.`,
    );
  }

  function onToggle(user: AdminUser): void {
    if (user.disabled) {
      void runAction(user, () => api.enableUser(user.id), `${user.email} réactivé.`);
      return;
    }
    if (!confirm(`Suspendre ${user.email} ? Toutes ses sessions seront fermées.`)) return;
    void runAction(user, () => api.disableUser(user.id), `${user.email} suspendu.`);
  }

  function onDelete(user: AdminUser): void {
    const message = user.invitation_pending
      ? `Annuler l'invitation de ${user.email} ? Le compte sera supprimé et le lien envoyé ne fonctionnera plus.`
      : `Supprimer définitivement ${user.email} ? Son compte, ses workflows, ses exécutions et ses connexions aux services seront effacés. Cette action est irréversible.`;
    if (!confirm(message)) return;
    void runAction(
      user,
      async () => {
        await api.deleteUser(user.id);
        setLoad((l) =>
          l.kind === "ok"
            ? {
                ...l,
                items: l.items.filter((u) => u.id !== user.id),
                total: Math.max(0, l.total - 1),
              }
            : l,
        );
      },
      user.invitation_pending
        ? `Invitation de ${user.email} annulée.`
        : `${user.email} supprimé, ainsi que ses données.`,
    );
  }

  function onResend(user: AdminUser): void {
    void runAction(
      user,
      () => api.resendInvitation(user.id),
      `Invitation renvoyée à ${user.email} ; le lien précédent ne fonctionne plus.`,
      describeResend,
    );
  }

  async function onInvite(email: string, role: AssignableRole): Promise<boolean> {
    setNotice(null);
    try {
      const invited = await api.inviteUser(email, role);
      setNotice(
        invited.invitation_sent
          ? { kind: "ok", text: `Invitation envoyée à ${invited.user.email}.` }
          : {
              kind: "error",
              text: `Compte créé pour ${invited.user.email}, mais l'email n'est pas parti : renvoyez l'invitation.`,
            },
      );
      await reload();
      return true;
    } catch (e) {
      setNotice({ kind: "error", text: `Invitation refusée : ${describeInvite(e)}` });
      return false;
    }
  }

  function onVerifyEmail(user: AdminUser): void {
    const message = `Marquer l'adresse ${user.email} comme vérifiée ? Son titulaire pourra se connecter sans ouvrir le lien reçu : faites-le seulement si vous savez que l'adresse lui appartient.`;
    if (!confirm(message)) return;
    void runAction(
      user,
      () => api.verifyUserEmail(user.id),
      `Adresse ${user.email} vérifiée.`,
    );
  }

  function onResetMfa(user: AdminUser): void {
    const message = `Réinitialiser la double authentification de ${user.email} ? Il se connectera de nouveau avec son seul mot de passe : faites-le seulement après avoir vérifié son identité (téléphone et codes de secours perdus).`;
    if (!confirm(message)) return;
    void runAction(
      user,
      () => api.resetUserMfa(user.id),
      `Double authentification de ${user.email} réinitialisée.`,
    );
  }

  function onLogout(user: AdminUser): void {
    if (!confirm(`Fermer toutes les sessions de ${user.email} ?`)) return;
    void runAction(
      user,
      () => api.logoutUser(user.id),
      `Sessions de ${user.email} fermées.`,
    );
  }

  return (
    <section aria-labelledby="admin-title" className="mx-auto w-full max-w-5xl p-6">
      <header className="mb-4">
        <h2 id="admin-title" className="text-lg font-semibold text-neutral-900">
          Administration des utilisateurs
        </h2>
        <p className="mt-1 text-sm text-neutral-500">
          Un changement de rôle s'applique immédiatement ; un compte suspendu ne peut
          plus se connecter et toutes ses sessions sont fermées.
        </p>
      </header>

      <InviteForm superAdmin={superAdmin} onInvite={onInvite} />

      <form role="search" onSubmit={onSearch} className="mb-4 flex gap-2">
        <label htmlFor="admin-search" className="sr-only">
          Rechercher par email
        </label>
        <input
          id="admin-search"
          type="search"
          placeholder="Rechercher par email"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="w-full max-w-sm rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
        />
        <button
          type="submit"
          className="rounded-md border border-neutral-200 bg-white px-3 py-2 text-sm text-neutral-700 hover:bg-neutral-50"
        >
          Rechercher
        </button>
      </form>

      {notice && (
        <p
          role={notice.kind === "error" ? "alert" : "status"}
          className={`mb-4 rounded-md px-3 py-2 text-sm ${
            notice.kind === "error"
              ? "bg-rose-50 text-rose-800"
              : "bg-emerald-50 text-emerald-800"
          }`}
        >
          {notice.text}
        </p>
      )}

      {load.kind === "loading" && (
        <p role="status" className="text-sm text-neutral-500">
          Chargement des comptes…
        </p>
      )}
      {load.kind === "error" && (
        <p role="alert" className="text-sm text-rose-700">
          Liste des comptes indisponible : {load.message}
        </p>
      )}
      {load.kind === "ok" && load.items.length === 0 && (
        <p className="text-sm text-neutral-500">Aucun compte ne correspond.</p>
      )}
      {load.kind === "ok" && load.items.length > 0 && (
        <>
          <div className="overflow-x-auto rounded-lg border border-neutral-200 bg-white">
            <table aria-label="Comptes" className="w-full text-left text-sm">
              <thead className="border-b border-neutral-200 bg-neutral-50 text-neutral-500">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Email
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Rôle
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Statut
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Créé le
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Actions
                  </th>
                </tr>
              </thead>
              <tbody>
                {load.items.map((user) => (
                  <UserRow
                    key={user.id}
                    user={user}
                    self={user.id === currentUserId}
                    superAdmin={superAdmin}
                    busy={busy === user.id}
                    onRole={(role) => onRole(user, role)}
                    onToggle={() => onToggle(user)}
                    onLogout={() => onLogout(user)}
                    onDelete={() => onDelete(user)}
                    onVerifyEmail={() => onVerifyEmail(user)}
                    onResend={() => onResend(user)}
                    onResetMfa={() => onResetMfa(user)}
                    {...(superAdmin ? { onWorkflows: () => showWorkflowsOf(user) } : {})}
                  />
                ))}
              </tbody>
            </table>
          </div>
          <Pager
            offset={offset}
            count={load.items.length}
            total={load.total}
            pageSize={pageSize}
            onChange={setOffset}
            label="Pagination des comptes"
          />
        </>
      )}

      {superAdmin && (
        <AdminWorkflows
          api={api}
          owner={workflowOwner}
          onOwnerChange={setWorkflowOwner}
          pageSize={pageSize}
        />
      )}
    </section>
  );
}

/** Formulaire d'invitation (#241) : email et rôle du futur compte. */
function InviteForm({
  superAdmin,
  onInvite,
}: {
  superAdmin: boolean;
  /** `true` si l'invitation est créée (le formulaire se vide). */
  onInvite: (email: string, role: AssignableRole) => Promise<boolean>;
}): JSX.Element {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<AssignableRole>("editor");
  const [sending, setSending] = useState(false);

  async function onSubmit(e: FormEvent<HTMLFormElement>): Promise<void> {
    e.preventDefault();
    if (!email.trim()) return;
    setSending(true);
    if (await onInvite(email, role)) {
      setEmail("");
      setRole("editor");
    }
    setSending(false);
  }

  return (
    <form
      aria-label="Inviter une personne"
      onSubmit={(e) => void onSubmit(e)}
      className="mb-6 flex flex-wrap items-end gap-2 rounded-lg border border-neutral-200 bg-white p-4"
    >
      <div className="min-w-56 flex-1">
        <label htmlFor="invite-email" className="block text-sm font-medium text-neutral-700">
          Inviter par email
        </label>
        <input
          id="invite-email"
          type="email"
          placeholder="prenom.nom@exemple.fr"
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
        />
      </div>
      <div>
        <label htmlFor="invite-role" className="block text-sm font-medium text-neutral-700">
          Rôle
        </label>
        <select
          id="invite-role"
          value={role}
          onChange={(e) => setRole(e.target.value as AssignableRole)}
          className="mt-1 rounded-md border border-neutral-300 bg-white px-2 py-2 text-sm"
        >
          {ASSIGNABLE.map((r) => (
            <option key={r} value={r} disabled={r === "admin" && !superAdmin}>
              {ROLE_LABELS[r]}
            </option>
          ))}
        </select>
      </div>
      <button
        type="submit"
        disabled={sending || !email.trim()}
        className="rounded-md bg-neutral-900 px-3 py-2 text-sm font-medium text-white hover:bg-neutral-700 disabled:opacity-50"
      >
        {sending ? "Envoi…" : "Inviter"}
      </button>
      <p className="w-full text-xs text-neutral-500">
        La personne reçoit un lien valable 7 jours pour choisir son mot de passe.
      </p>
    </form>
  );
}

function UserRow({
  user,
  self,
  superAdmin,
  busy,
  onRole,
  onToggle,
  onLogout,
  onDelete,
  onVerifyEmail,
  onResend,
  onResetMfa,
  onWorkflows,
}: {
  user: AdminUser;
  self: boolean;
  superAdmin: boolean;
  busy: boolean;
  onRole: (role: AssignableRole) => void;
  onToggle: () => void;
  onLogout: () => void;
  onDelete: () => void;
  onVerifyEmail: () => void;
  onResend: () => void;
  onResetMfa: () => void;
  /** Super-admin seulement : filtre la liste des workflows sur ce compte. */
  onWorkflows?: () => void;
}): JSX.Element {
  // Un super-admin n'est gérable que par un super-admin (refus 403 sinon).
  const locked = busy || (user.role === "admin" && !superAdmin);
  const created = new Date(user.created_at_ms).toLocaleDateString("fr-FR");
  return (
    <tr className="border-b border-neutral-100 last:border-0">
      <td className="px-3 py-2 text-neutral-900">
        {user.email}
        {self && <span className="ml-2 text-xs text-neutral-500">(vous)</span>}
      </td>
      <td className="px-3 py-2">
        <label className="sr-only" htmlFor={`role-${user.id}`}>
          Rôle de {user.email}
        </label>
        <select
          id={`role-${user.id}`}
          value={user.role}
          disabled={locked || self}
          onChange={(e) => onRole(e.target.value as AssignableRole)}
          className="rounded-md border border-neutral-300 bg-white px-2 py-1 text-sm disabled:opacity-60"
        >
          {user.role === "custom" && (
            <option value="custom" disabled>
              {ROLE_LABELS.custom}
            </option>
          )}
          {ASSIGNABLE.map((role) => (
            <option key={role} value={role} disabled={role === "admin" && !superAdmin}>
              {ROLE_LABELS[role]}
            </option>
          ))}
        </select>
      </td>
      <td className="px-3 py-2">
        {user.disabled ? (
          <span className="mr-1 rounded bg-rose-50 px-2 py-0.5 text-xs font-medium text-rose-700">
            Suspendu
          </span>
        ) : (
          !user.invitation_pending && (
            <span className="mr-1 rounded bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-700">
              Actif
            </span>
          )
        )}
        {user.invitation_pending ? (
          <span className="rounded bg-sky-50 px-2 py-0.5 text-xs font-medium text-sky-800">
            Invitation en attente
          </span>
        ) : (
          !user.email_verified && (
            <span className="rounded bg-amber-50 px-2 py-0.5 text-xs font-medium text-amber-800">
              Email non vérifié
            </span>
          )
        )}
        {user.mfa_enabled && (
          <span
            title="Double authentification active"
            className="ml-1 rounded bg-indigo-50 px-2 py-0.5 text-xs font-medium text-indigo-800"
          >
            MFA
          </span>
        )}
      </td>
      <td className="px-3 py-2 text-neutral-500">{created}</td>
      <td className="px-3 py-2">
        <div className="flex gap-2">
          {onWorkflows && (
            <button
              type="button"
              onClick={onWorkflows}
              aria-label={`Voir les workflows de ${user.email}`}
              className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50"
            >
              Workflows
            </button>
          )}
          {user.invitation_pending && (
            <button
              type="button"
              onClick={onResend}
              disabled={locked || user.disabled}
              aria-label={`Renvoyer l'invitation de ${user.email}`}
              className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
            >
              Renvoyer l'invitation
            </button>
          )}
          {!user.email_verified && !user.invitation_pending && (
            <button
              type="button"
              onClick={onVerifyEmail}
              disabled={locked}
              aria-label={`Vérifier l'adresse de ${user.email}`}
              className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
            >
              Vérifier l'email
            </button>
          )}
          {user.mfa_enabled && (
            <button
              type="button"
              onClick={onResetMfa}
              disabled={locked || self}
              aria-label={`Réinitialiser la double authentification de ${user.email}`}
              className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
            >
              Réinitialiser la MFA
            </button>
          )}
          <button
            type="button"
            onClick={onToggle}
            disabled={locked || self}
            aria-label={`${user.disabled ? "Réactiver" : "Suspendre"} ${user.email}`}
            className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
          >
            {user.disabled ? "Réactiver" : "Suspendre"}
          </button>
          <button
            type="button"
            onClick={onLogout}
            disabled={locked || self}
            aria-label={`Fermer les sessions de ${user.email}`}
            className="rounded-md border border-neutral-200 px-2 py-1 text-xs text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
          >
            Fermer les sessions
          </button>
          <button
            type="button"
            onClick={onDelete}
            disabled={locked || self}
            aria-label={
              user.invitation_pending
                ? `Annuler l'invitation de ${user.email}`
                : `Supprimer ${user.email}`
            }
            className="rounded-md border border-rose-200 px-2 py-1 text-xs text-rose-700 hover:bg-rose-50 disabled:opacity-60"
          >
            {user.invitation_pending ? "Annuler l'invitation" : "Supprimer"}
          </button>
        </div>
      </td>
    </tr>
  );
}
