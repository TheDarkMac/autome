// Section « Workflows de tous les comptes » de l'Administration (#243).
//
// Réservée au super-admin (`*`) : depuis le namespacing des ids (#188),
// chaque compte a son propre espace et `GET /workflows` ne montre que celui de
// l'appelant. Cette vue liste les workflows de tous les espaces avec leur
// propriétaire ; on la filtre par propriétaire (clic sur son email ici, ou
// bouton « Workflows » d'une ligne de la liste des comptes) et par id ou nom.
// Les workflows sans propriétaire (antérieurs à la migration `0010`) forment
// leur propre espace, filtrable lui aussi. Lecture seule : le serveur ne
// renvoie qu'un résumé, jamais le graphe.

import { useCallback, useEffect, useState, type FormEvent } from "react";

import { ApiError } from "./api";
import type { ApiClient, Schemas } from "./apiClient";
import { Pager } from "./Pager";

type AdminWorkflow = Schemas["AdminWorkflow"];

export type AdminWorkflowsApi = Pick<ApiClient, "listAllWorkflows">;

/** Propriétaire filtré : son id (`""` = sans propriétaire) et son email. */
export interface OwnerFilter {
  id: string;
  email: string | null;
}

export interface AdminWorkflowsProps {
  api: AdminWorkflowsApi;
  owner: OwnerFilter | null;
  onOwnerChange: (owner: OwnerFilter | null) => void;
  pageSize?: number;
}

type Load =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ok"; items: AdminWorkflow[]; total: number };

/** Libellé d'un propriétaire, compte connu ou non. */
function ownerLabel(owner: OwnerFilter): string {
  if (owner.id === "") return "Sans propriétaire";
  return owner.email ?? `Compte introuvable (${owner.id})`;
}

function describe(e: unknown): string {
  if (e instanceof ApiError && e.status === 403) {
    return "réservée aux super-administrateurs";
  }
  return (e as Error).message;
}

export function AdminWorkflows({
  api,
  owner,
  onOwnerChange,
  pageSize = 50,
}: AdminWorkflowsProps): JSX.Element {
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  const ownerId = owner?.id;
  // La page est celle du propriétaire filtré : un autre propriétaire repart
  // de la première page, sans requête intermédiaire.
  const [paging, setPaging] = useState({ ownerId, offset: 0 });
  const offset = paging.ownerId === ownerId ? paging.offset : 0;
  const setOffset = (next: number): void =>
    setPaging({ ownerId, offset: next });

  const reload = useCallback(async () => {
    setLoad({ kind: "loading" });
    try {
      const page = await api.listAllWorkflows({
        ...(ownerId !== undefined ? { owner: ownerId } : {}),
        q: query,
        offset,
        limit: pageSize,
      });
      setLoad({ kind: "ok", items: page.items, total: page.total });
    } catch (e) {
      setLoad({ kind: "error", message: describe(e) });
    }
  }, [api, ownerId, query, offset, pageSize]);

  useEffect(() => {
    void reload();
  }, [reload]);

  function onSearch(e: FormEvent<HTMLFormElement>): void {
    e.preventDefault();
    setOffset(0);
    setQuery(search.trim());
  }

  return (
    <section
      id="admin-workflows"
      aria-labelledby="admin-workflows-title"
      className="mt-10 border-t border-neutral-200 pt-6"
    >
      <header className="mb-4">
        <h2
          id="admin-workflows-title"
          className="text-lg font-semibold text-neutral-900"
        >
          Workflows de tous les comptes
        </h2>
        <p className="mt-1 text-sm text-neutral-500">
          Chaque compte a son propre espace de workflows ; cette liste les
          réunit avec leur propriétaire.
        </p>
      </header>

      <div className="mb-4 flex flex-wrap items-center gap-2">
        <form role="search" onSubmit={onSearch} className="flex gap-2">
          <label htmlFor="admin-workflows-search" className="sr-only">
            Rechercher un workflow par id ou nom
          </label>
          <input
            id="admin-workflows-search"
            type="search"
            placeholder="Rechercher par id ou nom"
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
        {owner ? (
          <span className="inline-flex items-center gap-1 rounded-full bg-sky-50 py-1 pl-3 pr-1 text-sm text-sky-900">
            Propriétaire : {ownerLabel(owner)}
            <button
              type="button"
              onClick={() => onOwnerChange(null)}
              aria-label="Retirer le filtre par propriétaire"
              className="rounded-full px-2 text-sky-700 hover:bg-sky-100"
            >
              ×
            </button>
          </span>
        ) : (
          <button
            type="button"
            onClick={() => onOwnerChange({ id: "", email: null })}
            className="rounded-md px-2 py-1 text-sm text-neutral-600 underline-offset-2 hover:underline"
          >
            Voir les workflows sans propriétaire
          </button>
        )}
      </div>

      {load.kind === "loading" && (
        <p role="status" className="text-sm text-neutral-500">
          Chargement des workflows…
        </p>
      )}
      {load.kind === "error" && (
        <p role="alert" className="text-sm text-rose-700">
          Liste des workflows indisponible : {load.message}
        </p>
      )}
      {load.kind === "ok" && load.items.length === 0 && (
        <p className="text-sm text-neutral-500">
          Aucun workflow ne correspond.
        </p>
      )}
      {load.kind === "ok" && load.items.length > 0 && (
        <>
          <div className="overflow-x-auto rounded-lg border border-neutral-200 bg-white">
            <table aria-label="Workflows" className="w-full text-left text-sm">
              <thead className="border-b border-neutral-200 bg-neutral-50 text-neutral-500">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Workflow
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Propriétaire
                  </th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">
                    Nœuds
                  </th>
                </tr>
              </thead>
              <tbody>
                {load.items.map((w) => (
                  <WorkflowRow
                    key={`${w.owner_id}/${w.id}`}
                    workflow={w}
                    filtered={owner !== null}
                    onOwner={() =>
                      onOwnerChange({
                        id: w.owner_id,
                        email: w.owner_email ?? null,
                      })
                    }
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
            label="Pagination des workflows"
          />
        </>
      )}
    </section>
  );
}

function WorkflowRow({
  workflow,
  filtered,
  onOwner,
}: {
  workflow: AdminWorkflow;
  /** Un filtre par propriétaire est déjà actif : propriétaire en texte. */
  filtered: boolean;
  onOwner: () => void;
}): JSX.Element {
  const label = ownerLabel({
    id: workflow.owner_id,
    email: workflow.owner_email ?? null,
  });
  const known = workflow.owner_id !== "" && workflow.owner_email != null;
  return (
    <tr className="border-b border-neutral-100 last:border-0">
      <td className="px-3 py-2">
        <div className="text-neutral-900">{workflow.name || workflow.id}</div>
        <div className="font-mono text-xs text-neutral-500">{workflow.id}</div>
      </td>
      <td className="px-3 py-2">
        {filtered ? (
          <OwnerText label={label} known={known} />
        ) : (
          <button
            type="button"
            onClick={onOwner}
            title="Ne montrer que les workflows de ce propriétaire"
            aria-label={`Workflows de ${label}`}
            className="text-left hover:underline"
          >
            <OwnerText label={label} known={known} />
          </button>
        )}
      </td>
      <td className="px-3 py-2 text-right tabular-nums text-neutral-700">
        {workflow.node_count}
      </td>
    </tr>
  );
}

function OwnerText({
  label,
  known,
}: {
  label: string;
  known: boolean;
}): JSX.Element {
  return known ? (
    <span className="text-neutral-900">{label}</span>
  ) : (
    <span className="rounded bg-amber-50 px-2 py-0.5 text-xs font-medium text-amber-800">
      {label}
    </span>
  );
}
