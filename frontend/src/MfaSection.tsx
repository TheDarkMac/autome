// Section « Double authentification » de l'onglet Compte (#245).
//
// Parcours d'activation : mot de passe actuel ⇒ QR code (et secret à saisir
// à la main) ⇒ premier code de l'application ⇒ codes de secours, montrés une
// seule fois. L'activation ferme les autres sessions (la courante continue).
// Une fois active : nombre de codes de secours restants, régénération (code
// exigé) et désactivation (mot de passe et code exigés). Les appels réseau
// sont injectés (`MfaActions`) pour les tests.

import { useCallback, useEffect, useState, type FormEvent } from "react";

import type { MfaSetup, MfaStatus } from "./mfa";

/** Opérations du second facteur, adossées à la session courante. */
export interface MfaActions {
  status(): Promise<MfaStatus>;
  setup(currentPassword: string): Promise<MfaSetup>;
  /** Active et renvoie les codes de secours (la session est renouvelée). */
  enable(code: string): Promise<string[]>;
  disable(currentPassword: string, code: string): Promise<void>;
  regenerate(code: string): Promise<string[]>;
}

/** Sous ce nombre de codes de secours restants, l'interface alerte. */
export const LOW_RECOVERY_CODES = 3;

type View =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "off" }
  | { kind: "password" }
  | { kind: "scan"; setup: MfaSetup }
  | { kind: "codes"; codes: string[]; fresh: boolean }
  | { kind: "on"; status: MfaStatus };

export function MfaSection({ actions }: { actions: MfaActions }): JSX.Element {
  const [view, setView] = useState<View>({ kind: "loading" });

  const reload = useCallback(async () => {
    try {
      const status = await actions.status();
      setView(status.enabled ? { kind: "on", status } : { kind: "off" });
    } catch (err) {
      setView({ kind: "error", message: (err as Error).message });
    }
  }, [actions]);

  useEffect(() => {
    void reload();
  }, [reload]);

  return (
    <section
      aria-labelledby="mfa-title"
      className="mt-6 space-y-4 rounded-lg border border-neutral-200 bg-white p-4"
    >
      <div className="flex items-center justify-between">
        <h3 id="mfa-title" className="text-sm font-semibold text-neutral-900">
          Double authentification
        </h3>
        {view.kind === "on" && (
          <span className="rounded-full bg-emerald-50 px-2 py-0.5 text-xs font-medium text-emerald-800">
            Active
          </span>
        )}
      </div>

      {view.kind === "loading" && <p className="text-sm text-neutral-500">Chargement…</p>}
      {view.kind === "error" && (
        <p role="alert" className="text-sm text-rose-700">
          {view.message}
        </p>
      )}

      {view.kind === "off" && (
        <>
          <p className="text-sm text-neutral-600">
            En plus du mot de passe, la connexion demandera un code à 6 chiffres affiché par une
            application d'authentification (Google Authenticator, Microsoft Authenticator, Aegis,
            1Password…). Un mot de passe deviné ou divulgué ne suffira plus.
          </p>
          <button
            type="button"
            onClick={() => setView({ kind: "password" })}
            className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-700"
          >
            Activer la double authentification
          </button>
        </>
      )}

      {view.kind === "password" && (
        <PasswordStep
          onCancel={() => setView({ kind: "off" })}
          onSubmit={async (password) => {
            setView({ kind: "scan", setup: await actions.setup(password) });
          }}
        />
      )}

      {view.kind === "scan" && (
        <ScanStep
          setup={view.setup}
          onCancel={() => setView({ kind: "off" })}
          onSubmit={async (code) => {
            setView({ kind: "codes", codes: await actions.enable(code), fresh: true });
          }}
        />
      )}

      {view.kind === "codes" && (
        <RecoveryCodesView
          codes={view.codes}
          fresh={view.fresh}
          onDone={() => {
            setView({ kind: "loading" });
            void reload();
          }}
        />
      )}

      {view.kind === "on" && (
        <EnabledView
          status={view.status}
          regenerate={async (code) => {
            setView({ kind: "codes", codes: await actions.regenerate(code), fresh: false });
          }}
          disable={async (password, code) => {
            await actions.disable(password, code);
            setView({ kind: "off" });
          }}
        />
      )}
    </section>
  );
}

/** Formulaire générique : soumission asynchrone, erreur affichée. */
function useSubmit(
  run: () => Promise<void>,
): [busy: boolean, error: string | null, submit: (e: FormEvent) => void] {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = (e: FormEvent): void => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    run()
      .catch((err: unknown) => setError((err as Error).message))
      .finally(() => setBusy(false));
  };
  return [busy, error, submit];
}

function PasswordStep({
  onSubmit,
  onCancel,
}: {
  onSubmit: (password: string) => Promise<void>;
  onCancel: () => void;
}): JSX.Element {
  const [password, setPassword] = useState("");
  const [busy, error, submit] = useSubmit(() => onSubmit(password));
  return (
    <form aria-label="Confirmer le mot de passe" onSubmit={submit} className="space-y-3">
      <TextField
        id="mfa-password"
        label="Mot de passe actuel"
        type="password"
        autoComplete="current-password"
        value={password}
        onChange={setPassword}
      />
      <ErrorLine error={error} />
      <Buttons busy={busy} label="Continuer" busyLabel="Vérification…" onCancel={onCancel} />
    </form>
  );
}

function ScanStep({
  setup,
  onSubmit,
  onCancel,
}: {
  setup: MfaSetup;
  onSubmit: (code: string) => Promise<void>;
  onCancel: () => void;
}): JSX.Element {
  const [code, setCode] = useState("");
  const [busy, error, submit] = useSubmit(() => onSubmit(code));
  // SVG produit par le serveur, affiché comme image : aucun script exécuté.
  const qrSrc = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(setup.qrSvg)}`;
  const grouped = setup.secret.replace(/(.{4})/g, "$1 ").trim();
  return (
    <form aria-label="Configurer l'application" onSubmit={submit} className="space-y-3">
      <ol className="list-decimal space-y-1 pl-5 text-sm text-neutral-600">
        <li>Scannez ce QR code avec votre application d'authentification.</li>
        <li>Saisissez le code à 6 chiffres qu'elle affiche.</li>
      </ol>
      {setup.qrSvg && (
        <img
          src={qrSrc}
          alt="QR code à scanner avec votre application d'authentification"
          width={200}
          height={200}
          className="rounded border border-neutral-200"
        />
      )}
      <p className="text-sm text-neutral-600">
        Impossible de scanner ? Saisissez cette clé dans l'application :{" "}
        <code data-testid="mfa-secret" className="break-all font-mono text-neutral-900">
          {grouped}
        </code>
      </p>
      <TextField
        id="mfa-enable-code"
        label="Code de l'application"
        type="text"
        autoComplete="one-time-code"
        inputMode="numeric"
        value={code}
        onChange={setCode}
      />
      <ErrorLine error={error} />
      <Buttons busy={busy} label="Activer" busyLabel="Activation…" onCancel={onCancel} />
    </form>
  );
}

function RecoveryCodesView({
  codes,
  fresh,
  onDone,
}: {
  codes: string[];
  fresh: boolean;
  onDone: () => void;
}): JSX.Element {
  const [kept, setKept] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div className="space-y-3">
      {fresh && (
        <p role="status" className="rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-800">
          Double authentification activée. Vos autres appareils ont été déconnectés.
        </p>
      )}
      <p className="text-sm text-neutral-600">
        Conservez ces codes de secours en lieu sûr (gestionnaire de mots de passe, papier). Chacun
        permet de se connecter une fois sans l'application. Ils ne seront plus affichés.
      </p>
      <ul
        aria-label="Codes de secours"
        className="grid grid-cols-2 gap-1 rounded-md bg-neutral-50 p-3 font-mono text-sm text-neutral-900"
      >
        {codes.map((code) => (
          <li key={code}>{code}</li>
        ))}
      </ul>
      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={() => {
            // Presse-papiers absent (contexte non sécurisé) : rien à faire.
            try {
              navigator.clipboard.writeText(codes.join("\n")).then(
                () => setCopied(true),
                () => undefined,
              );
            } catch {
              /* copie manuelle */
            }
          }}
          className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm text-neutral-700 hover:bg-neutral-50"
        >
          {copied ? "Copiés" : "Copier les codes"}
        </button>
      </div>
      <label className="flex items-center gap-2 text-sm text-neutral-700">
        <input type="checkbox" checked={kept} onChange={(e) => setKept(e.target.checked)} />
        J'ai conservé mes codes de secours
      </label>
      <button
        type="button"
        disabled={!kept}
        onClick={onDone}
        className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-700 disabled:opacity-60"
      >
        Terminé
      </button>
    </div>
  );
}

function EnabledView({
  status,
  regenerate,
  disable,
}: {
  status: MfaStatus;
  regenerate: (code: string) => Promise<void>;
  disable: (password: string, code: string) => Promise<void>;
}): JSX.Element {
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [disableCode, setDisableCode] = useState("");
  const [regenBusy, regenError, submitRegen] = useSubmit(() => regenerate(code));
  const [offBusy, offError, submitOff] = useSubmit(() => disable(password, disableCode));
  const low = status.recoveryCodesLeft <= LOW_RECOVERY_CODES;
  return (
    <div className="space-y-4">
      <p className={low ? "text-sm text-amber-800" : "text-sm text-neutral-600"}>
        {status.recoveryCodesLeft === 0
          ? "Aucun code de secours restant : générez-en de nouveaux."
          : `${status.recoveryCodesLeft} code${status.recoveryCodesLeft > 1 ? "s" : ""} de secours restant${status.recoveryCodesLeft > 1 ? "s" : ""}.`}
      </p>

      <form aria-label="Nouveaux codes de secours" onSubmit={submitRegen} className="space-y-3">
        <h4 className="text-sm font-medium text-neutral-900">Nouveaux codes de secours</h4>
        <p className="text-sm text-neutral-500">Les codes actuels cesseront de fonctionner.</p>
        <TextField
          id="mfa-regen-code"
          label="Code de l'application"
          type="text"
          autoComplete="one-time-code"
          value={code}
          onChange={setCode}
        />
        <ErrorLine error={regenError} />
        <Buttons busy={regenBusy} label="Générer" busyLabel="Génération…" />
      </form>

      <form
        aria-label="Désactiver la double authentification"
        onSubmit={submitOff}
        className="space-y-3 border-t border-neutral-200 pt-4"
      >
        <h4 className="text-sm font-medium text-neutral-900">Désactiver</h4>
        <TextField
          id="mfa-disable-password"
          label="Mot de passe actuel"
          type="password"
          autoComplete="current-password"
          value={password}
          onChange={setPassword}
        />
        <TextField
          id="mfa-disable-code"
          label="Code de l'application ou code de secours"
          type="text"
          autoComplete="one-time-code"
          value={disableCode}
          onChange={setDisableCode}
        />
        <ErrorLine error={offError} />
        <Buttons
          busy={offBusy}
          label="Désactiver la double authentification"
          busyLabel="Désactivation…"
          danger
        />
      </form>
    </div>
  );
}

function TextField({
  id,
  label,
  type,
  autoComplete,
  inputMode,
  value,
  onChange,
}: {
  id: string;
  label: string;
  type: "text" | "password";
  autoComplete: string;
  inputMode?: "numeric" | "text";
  value: string;
  onChange: (v: string) => void;
}): JSX.Element {
  return (
    <div>
      <label htmlFor={id} className="block text-sm font-medium text-neutral-700">
        {label}
      </label>
      <input
        id={id}
        name={id}
        type={type}
        autoComplete={autoComplete}
        {...(inputMode ? { inputMode } : {})}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
      />
    </div>
  );
}

function ErrorLine({ error }: { error: string | null }): JSX.Element | null {
  if (!error) return null;
  return (
    <p role="alert" className="text-sm text-rose-700">
      {error}
    </p>
  );
}

function Buttons({
  busy,
  label,
  busyLabel,
  onCancel,
  danger = false,
}: {
  busy: boolean;
  label: string;
  busyLabel: string;
  onCancel?: () => void;
  danger?: boolean;
}): JSX.Element {
  return (
    <div className="flex items-center gap-3">
      <button
        type="submit"
        disabled={busy}
        className={`rounded-md px-4 py-2 text-sm font-medium text-white disabled:opacity-60 ${
          danger ? "bg-rose-600 hover:bg-rose-700" : "bg-sky-600 hover:bg-sky-700"
        }`}
      >
        {busy ? busyLabel : label}
      </button>
      {onCancel && (
        <button
          type="button"
          onClick={onCancel}
          className="text-sm font-medium text-neutral-600 hover:underline"
        >
          Annuler
        </button>
      )}
    </div>
  );
}
