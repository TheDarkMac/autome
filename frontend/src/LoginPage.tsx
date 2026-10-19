// Page de connexion / inscription (F8.6.a), demande de lien de
// réinitialisation du mot de passe (#204) et attente de la vérification de
// l'adresse (#231).
//
// Formulaire unique avec bascule login ↔ signup ↔ « mot de passe oublié ».
// Une inscription qui exige la vérification de l'adresse, ou une connexion
// refusée faute de vérification, mène à l'écran « vérifiez votre boîte »
// (renvoi du lien possible).
// La validation client (`validateCredentials`) évite les allers-retours
// inutiles ; les erreurs serveur (401/409/…) remontent via
// `ApiError.message`. Découplé du hook : reçoit `login`/`signup` (et
// `requestPasswordReset`) en props pour rester testable isolément.

import { useState, type FormEvent } from "react";

import {
  EmailNotVerifiedError,
  validateCredentials,
  type Credentials,
} from "./auth";
import type { PendingVerification } from "./useSession";

type Mode = "login" | "signup" | "forgot" | "verify";

/** Pourquoi l'écran de vérification est affiché. */
type VerifyReason = "signup" | "login";

export interface LoginPageProps {
  login: (creds: Credentials) => Promise<void>;
  /** Inscription ; une adresse renvoyée ⇒ à vérifier avant connexion (#231). */
  signup: (creds: Credentials) => Promise<PendingVerification | null | void>;
  /** Renvoi du lien de vérification (#231) ; absent ⇒ pas de bouton. */
  resendVerification?: (email: string) => Promise<void>;
  /** Demande d'un lien de réinitialisation ; absent ⇒ pas de lien « oublié ». */
  requestPasswordReset?: (email: string) => Promise<void>;
  /** Information affichée au-dessus du formulaire (ex. après réinitialisation). */
  notice?: string | null;
  /** Mode initial du formulaire. */
  initialMode?: Mode;
}

/** Confirmation d'une demande : identique que le compte existe ou non. */
export const RESET_REQUESTED_MESSAGE =
  "Si un compte correspond à cette adresse, un email contenant un lien de réinitialisation vient d'être envoyé. Le lien est valable 30 minutes.";

/** Confirmation d'un renvoi : identique que le compte attende ou non (#231). */
export const VERIFICATION_RESENT_MESSAGE =
  "Si ce compte attend encore sa vérification, un nouveau lien vient d'être envoyé. Il est valable 24 heures.";

export function LoginPage({
  login,
  signup,
  resendVerification,
  requestPasswordReset,
  notice = null,
  initialMode = "login",
}: LoginPageProps): JSX.Element {
  const [mode, setMode] = useState<Mode>(initialMode);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [resetRequested, setResetRequested] = useState(false);
  const [pending, setPending] = useState<{ email: string; reason: VerifyReason } | null>(
    null,
  );

  const isSignup = mode === "signup";

  if (mode === "verify" && pending) {
    return (
      <VerifyEmailNotice
        email={pending.email}
        reason={pending.reason}
        resendVerification={resendVerification}
        onBack={() => {
          setMode("login");
          setPending(null);
          setPassword("");
          setError(null);
        }}
      />
    );
  }

  if (mode === "forgot" && requestPasswordReset) {
    return (
      <ForgotPasswordForm
        email={email}
        onEmailChange={setEmail}
        requested={resetRequested}
        requestPasswordReset={async (address) => {
          await requestPasswordReset(address);
          setResetRequested(true);
        }}
        onBack={() => {
          setMode("login");
          setResetRequested(false);
          setError(null);
        }}
      />
    );
  }

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
      if (isSignup) {
        const awaiting = await signup(creds);
        if (awaiting) {
          setPending({ email: awaiting.email, reason: "signup" });
          setMode("verify");
        }
      } else {
        await login(creds);
      }
      // Succès : le parent (App) re-rend hors de la page de login.
    } catch (err) {
      if (err instanceof EmailNotVerifiedError) {
        setPending({ email: err.email, reason: "login" });
        setMode("verify");
        return;
      }
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

        {notice && (
          <p
            role="status"
            className="mt-4 rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-800"
          >
            {notice}
          </p>
        )}

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
          {!isSignup && requestPasswordReset && (
            <button
              type="button"
              onClick={() => {
                setMode("forgot");
                setError(null);
              }}
              className="text-sm font-medium text-sky-700 hover:underline"
            >
              Mot de passe oublié ?
            </button>
          )}
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

/** Attente de la vérification de l'adresse, avec renvoi du lien (#231). */
function VerifyEmailNotice({
  email,
  reason,
  resendVerification,
  onBack,
}: {
  email: string;
  reason: VerifyReason;
  resendVerification?: ((email: string) => Promise<void>) | undefined;
  onBack: () => void;
}): JSX.Element {
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [resent, setResent] = useState(false);

  async function resend(): Promise<void> {
    if (!resendVerification) return;
    setError(null);
    setSending(true);
    try {
      await resendVerification(email);
      setResent(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Échec du renvoi.");
    } finally {
      setSending(false);
    }
  }

  return (
    <main className="flex min-h-screen items-center justify-center bg-neutral-50 px-4">
      <section
        aria-label="Vérification de l'email"
        className="w-full max-w-sm rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
      >
        <h1 className="text-lg font-semibold text-neutral-900">
          Vérifiez votre boîte email
        </h1>
        <p className="mt-3 text-sm text-neutral-600">
          {reason === "signup"
            ? "Votre compte est créé. "
            : "Votre adresse n'est pas encore vérifiée. "}
          Un lien de confirmation a été envoyé à{" "}
          <strong className="font-medium text-neutral-900">{email}</strong>.
          Ouvrez-le (il est valable 24 heures), puis connectez-vous.
        </p>

        {resent && (
          <p
            role="status"
            className="mt-4 rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-800"
          >
            {VERIFICATION_RESENT_MESSAGE}
          </p>
        )}
        {error && (
          <p
            role="alert"
            className="mt-4 rounded-md bg-rose-50 px-3 py-2 text-sm text-rose-700"
          >
            {error}
          </p>
        )}

        {resendVerification && (
          <button
            type="button"
            onClick={() => void resend()}
            disabled={sending}
            className="mt-6 w-full rounded-md border border-neutral-300 bg-white px-4 py-2 text-sm font-medium text-neutral-900 transition hover:bg-neutral-50 disabled:opacity-50"
          >
            {sending ? "Envoi…" : "Renvoyer le lien"}
          </button>
        )}

        <p className="mt-4 text-center text-sm text-neutral-500">
          <button
            type="button"
            onClick={onBack}
            className="font-medium text-sky-700 hover:underline"
          >
            Retour à la connexion
          </button>
        </p>
      </section>
    </main>
  );
}

/** Demande d'un lien de réinitialisation (#204). */
function ForgotPasswordForm({
  email,
  onEmailChange,
  requested,
  requestPasswordReset,
  onBack,
}: {
  email: string;
  onEmailChange: (v: string) => void;
  requested: boolean;
  requestPasswordReset: (email: string) => Promise<void>;
  onBack: () => void;
}): JSX.Element {
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function handleSubmit(e: FormEvent): Promise<void> {
    e.preventDefault();
    setError(null);
    setSubmitting(true);
    try {
      await requestPasswordReset(email);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Échec de la demande.");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <main className="flex min-h-screen items-center justify-center bg-neutral-50 px-4">
      <form
        onSubmit={handleSubmit}
        noValidate
        aria-label="Mot de passe oublié"
        className="w-full max-w-sm rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
      >
        <h1 className="text-lg font-semibold text-neutral-900">
          Mot de passe oublié
        </h1>
        <p className="mt-1 text-sm text-neutral-500">
          Recevez par email un lien pour choisir un nouveau mot de passe.
        </p>

        {requested ? (
          <p
            role="status"
            className="mt-6 rounded-md bg-emerald-50 px-3 py-2 text-sm text-emerald-800"
          >
            {RESET_REQUESTED_MESSAGE}
          </p>
        ) : (
          <>
            <div className="mt-6">
              <Field
                id="email"
                label="Email"
                type="email"
                autoComplete="email"
                value={email}
                onChange={onEmailChange}
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
              {submitting ? "Veuillez patienter…" : "Envoyer le lien"}
            </button>
          </>
        )}

        <p className="mt-4 text-center text-sm text-neutral-500">
          <button
            type="button"
            onClick={onBack}
            className="font-medium text-sky-700 hover:underline"
          >
            Retour à la connexion
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
