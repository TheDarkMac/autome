// Client API typé (F8.7.a).
//
// Façade unique pour parler au backend Autome. Les types des enveloppes
// (requêtes/réponses) proviennent de `apiSchema.ts`, **généré depuis la spec
// OpenAPI** du backend (`pnpm gen:api`, source : `frontend/openapi.json` —
// elle-même produite par `cargo run -p autome-api --example dump_openapi`).
//
// Le client est paramétré par un `fetchImpl` : en production c'est l'`authFetch`
// (F8.7.c) qui injecte le Bearer + gère le refresh ; en test, un fetch factice.
// Les chemins vivent dans `API_PATHS` (templates `{param}` identiques à la spec)
// — un test vérifie qu'ils existent tous dans `openapi.json` (garde anti-drift).

import { ApiError, apiBaseUrl } from "./api";
import type { components } from "./apiSchema";
import type { WireGraph } from "./graph";
import {
  fetchRun,
  fetchRunsForWorkflow,
  type FetchRunsParams,
  type RunCheckpoint,
  type RunListPage,
} from "./runs";

/** Raccourci vers les schémas générés depuis OpenAPI. */
export type Schemas = components["schemas"];

/** Templates de chemins — verbatim de la spec OpenAPI (placeholders `{x}`). */
export const API_PATHS = {
  workflows: "/workflows",
  workflow: "/workflows/{id}",
  workflowRuns: "/workflows/{id}/runs",
  webhooks: "/workflows/{id}/webhooks",
  webhookRotate: "/workflows/{id}/webhooks/{node_id}/rotate",
  run: "/runs/{run_id}",
  credentials: "/credentials",
  providers: "/credentials/providers",
  credential: "/credentials/{provider}",
  authorize: "/credentials/{provider}/authorize",
  callback: "/credentials/{provider}/callback",
  me: "/auth/me",
  adminUsers: "/admin/users",
  adminUser: "/admin/users/{id}",
  adminUserRole: "/admin/users/{id}/role",
  adminUserDisable: "/admin/users/{id}/disable",
  adminUserEnable: "/admin/users/{id}/enable",
  adminUserLogoutAll: "/admin/users/{id}/logout-all",
} as const;

/** Rôles attribuables par l'administration (#208). */
export type AssignableRole = "admin" | "editor" | "viewer";

export interface ListUsersParams {
  /** Filtre : sous-chaîne de l'email. */
  q?: string;
  offset?: number;
  limit?: number;
}

/** Page de workflows : enveloppe `ListResponse`, items typés `WireGraph`. */
export interface WorkflowPage {
  readonly items: WireGraph[];
  readonly total: number;
  readonly offset: number;
  readonly limit: number;
}

export interface ListWorkflowsParams {
  offset?: number;
  limit?: number;
}

/** Surface typée du backend Autome. */
export interface ApiClient {
  listWorkflows(params?: ListWorkflowsParams): Promise<WorkflowPage>;
  getWorkflow(id: string): Promise<WireGraph>;
  createWorkflow(workflow: WireGraph): Promise<WireGraph>;
  updateWorkflow(id: string, workflow: WireGraph): Promise<WireGraph>;
  deleteWorkflow(id: string): Promise<void>;

  listRuns(workflowId: string, params?: FetchRunsParams): Promise<RunListPage>;
  createRun(
    workflowId: string,
    body?: Schemas["CreateRunBody"],
  ): Promise<Schemas["CreateRunResponse"]>;
  getRun(runId: string): Promise<RunCheckpoint>;

  /** URL secrètes des nœuds webhook d'un workflow enregistré (#202). */
  listWebhooks(workflowId: string): Promise<Schemas["WebhookList"]>;
  /** Nouvelle URL pour un nœud webhook ; l'ancienne cesse de fonctionner. */
  rotateWebhook(workflowId: string, nodeId: string): Promise<Schemas["WebhookInfo"]>;

  listProviders(): Promise<Schemas["ListProvidersResponse"]>;
  listCredentials(): Promise<Schemas["ListCredentialsResponse"]>;
  startAuthorize(provider: string): Promise<Schemas["AuthorizeResponse"]>;
  completeAuthorize(provider: string, body: Schemas["CallbackBody"]): Promise<void>;
  deleteCredential(provider: string): Promise<void>;

  /** Utilisateur courant, son rôle et ses permissions (#208). */
  me(): Promise<Schemas["MeResponse"]>;
  /** Administration des utilisateurs (#208) — permission `users.admin`. */
  listUsers(params?: ListUsersParams): Promise<Schemas["UserListResponse"]>;
  setUserRole(id: string, role: AssignableRole): Promise<Schemas["AdminUser"]>;
  disableUser(id: string): Promise<Schemas["AdminUser"]>;
  enableUser(id: string): Promise<Schemas["AdminUser"]>;
  /** Ferme toutes les sessions d'un autre utilisateur. */
  logoutUser(id: string): Promise<void>;
}

/** `true` si les permissions contiennent `permission` (ou le joker `*`). */
export function hasPermission(permissions: readonly string[], permission: string): boolean {
  return permissions.includes("*") || permissions.includes(permission);
}

/** `true` si les permissions donnent accès à l'administration des utilisateurs. */
export function canAdministerUsers(permissions: readonly string[]): boolean {
  return hasPermission(permissions, "users.admin");
}

/**
 * Construit un client adossé à `fetchImpl`. En production, passer l'`authFetch`
 * issu de la session ; par défaut le `fetch` global (non authentifié).
 */
export function createApiClient(fetchImpl: typeof fetch = fetch): ApiClient {
  return {
    async listWorkflows(params = {}) {
      const env = await requestJson<Schemas["ListResponse"]>(
        fetchImpl,
        "GET",
        API_PATHS.workflows,
        { query: { offset: params.offset, limit: params.limit } },
      );
      return {
        items: (env.items as unknown[]).map(coerceWorkflow),
        total: env.total,
        offset: env.offset,
        limit: env.limit,
      };
    },

    getWorkflow(id) {
      return requestJson<unknown>(
        fetchImpl,
        "GET",
        buildPath(API_PATHS.workflow, { id }),
      ).then(coerceWorkflow);
    },

    createWorkflow(workflow) {
      return requestJson<unknown>(fetchImpl, "POST", API_PATHS.workflows, {
        body: workflow,
      }).then(coerceWorkflow);
    },

    updateWorkflow(id, workflow) {
      return requestJson<unknown>(
        fetchImpl,
        "PUT",
        buildPath(API_PATHS.workflow, { id }),
        { body: workflow },
      ).then(coerceWorkflow);
    },

    deleteWorkflow(id) {
      return requestVoid(fetchImpl, "DELETE", buildPath(API_PATHS.workflow, { id }));
    },

    // Runs : on réutilise les helpers tolérants de `runs.ts` (parsing partagé)
    // en leur passant l'`authFetch`.
    listRuns(workflowId, params) {
      return fetchRunsForWorkflow(workflowId, params, fetchImpl);
    },
    createRun(workflowId, body) {
      return requestJson<Schemas["CreateRunResponse"]>(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.workflowRuns, { id: workflowId }),
        { body: body ?? {} },
      );
    },
    getRun(runId) {
      return fetchRun(runId, fetchImpl);
    },

    listWebhooks(workflowId) {
      return requestJson<Schemas["WebhookList"]>(
        fetchImpl,
        "GET",
        buildPath(API_PATHS.webhooks, { id: workflowId }),
      );
    },
    rotateWebhook(workflowId, nodeId) {
      return requestJson<Schemas["WebhookInfo"]>(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.webhookRotate, { id: workflowId, node_id: nodeId }),
        { body: {} },
      );
    },

    listProviders() {
      return requestJson<Schemas["ListProvidersResponse"]>(
        fetchImpl,
        "GET",
        API_PATHS.providers,
      );
    },
    listCredentials() {
      return requestJson<Schemas["ListCredentialsResponse"]>(
        fetchImpl,
        "GET",
        API_PATHS.credentials,
      );
    },
    startAuthorize(provider) {
      return requestJson<Schemas["AuthorizeResponse"]>(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.authorize, { provider }),
        { body: {} },
      );
    },
    completeAuthorize(provider, body) {
      return requestVoid(fetchImpl, "POST", buildPath(API_PATHS.callback, { provider }), body);
    },
    deleteCredential(provider) {
      return requestVoid(
        fetchImpl,
        "DELETE",
        buildPath(API_PATHS.credential, { provider }),
      );
    },

    me() {
      return requestJson<Schemas["MeResponse"]>(fetchImpl, "GET", API_PATHS.me);
    },
    listUsers(params = {}) {
      return requestJson<Schemas["UserListResponse"]>(
        fetchImpl,
        "GET",
        API_PATHS.adminUsers,
        {
          query: {
            q: params.q?.trim() || undefined,
            offset: params.offset,
            limit: params.limit,
          },
        },
      );
    },
    setUserRole(id, role) {
      return requestJson<Schemas["AdminUser"]>(
        fetchImpl,
        "PUT",
        buildPath(API_PATHS.adminUserRole, { id }),
        { body: { role } },
      );
    },
    disableUser(id) {
      return requestJson<Schemas["AdminUser"]>(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.adminUserDisable, { id }),
        { body: {} },
      );
    },
    enableUser(id) {
      return requestJson<Schemas["AdminUser"]>(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.adminUserEnable, { id }),
        { body: {} },
      );
    },
    logoutUser(id) {
      return requestVoid(
        fetchImpl,
        "POST",
        buildPath(API_PATHS.adminUserLogoutAll, { id }),
        {},
      );
    },
  };
}

// ───────────────────────────── helpers ─────────────────────────────

/** Remplace les `{param}` d'un template par des valeurs encodées. */
export function buildPath(
  template: string,
  params: Readonly<Record<string, string>>,
): string {
  return template.replace(/\{(\w+)\}/g, (_m, key: string) => {
    const v = params[key];
    if (v === undefined) throw new ApiError(`paramètre de chemin manquant : ${key}`);
    return encodeURIComponent(v);
  });
}

function buildQuery(
  query: Readonly<Record<string, string | number | undefined>> | undefined,
): string {
  if (!query) return "";
  const parts: string[] = [];
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined) parts.push(`${encodeURIComponent(k)}=${encodeURIComponent(v)}`);
  }
  return parts.length > 0 ? `?${parts.join("&")}` : "";
}

interface RequestOptions {
  body?: unknown;
  query?: Readonly<Record<string, string | number | undefined>>;
}

async function send(
  fetchImpl: typeof fetch,
  method: string,
  path: string,
  opts: RequestOptions,
): Promise<Response> {
  const url = `${apiBaseUrl()}${path}${buildQuery(opts.query)}`;
  const headers: Record<string, string> = { Accept: "application/json" };
  if (opts.body !== undefined) headers["Content-Type"] = "application/json";
  let res: Response;
  try {
    res = await fetchImpl(url, {
      method,
      headers,
      ...(opts.body !== undefined ? { body: JSON.stringify(opts.body) } : {}),
    });
  } catch (e) {
    throw new ApiError(`backend injoignable : ${(e as Error).message}`);
  }
  if (!res.ok) {
    throw new ApiError(`${method} ${path} a échoué (${res.status})`, res.status);
  }
  return res;
}

async function requestJson<T>(
  fetchImpl: typeof fetch,
  method: string,
  path: string,
  opts: RequestOptions = {},
): Promise<T> {
  const res = await send(fetchImpl, method, path, opts);
  return (await res.json()) as T;
}

async function requestVoid(
  fetchImpl: typeof fetch,
  method: string,
  path: string,
  body?: unknown,
): Promise<void> {
  await send(fetchImpl, method, path, body !== undefined ? { body } : {});
}

/** Valide la forme minimale d'un workflow renvoyé par le backend. */
function coerceWorkflow(raw: unknown): WireGraph {
  if (typeof raw !== "object" || raw === null) {
    throw new ApiError("workflow mal formé : objet attendu");
  }
  const v = raw as Record<string, unknown>;
  const id = typeof v["id"] === "string" ? v["id"] : null;
  const name = typeof v["name"] === "string" ? v["name"] : null;
  if (id === null || name === null) {
    throw new ApiError("workflow mal formé : `id`/`name` requis");
  }
  return {
    id,
    name,
    nodes: Array.isArray(v["nodes"]) ? (v["nodes"] as WireGraph["nodes"]) : [],
    edges: Array.isArray(v["edges"]) ? (v["edges"] as WireGraph["edges"]) : [],
  };
}
