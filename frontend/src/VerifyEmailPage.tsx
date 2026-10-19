// Page de vérification de l'adresse email (#231).
//
// Ouverte par le lien reçu par email (`…/#verify=<jeton>`) : le jeton est
// envoyé au serveur dès l'affichage, sans autre action. En cas de succès,
// retour à la connexion (`onDone`) — le lien n'ouvre pas de session. Un lien
// invalide ou expiré est expliqué : se connecter permet d'en recevoir un
// nouveau. Découplée du réseau : reçoit `verifyEmail` en props.

import { useEffect, useRef, useState } from "react";

export interface VerifyEmailPageProps {
  /** Jeton lu dans le fragment du lien. */
  token: string;
  verifyEmail: (token: string) => Promise<void>;
  /** Appelé après une vérification réussie. */
  onDone: () => void;
  /** Retour à la connexion après un échec. */
  onCancel: () => void;
}

export function VerifyEmailPage({
  token,
  verifyEmail,
  onDone,
  onCancel,
}: VerifyEmailPageProps): JSX.Element {
  const [error, setError] = useState<string | null>(null);
  // Un seul envoi par jeton (le double montage du mode strict de React
  // rejouerait l'effet).
  const sent = useRef<string | null>(null);

  useEffect(() => {
    if (sent.current === token) return;
    sent.current = token;
    verifyEmail(token).then(onDone, (err: unknown) =>
      setError(err instanceof Error ? err.message : "Échec de la vérification."),
    );
  }, [token, verifyEmail, onDone]);

  return (
    <main className="flex min-h-screen items-center justify-center bg-neutral-50 px-4">
      <section
        aria-label="Vérification de l'email"
        className="w-full max-w-sm rounded-xl border border-neutral-200 bg-white p-8 shadow-sm"
      >
        <h1 className="text-lg font-semibold text-neutral-900">
          Vérification de votre adresse
        </h1>
        {error ? (
          <>
            <p
              role="alert"
              className="mt-4 rounded-md bg-rose-50 px-3 py-2 text-sm text-rose-700"
            >
              {error}
            </p>
            <p className="mt-4 text-center text-sm text-neutral-500">
              <button
                type="button"
                onClick={onCancel}
                className="font-medium text-sky-700 hover:underline"
              >
                Retour à la connexion
              </button>
            </p>
          </>
        ) : (
          <p role="status" className="mt-4 text-sm text-neutral-500">
            Vérification en cours…
          </p>
        )}
      </section>
    </main>
  );
}
