// Onglet « Compte » (#196) : changement du mot de passe.
//
// Changer son mot de passe ferme toutes les sessions côté serveur (autres
// appareils compris) ; l'appareil courant reçoit une nouvelle paire de jetons
// et reste connecté. L'action réseau est injectée (`changePassword` de
// `useSession`) pour les tests.

import { useState, type FormEvent } from "react";

import { validatePasswordChange } from "./auth";

export interface AccountPanelProps {
  email?: string;
  changePassword: (currentPassword: string, newPassword: string) => Promise<void>;
}

type Outcome =
  | { kind: "idle" }
  | { kind: "busy" }
  | { kind: "done" }
  | { kind: "error"; message: string };

export function AccountPanel({ email, changePassword }: AccountPanelProps): JSX.Element {
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
    </section>
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
