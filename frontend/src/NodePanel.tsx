// Panneau de configuration d'un node (F8.4).
//
// F8.4.a — Le formulaire est généré dynamiquement depuis le `NodeSchema`
//          du couple (kind, subtype). Aucun composant spécifique par type.
// F8.4.b — La validation s'exécute à chaque rendu via `validateParams` ;
//          les erreurs s'affichent sous chaque champ ; un récap en haut
//          indique l'état global.
// F8.4.c — Les champs string/text détectent les expressions de la forme
//          `{{ $node('id').path }}` et affichent les références extraites
//          sous forme de chips.
// F8.4.d — Si le schéma déclare `credentialProvider`, une dropdown est
//          chargée via `fetchCredentials(provider)`. La valeur est
//          stockée dans `params.credential_id`.
//
// L'édition est **live** : chaque modification déclenche `onChange`
// immédiatement (le parent met à jour le node dans le canvas). Pas de
// bouton « Enregistrer » — l'état des erreurs est visuel.

import { useEffect, useMemo, useState } from "react";

import { fetchCredentials, type Credential } from "./credentials";
import type { CanvasNode } from "./graph";
import {
  type ExpressionReference,
  type NodeSchema,
  type ParamSchema,
  type ParamValue,
  type ParamValues,
  coerceNumberInput,
  defaultParams,
  extractReferences,
  findNodeSchema,
  validateParams,
} from "./paramSchema";

export interface NodePanelProps {
  node: CanvasNode | null;
  onChange: (nodeId: string, params: ParamValues) => void;
  onClose: () => void;
  /** Override pour tests. */
  fetchCredentialsImpl?: typeof fetchCredentials;
}

const CREDENTIAL_KEY = "credential_id";

export function NodePanel({
  node,
  onChange,
  onClose,
  fetchCredentialsImpl = fetchCredentials,
}: NodePanelProps): JSX.Element {
  if (!node) return <EmptyPanel />;
  const schema = findNodeSchema(node.data.kind, node.data.subtype);
  if (!schema) {
    return <UnknownSchemaPanel node={node} onClose={onClose} />;
  }
  return (
    <NodePanelForm
      key={node.id}
      node={node}
      schema={schema}
      onChange={onChange}
      onClose={onClose}
      fetchCredentialsImpl={fetchCredentialsImpl}
    />
  );
}

// ─────────────── Empty / unknown ───────────────

function EmptyPanel(): JSX.Element {
  return (
    <aside
      aria-label="Panneau de configuration"
      data-testid="node-panel"
      data-empty="true"
      className="flex h-full w-80 shrink-0 flex-col border-l border-neutral-200 bg-white"
    >
      <header className="border-b border-neutral-200 px-4 py-3">
        <h2 className="text-sm font-semibold text-neutral-800">Configuration</h2>
        <p className="mt-0.5 text-xs text-neutral-500">
          Sélectionnez un node sur le canvas.
        </p>
      </header>
    </aside>
  );
}

function UnknownSchemaPanel({
  node,
  onClose,
}: {
  node: CanvasNode;
  onClose: () => void;
}): JSX.Element {
  return (
    <aside
      aria-label="Panneau de configuration"
      data-testid="node-panel"
      data-unknown="true"
      className="flex h-full w-80 shrink-0 flex-col border-l border-neutral-200 bg-white"
    >
      <PanelHeader title={node.data.label} subtitle={node.data.kind} onClose={onClose} />
      <p className="px-4 py-3 text-sm text-neutral-600">
        Aucun schéma de paramètres n'est défini pour ce node ({node.data.kind}
        {node.data.subtype ? ` · ${node.data.subtype}` : ""}).
      </p>
    </aside>
  );
}

// ─────────────── Form ───────────────

interface NodePanelFormProps {
  node: CanvasNode;
  schema: NodeSchema;
  onChange: (nodeId: string, params: ParamValues) => void;
  onClose: () => void;
  fetchCredentialsImpl: typeof fetchCredentials;
}

function NodePanelForm({
  node,
  schema,
  onChange,
  onClose,
  fetchCredentialsImpl,
}: NodePanelFormProps): JSX.Element {
  const initial = useMemo(
    () => initialValues(node, schema),
    [node, schema],
  );
  const [values, setValues] = useState<ParamValues>(initial);
  const errors = useMemo(() => validateParams(schema, values), [schema, values]);
  const errorCount = Object.keys(errors).length;

  const updateValue = (name: string, value: ParamValue): void => {
    const next: ParamValues = { ...values };
    if (value === null || value === "") delete next[name];
    else next[name] = value;
    setValues(next);
    onChange(node.id, next);
  };

  return (
    <aside
      aria-label="Panneau de configuration"
      data-testid="node-panel"
      data-node-id={node.id}
      className="flex h-full w-80 shrink-0 flex-col border-l border-neutral-200 bg-white"
    >
      <PanelHeader title={node.data.label} subtitle={schema.label} onClose={onClose} />

      <div
        className={`border-b px-4 py-2 text-xs ${
          errorCount === 0
            ? "border-emerald-100 bg-emerald-50 text-emerald-800"
            : "border-rose-100 bg-rose-50 text-rose-800"
        }`}
        role={errorCount === 0 ? "status" : "alert"}
        data-testid="panel-status"
      >
        {errorCount === 0
          ? "Configuration valide."
          : `${errorCount} erreur${errorCount > 1 ? "s" : ""} à corriger.`}
      </div>

      <form
        className="flex-1 space-y-4 overflow-y-auto px-4 py-4"
        onSubmit={(e) => e.preventDefault()}
      >
        {schema.params.map((p) => (
          <ParamField
            key={p.name}
            schema={p}
            value={values[p.name] ?? null}
            error={errors[p.name]}
            onChange={(v) => updateValue(p.name, v)}
          />
        ))}

        {schema.credentialProvider ? (
          <CredentialField
            provider={schema.credentialProvider}
            value={typeof values[CREDENTIAL_KEY] === "string" ? (values[CREDENTIAL_KEY] as string) : ""}
            onChange={(id) => updateValue(CREDENTIAL_KEY, id === "" ? null : id)}
            fetchImpl={fetchCredentialsImpl}
          />
        ) : null}
      </form>
    </aside>
  );
}

function PanelHeader({
  title,
  subtitle,
  onClose,
}: {
  title: string;
  subtitle: string;
  onClose: () => void;
}): JSX.Element {
  return (
    <header className="flex items-start justify-between gap-2 border-b border-neutral-200 px-4 py-3">
      <div className="min-w-0">
        <h2 className="truncate text-sm font-semibold text-neutral-800">{title}</h2>
        <p className="mt-0.5 truncate text-xs text-neutral-500">{subtitle}</p>
      </div>
      <button
        type="button"
        onClick={onClose}
        aria-label="Fermer le panneau"
        className="rounded-md p-1 text-neutral-400 hover:bg-neutral-100 hover:text-neutral-700"
      >
        ×
      </button>
    </header>
  );
}

// ─────────────── Champs ───────────────

interface ParamFieldProps {
  schema: ParamSchema;
  value: ParamValue;
  error: string | undefined;
  onChange: (v: ParamValue) => void;
}

function ParamField({ schema, value, error, onChange }: ParamFieldProps): JSX.Element {
  const id = `param-${schema.name}`;
  const baseInputCls =
    "w-full rounded-md border bg-white px-3 py-1.5 text-sm outline-none transition focus:border-sky-400";
  const errCls = error ? "border-rose-400" : "border-neutral-200";
  const cls = `${baseInputCls} ${errCls}`;
  const refs = schema.type === "string" || schema.type === "text"
    ? extractReferences(value)
    : [];

  let input: JSX.Element;
  switch (schema.type) {
    case "string":
      input = (
        <input
          id={id}
          type="text"
          value={typeof value === "string" ? value : ""}
          placeholder={schema.placeholder ?? ""}
          onChange={(e) => onChange(e.target.value)}
          className={cls}
          aria-invalid={error ? "true" : undefined}
        />
      );
      break;
    case "text":
      input = (
        <textarea
          id={id}
          rows={4}
          value={typeof value === "string" ? value : ""}
          placeholder={schema.placeholder ?? ""}
          onChange={(e) => onChange(e.target.value)}
          className={`${cls} font-mono`}
          aria-invalid={error ? "true" : undefined}
        />
      );
      break;
    case "number":
      input = (
        <input
          id={id}
          type="number"
          value={typeof value === "number" ? value : ""}
          placeholder={schema.placeholder ?? ""}
          onChange={(e) => onChange(coerceNumberInput(e.target.value))}
          className={cls}
          aria-invalid={error ? "true" : undefined}
        />
      );
      break;
    case "boolean":
      input = (
        <label className="flex items-center gap-2 text-sm">
          <input
            id={id}
            type="checkbox"
            checked={value === true}
            onChange={(e) => onChange(e.target.checked)}
          />
          <span>{schema.label}</span>
        </label>
      );
      break;
    case "enum":
      input = (
        <select
          id={id}
          value={typeof value === "string" ? value : ""}
          onChange={(e) => onChange(e.target.value === "" ? null : e.target.value)}
          className={cls}
          aria-invalid={error ? "true" : undefined}
        >
          <option value="">—</option>
          {schema.options?.map((opt) => (
            <option key={opt} value={opt}>
              {opt}
            </option>
          ))}
        </select>
      );
      break;
  }

  return (
    <div data-testid={`field-${schema.name}`}>
      {schema.type !== "boolean" ? (
        <label htmlFor={id} className="mb-1 block text-xs font-medium text-neutral-700">
          {schema.label}
          {schema.required ? <span className="ml-1 text-rose-500">*</span> : null}
        </label>
      ) : null}
      {input}
      {schema.help ? (
        <p className="mt-1 text-xs text-neutral-500">{schema.help}</p>
      ) : null}
      {error ? (
        <p
          className="mt-1 text-xs text-rose-600"
          role="alert"
          data-testid={`error-${schema.name}`}
        >
          {error}
        </p>
      ) : null}
      {refs.length > 0 ? <ReferenceChips refs={refs} /> : null}
    </div>
  );
}

function ReferenceChips({ refs }: { refs: readonly ExpressionReference[] }): JSX.Element {
  return (
    <ul
      className="mt-1 flex flex-wrap gap-1"
      data-testid="refs"
      aria-label="Références détectées"
    >
      {refs.map((r, i) => (
        <li
          key={`${r.raw}-${i}`}
          className="rounded-full bg-sky-50 px-2 py-0.5 text-[10px] font-medium text-sky-700"
        >
          {r.path ? `${r.nodeId}.${r.path}` : r.nodeId}
        </li>
      ))}
    </ul>
  );
}

// ─────────────── Credentials (F8.4.d) ───────────────

function CredentialField({
  provider,
  value,
  onChange,
  fetchImpl,
}: {
  provider: string;
  value: string;
  onChange: (id: string) => void;
  fetchImpl: typeof fetchCredentials;
}): JSX.Element {
  const [creds, setCreds] = useState<readonly Credential[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    fetchImpl(provider).then((items) => {
      if (cancelled) return;
      setCreds(items);
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [provider, fetchImpl]);

  return (
    <div data-testid="credential-field">
      <label
        htmlFor="param-credential_id"
        className="mb-1 block text-xs font-medium text-neutral-700"
      >
        Credential ({provider})
      </label>
      {loading ? (
        <p className="text-xs text-neutral-500" role="status">
          Chargement des credentials…
        </p>
      ) : creds.length === 0 ? (
        <p className="text-xs text-neutral-500" role="status">
          Aucun credential disponible pour « {provider} ».
        </p>
      ) : (
        <select
          id="param-credential_id"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="w-full rounded-md border border-neutral-200 bg-white px-3 py-1.5 text-sm outline-none focus:border-sky-400"
        >
          <option value="">—</option>
          {creds.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      )}
    </div>
  );
}

function initialValues(node: CanvasNode, schema: NodeSchema): ParamValues {
  const existing =
    typeof node.data.params === "object" && node.data.params !== null
      ? (node.data.params as ParamValues)
      : {};
  return { ...defaultParams(schema), ...existing };
}
