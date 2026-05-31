// Page de connexion / inscription (F8.6.a).
//
// Formulaire unique avec bascule login ↔ signup. La validation client
// (`validateCredentials`) évite les allers-retours inutiles ; les erreurs
// serveur (401/409/…) remontent via `ApiError.message`. Découplé du hook :
// reçoit `login`/`signup` en props pour rester testable isolément.

import { useState, type FormEvent } from "react";

import { validateCredentials, type Credentials } from "./auth";

type Mode = "login" | "signup";

export interface LoginPageProps {
  login: (creds: Credentials) => Promise<void>;
  signup: (creds: Credentials) => Promise<void>;
  /** Mode initial du formulaire. */
  initialMode?: Mode;
}

export function LoginPage({
  login,
  signup,
  initialMode = "login",
}: LoginPageProps): JSX.Element {
  const [mode, setMode] = useState<Mode>(initialMode);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const isSignup = mode === "signup";

  async function handleSubmit(e: FormEvent): Promise<void> {
    e.preventDefault();
    setError(null);
    const creds: Credentials = { email, password };
    const clientErrors = validateCredentials(creds);
    if (clientErrors.length > 0) {
      setError(clientErrors.join(" "));
      return;
    }
    setSubmitting(true);
    try {
      await (isSignup ? signup(creds) : login(creds));
      // Succès : le parent (App) re-rend hors de la page de login.
    } catch (err) {
      setError(err instanceof Error ? err.message : "Échec de l'authentification.");
    } finally {
      setSubmitting(false);
    }
  }

  function switchMode(): void {
    setMode(isSignup ? "login" : "signup");
    setError(null);
  }

  return (
    <main className="flex min-h-screen items-center justify-center bg-neutral-50 px-4">
      <form
        onSubmit={handleSubmit}
        noValidate
        aria-label={isSignup ? "Inscription" : "Connexion"}
        className="w-full max-w-sm rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
      >
        <h1 className="text-lg font-semibold text-neutral-900">
          {isSignup ? "Créer un compte" : "Se connecter"}
        </h1>
        <p className="mt-1 text-sm text-neutral-500">
          {isSignup
            ? "Rejoignez Autome pour orchestrer vos workflows."
            : "Accédez à vos workflows Autome."}
        </p>

        <div className="mt-6 space-y-4">
          <Field
            id="email"
            label="Email"
            type="email"
            autoComplete="email"
            value={email}
            onChange={setEmail}
          />
          <Field
            id="password"
            label="Mot de passe"
            type="password"
            autoComplete={isSignup ? "new-password" : "current-password"}
            value={password}
            onChange={setPassword}
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
          {submitting
            ? "Veuillez patienter…"
            : isSignup
              ? "Créer le compte"
              : "Se connecter"}
        </button>

        <p className="mt-4 text-center text-sm text-neutral-500">
          {isSignup ? "Déjà un compte ? " : "Pas encore de compte ? "}
          <button
            type="button"
            onClick={switchMode}
            className="font-medium text-sky-700 hover:underline"
          >
            {isSignup ? "Se connecter" : "Créer un compte"}
          </button>
        </p>
      </form>
    </main>
  );
}

function Field({
  id,
  label,
  type,
  value,
  autoComplete,
  onChange,
}: {
  id: string;
  label: string;
  type: string;
  value: string;
  autoComplete: string;
  onChange: (v: string) => void;
}): JSX.Element {
  return (
    <div>
      <label
        htmlFor={id}
        className="block text-sm font-medium text-neutral-700"
      >
        {label}
      </label>
      <input
        id={id}
        name={id}
        type={type}
        autoComplete={autoComplete}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="mt-1 w-full rounded-md border border-neutral-300 px-3 py-2 text-sm outline-none focus:border-sky-500 focus:ring-1 focus:ring-sky-500"
      />
    </div>
  );
}
