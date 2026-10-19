// Onglet « Compte » (#196) : changement du mot de passe, double
// authentification (#245) et suppression du compte (#229, RGPD).
//
// Changer son mot de passe ferme toutes les sessions côté serveur (autres
// appareils compris) ; l'appareil courant reçoit une nouvelle paire de jetons
// et reste connecté. Supprimer son compte efface définitivement le compte et
// ses données, après le mot de passe actuel et l'email retapé en
// confirmation ; la session locale est ensuite fermée. Les actions réseau
// sont injectées (`changePassword` / `deleteAccount` de `useSession`) pour
// les tests.

import { useState, type FormEvent } from "react";

import { validatePasswordChange } from "./auth";
import { MfaSection, type MfaActions } from "./MfaSection";

export interface AccountPanelProps {
  email?: string;
  changePassword: (currentPassword: string, newPassword: string) => Promise<void>;
  /** Suppression du compte ; absente ⇒ section masquée. */
  deleteAccount?: (currentPassword: string) => Promise<void>;
  /** Double authentification (#245) ; absente ⇒ section masquée. */
  mfa?: MfaActions;
}

/** Saisie de confirmation quand l'email n'est pas connu. */
export const DELETE_CONFIRMATION_WORD = "SUPPRIMER";

type Outcome =
  | { kind: "idle" }
  | { kind: "busy" }
  | { kind: "done" }
  | { kind: "error"; message: string };

export function AccountPanel({
  email,
  changePassword,
  deleteAccount,
  mfa,
}: AccountPanelProps): JSX.Element {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [outcome, setOutcome] = useState<Outcome>({ kind: "idle" });

  async function onSubmit(e: FormEvent<HTMLFormElement>): Promise<void> {
    e.preventDefault();
    const invalid = validatePasswordChange(current, next, confirm);
    if (invalid) {
      setOutcome({ kind: "error", message: invalid });
      return;
    }
    setOutcome({ kind: "busy" });
    try {
      await changePassword(current, next);
      setCurrent("");
      setNext("");
      setConfirm("");
      setOutcome({ kind: "done" });
    } catch (err) {
      setOutcome({ kind: "error", message: (err as Error).message });
    }
  }

  return (
    <section aria-labelledby="account-title" className="mx-auto w-full max-w-2xl p-6">
      <header className="mb-4">
        <h2 id="account-title" className="text-lg font-semibold text-neutral-900">
          Compte
        </h2>
        {email && <p className="mt-1 text-sm text-neutral-500">{email}</p>}
      </header>

      <form
        aria-label="Changer le mot de passe"
        onSubmit={(e) => void onSubmit(e)}
        className="space-y-4 rounded-lg border border-neutral-200 bg-white p-4"
      >
        <h3 className="text-sm font-semibold text-neutral-900">Changer le mot de passe</h3>
        <p className="text-sm text-neutral-500">
          Vos autres appareils seront déconnectés ; celui-ci reste connecté.
        </p>
        <PasswordField
          id="current-password"
          label="Mot de passe actuel"
          autoComplete="current-password"
          value={current}
          onChange={setCurrent}
        />
        <PasswordField
          id="new-password"
          label="Nouveau mot de passe"
          autoComplete="new-password"
          value={next}
          onChange={setNext}
        />
        <PasswordField
          id="confirm-password"
          label="Confirmer le nouveau mot de passe"
          autoComplete="new-password"
          value={confirm}
          onChange={setConfirm}
        />
        {outcome.kind === "error" && (
          <p role="alert" className="text-sm text-rose-700">
            {outcome.message}
          </p>
        )}
        {outcome.kind === "done" && (
          <p role="status" className="rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-800">
            Mot de passe changé. Vos autres appareils ont été déconnectés.
          </p>
        )}
        <button
          type="submit"
          disabled={outcome.kind === "busy"}
          className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-700 disabled:opacity-60"
        >
          {outcome.kind === "busy" ? "Changement…" : "Changer le mot de passe"}
        </button>
      </form>

      {mfa && <MfaSection actions={mfa} />}

      {deleteAccount && <DeleteAccountForm email={email} deleteAccount={deleteAccount} />}
    </section>
  );
}

function DeleteAccountForm({
  email,
  deleteAccount,
}: {
  email: string | undefined;
  deleteAccount: (currentPassword: string) => Promise<void>;
}): JSX.Element {
  const [password, setPassword] = useState("");
  const [typed, setTyped] = useState("");
  const [outcome, setOutcome] = useState<Outcome>({ kind: "idle" });
  const expected = email ?? DELETE_CONFIRMATION_WORD;
  const confirmed = typed.trim().toLowerCase() === expected.toLowerCase();

  async function onSubmit(e: FormEvent<HTMLFormElement>): Promise<void> {
    e.preventDefault();
    if (!password || !confirmed) return;
    setOutcome({ kind: "busy" });
    try {
      // En cas de succès, la session est fermée et ce panneau démonté.
      await deleteAccount(password);
    } catch (err) {
      setOutcome({ kind: "error", message: (err as Error).message });
    }
  }

  return (
    <form
      aria-label="Supprimer mon compte"
      onSubmit={(e) => void onSubmit(e)}
      className="mt-6 space-y-4 rounded-lg border border-rose-200 bg-white p-4"
    >
      <h3 className="text-sm font-semibold text-rose-800">Supprimer mon compte</h3>
      <div className="text-sm text-neutral-600">
        <p>La suppression est définitive. Seront effacés :</p>
        <ul className="mt-1 list-disc pl-5">
          <li>votre compte, votre email et votre mot de passe ;</li>
          <li>vos workflows, leurs URL webhook et leurs planifications ;</li>
          <li>l'historique de vos exécutions et leurs résultats ;</li>
          <li>vos connexions aux services (Slack, Gmail, Notion, GitHub…).</li>
        </ul>
        <p className="mt-2">
          Le journal d'audit est conservé, mais ne vous désigne plus que par un identifiant
          anonyme. Tous vos appareils seront déconnectés.
        </p>
      </div>
      <PasswordField
        id="delete-password"
        label="Votre mot de passe"
        autoComplete="current-password"
        value={password}
        onChange={setPassword}
      />
      <div>
        <label htmlFor="delete-confirm" className="block text-sm font-medium text-neutral-700">
          Saisissez « {expected} » pour confirmer
        </label>
        <input
          id="delete-confirm"
          name="delete-confirm"
          type="text"
          autoComplete="off"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-rose-500 focus:ring-1 focus:ring-rose-500"
        />
      </div>
      {outcome.kind === "error" && (
        <p role="alert" className="text-sm text-rose-700">
          {outcome.message}
        </p>
      )}
      <button
        type="submit"
        disabled={outcome.kind === "busy" || !confirmed || !password}
        className="rounded-md bg-rose-600 px-4 py-2 text-sm font-medium text-white hover:bg-rose-700 disabled:opacity-60"
      >
        {outcome.kind === "busy" ? "Suppression…" : "Supprimer définitivement mon compte"}
      </button>
    </form>
  );
}

function PasswordField({
  id,
  label,
  autoComplete,
  value,
  onChange,
}: {
  id: string;
  label: string;
  autoComplete: string;
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
        type="password"
        autoComplete={autoComplete}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
      />
    </div>
  );
}
