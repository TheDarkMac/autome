// Page d'acceptation d'une invitation (#241).
//
// Ouverte par le lien reçu par email (`…/#invite=<jeton>`) : la personne
// invitée choisit son mot de passe (saisi deux fois), ce qui active le compte
// créé par un administrateur et ouvre directement sa session (`onDone`).
// Découplée du réseau : reçoit `acceptInvitation` en props.

import { MIN_PASSWORD_LENGTH } from "./auth";
import { ChoosePasswordForm } from "./ResetPasswordPage";

export interface AcceptInvitationPageProps {
  /** Jeton lu dans le fragment du lien. */
  token: string;
  acceptInvitation: (token: string, password: string) => Promise<void>;
  /** Appelé une fois le compte activé et la session ouverte. */
  onDone: () => void;
  /** Abandon (retour à la connexion sans rien changer). */
  onCancel: () => void;
}

export function AcceptInvitationPage({
  token,
  acceptInvitation,
  onDone,
  onCancel,
}: AcceptInvitationPageProps): JSX.Element {
  return (
    <ChoosePasswordForm
      label="Activation du compte"
      title="Bienvenue sur Autome"
      intro={`Un administrateur vous a invité. Choisissez votre mot de passe (au moins ${MIN_PASSWORD_LENGTH} caractères) pour activer votre compte.`}
      submitLabel="Activer mon compte"
      failure="Échec de l'activation."
      submit={(password) => acceptInvitation(token, password)}
      onDone={onDone}
      onCancel={onCancel}
    />
  );
}
