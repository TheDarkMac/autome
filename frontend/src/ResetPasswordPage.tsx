// Page de réinitialisation du mot de passe (#204).
//
// Ouverte par le lien reçu par email (`…/#reset=<jeton>`). L'utilisateur
// choisit un nouveau mot de passe (saisi deux fois) ; le serveur ferme alors
// toutes ses sessions, d'où le retour à la page de connexion (`onDone`).
// Découplée du réseau : reçoit `resetPassword` en props.

import { useState, type FormEvent } from "react";

import { MIN_PASSWORD_LENGTH, validatePasswordReset } from "./auth";

export interface ResetPasswordPageProps {
  /** Jeton lu dans le fragment du lien. */
  token: string;
  resetPassword: (token: string, newPassword: string) => Promise<void>;
  /** Appelé après une réinitialisation réussie. */
  onDone: () => void;
  /** Abandon (retour à la connexion sans rien changer). */
  onCancel: () => void;
}

export function ResetPasswordPage({
  token,
  resetPassword,
  onDone,
  onCancel,
}: ResetPasswordPageProps): JSX.Element {
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function handleSubmit(e: FormEvent): Promise<void> {
    e.preventDefault();
    const invalid = validatePasswordReset(next, confirm);
    if (invalid) {
      setError(invalid);
      return;
    }
    setError(null);
    setSubmitting(true);
    try {
      await resetPassword(token, next);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Échec de la réinitialisation.");
      setSubmitting(false);
    }
  }

  return (
    <main className="flex min-h-screen items-center justify-center bg-neutral-50 px-4">
      <form
        onSubmit={handleSubmit}
        noValidate
        aria-label="Réinitialisation du mot de passe"
        className="w-full max-w-sm rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
      >
        <h1 className="text-lg font-semibold text-neutral-900">
          Choisir un nouveau mot de passe
        </h1>
        <p className="mt-1 text-sm text-neutral-500">
          Au moins {MIN_PASSWORD_LENGTH} caractères. Toutes vos sessions
          ouvertes seront fermées.
        </p>

        <div className="mt-6 space-y-4">
          <PasswordField
            id="new-password"
            label="Nouveau mot de passe"
            value={next}
            onChange={setNext}
          />
          <PasswordField
            id="confirm-password"
            label="Confirmation"
            value={confirm}
            onChange={setConfirm}
          />
        </div>

        {error && (
          <p
            role="alert"
            className="mt-4 rounded-md bg-rose-50 px-3 py-2 text-sm text-rose-700"
          >
            {error}
          </p>
        )}

        <button
          type="submit"
          disabled={submitting}
          className="mt-6 w-full rounded-md bg-neutral-900 px-4 py-2 text-sm font-medium text-white transition hover:bg-neutral-700 disabled:opacity-50"
        >
          {submitting ? "Veuillez patienter…" : "Enregistrer le mot de passe"}
        </button>

        <p className="mt-4 text-center text-sm text-neutral-500">
          <button
            type="button"
            onClick={onCancel}
            className="font-medium text-sky-700 hover:underline"
          >
            Retour à la connexion
          </button>
        </p>
      </form>
    </main>
  );
}

function PasswordField({
  id,
  label,
  value,
  onChange,
}: {
  id: string;
  label: string;
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
        autoComplete="new-password"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
      />
    </div>
  );
}
