// Onglet « Administration » (#208) : comptes, rôles, suspension, sessions.
//
// Visible des seuls détenteurs de `users.admin` (ou `*`). Le serveur reste
// seul juge : le panneau grise ce qu'il refuserait (son propre compte, le
// rôle `admin` et les super-admins sans `*`) et traduit ses refus en clair.
// Suspendre un compte ou fermer ses sessions est confirmé, comme la
// déconnexion de tous ses appareils ; la suppression définitive d'un compte
// et de ses données (#229) aussi. Le client API est injecté pour les tests.

import { useCallback, useEffect, useState, type FormEvent } from "react";

import { ApiError } from "./api";
import type { ApiClient, AssignableRole, Schemas } from "./apiClient";

type AdminUser = Schemas["AdminUser"];

export type AdminApi = Pick<
  ApiClient,
  "listUsers" | "setUserRole" | "disableUser" | "enableUser" | "logoutUser" | "deleteUser"
>;

export interface AdminPanelProps {
  api: AdminApi;
  /** Identifiant de l'administrateur connecté (actions sur soi grisées). */
  currentUserId: string;
  /** L'administrateur détient `*` : rôle `admin` et super-admins gérables. */
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
      setNotice({ kind: "error", text: `${user.email} : ${describe(e)}` });
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
    const message = `Supprimer définitivement ${user.email} ? Son compte, ses workflows, ses exécutions et ses connexions aux services seront effacés. Cette action est irréversible.`;
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
      `${user.email} supprimé, ainsi que ses données.`,
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
            <table className="w-full text-left text-sm">
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
          />
        </>
      )}
    </section>
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
}: {
  user: AdminUser;
  self: boolean;
  superAdmin: boolean;
  busy: boolean;
  onRole: (role: AssignableRole) => void;
  onToggle: () => void;
  onLogout: () => void;
  onDelete: () => void;
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
          <span className="rounded bg-rose-50 px-2 py-0.5 text-xs font-medium text-rose-700">
            Suspendu
          </span>
        ) : (
          <span className="rounded bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-700">
            Actif
          </span>
        )}
      </td>
      <td className="px-3 py-2 text-neutral-500">{created}</td>
      <td className="px-3 py-2">
        <div className="flex gap-2">
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
            aria-label={`Supprimer ${user.email}`}
            className="rounded-md border border-rose-200 px-2 py-1 text-xs text-rose-700 hover:bg-rose-50 disabled:opacity-60"
          >
            Supprimer
          </button>
        </div>
      </td>
    </tr>
  );
}

function Pager({
  offset,
  count,
  total,
  pageSize,
  onChange,
}: {
  offset: number;
  count: number;
  total: number;
  pageSize: number;
  onChange: (offset: number) => void;
}): JSX.Element {
  return (
    <nav aria-label="Pagination" className="mt-3 flex items-center gap-3 text-sm">
      <span className="text-neutral-500">
        {offset + 1}–{offset + count} sur {total}
      </span>
      <button
        type="button"
        disabled={offset === 0}
        onClick={() => onChange(Math.max(0, offset - pageSize))}
        className="rounded-md border border-neutral-200 px-2 py-1 text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
      >
        Précédent
      </button>
      <button
        type="button"
        disabled={offset + count >= total}
        onClick={() => onChange(offset + pageSize)}
        className="rounded-md border border-neutral-200 px-2 py-1 text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
      >
        Suivant
      </button>
    </nav>
  );
}
