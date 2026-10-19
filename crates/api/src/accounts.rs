//! Cycle de vie des comptes : inscription, connexion, rotation des jetons
//! (backlog post-audit — auth P0).
//!
//! Endpoints REST/JSON, **publics** (ils émettent les jetons) :
//!
//! | Méthode | Path            | Body                    | Succès                      |
//! |---------|-----------------|-------------------------|-----------------------------|
//! | POST    | `/auth/signup`  | `{ email, password }`   | 201 + [`TokenResponse`], ou 202 (email à vérifier) |
//! | POST    | `/auth/login`   | `{ email, password }`   | 200 + [`TokenResponse`], ou 202 + [`MfaChallenge`] (second facteur, cf. [`crate::mfa`]) |
//! | POST    | `/auth/refresh` | `{ refresh_token }`     | 200 + [`TokenResponse`]     |
//! | POST    | `/auth/logout`  | `{ refresh_token }`     | 204                         |
//! | POST    | `/auth/logout-all` | — (Bearer requis)    | 204                         |
//! | POST    | `/auth/password` | `{ current_password, new_password }` (Bearer) | 200 + [`TokenResponse`] |
//! | POST    | `/auth/password/forgot` | `{ email }`      | 202                         |
//! | POST    | `/auth/password/reset`  | `{ token, new_password }` | 204               |
//! | POST    | `/auth/email/verify`    | `{ token }`      | 204                         |
//! | POST    | `/auth/email/resend`    | `{ email }`      | 202                         |
//! | POST    | `/auth/invitation/accept` | `{ token, password }` | 200 + [`TokenResponse`] |
//! | GET     | `/auth/me`      | — (Bearer requis)       | 200 + [`MeResponse`]        |
//! | DELETE  | `/auth/me`      | `{ current_password }` (Bearer) | 204                 |
//!
//! Contrat aligné sur le frontend (`frontend/src/auth.ts`) : 400 saisie
//! invalide, 401 identifiants/jeton refusés, 403 compte suspendu ou email non
//! vérifié (connexion avec le bon mot de passe) ou inscriptions fermées, 409
//! email déjà pris, 429 trop de tentatives (`Retry-After`), 503
//! réinitialisation, vérification ou invitation indisponible (aucun envoi
//! d'email configuré).
//!
//! Choix de sécurité :
//! - **Argon2id** ([`PasswordHasher`]) exécuté dans `spawn_blocking` : ~50 ms
//!   de CPU par hash ne doivent pas bloquer le runtime async.
//! - **Pas d'énumération de comptes** au login : email inconnu et mauvais mot
//!   de passe donnent la même réponse 401, et un email inconnu paie quand même
//!   une vérification Argon2 (contre un hash leurre) pour égaliser le temps.
//! - **Anti-bruteforce** ([`LoginThrottleStore`]) : au-delà de
//!   [`MAX_LOGIN_FAILURES`] tentatives sans succès sur un email dans la
//!   fenêtre, 429 jusqu'à sa fin. En mode `postgres`, le compteur est
//!   **partagé** par toutes les répliques et survit aux redémarrages (#182) ;
//!   une erreur du compteur refuse la connexion (fail closed).
//! - **Anti-bruteforce par IP** ([`IpThrottle`], #190) : le compteur par email
//!   ne voit ni la pulvérisation de mots de passe (quelques essais sur beaucoup
//!   d'emails), ni le bourrage d'identifiants (un essai par email), ni
//!   l'épuisement CPU (Argon2 sur des emails aléatoires). Par adresse IP
//!   cliente : échecs de connexion (une connexion réussie rend sa tentative)
//!   et tentatives d'inscription, chacun avec son quota. L'IP n'est ni stockée
//!   ni journalisée : le compteur est indexé par un HMAC à clé secrète
//!   (cf. [`crate::client_ip`]).
//! - Le refresh réutilise [`autome_core::TokenService::rotate`] : le refresh
//!   présenté est révoqué, un rejeu est refusé — y compris entre requêtes
//!   concurrentes (révocation atomique `revoke_once`, #184).
//! - Le logout (#184) révoque côté serveur le refresh présenté et, s'il est
//!   fourni en `Authorization: Bearer`, l'access token de la même session :
//!   vider le stockage du client ne suffit pas, un jeton copié resterait
//!   valide jusqu'à son expiration.
//! - Le logout-all (#186) ferme **toutes** les sessions de l'utilisateur, sur
//!   tous les appareils : il incrémente sa version de session, que portent
//!   tous ses jetons (claim `sv`) ; ceux émis avant sont refusés partout,
//!   access comme refresh. Les jetons sont toujours émis à la version
//!   courante.
//! - Le changement de mot de passe (#196) exige le mot de passe **actuel**
//!   (un access token volé ne suffit pas ; essais limités par compte), écrit
//!   le nouveau hash seulement si l'ancien est toujours en place, puis ferme
//!   **toutes** les sessions (version de session) et renvoie une nouvelle
//!   paire à l'appareil courant. Mot de passe actuel faux : **403**, pas 401 —
//!   le client croirait sa session expirée et se déconnecterait.
//! - Le mot de passe oublié (#204) répond **toujours 202** à une demande bien
//!   formée : la recherche du compte et l'envoi de l'email se font **en tâche
//!   de fond**, si bien que ni le code ni la durée de la réponse ne révèlent
//!   l'existence du compte. Demandes limitées par email (anti-harcèlement
//!   d'une boîte) et par IP. Le lien porte un jeton sans état lié au hash
//!   actuel (cf. [`crate::password_reset`]) : usage unique, 30 min. Sa
//!   consommation remplace le hash (écriture conditionnelle), ferme **toutes**
//!   les sessions et lève le blocage de connexion du compte ; jeton invalide,
//!   expiré ou déjà utilisé : 400 (pas 401, qui signifie « session
//!   expirée » pour le client).
//! - Un compte **suspendu** par l'administration (#208, cf. [`crate::admin`])
//!   ne se connecte plus : 403 après vérification du mot de passe (seul le
//!   titulaire apprend la suspension), et aucun lien de réinitialisation ne
//!   lui est envoyé. Contre une suspension concurrente, la version de session
//!   est lue **avant** de relire le drapeau : l'administration écrit le
//!   drapeau puis incrémente la version, si bien qu'une paire émise sans voir
//!   le drapeau porte une version déjà périmée. Le changement de mot de passe
//!   suit le même ordre.
//! - La suppression du compte (#229, RGPD) exige le mot de passe **actuel**,
//!   comme son changement (mêmes essais limités, 403 s'il est faux) ; elle
//!   ferme toutes les sessions puis efface le compte et ses données (cf.
//!   [`crate::erasure`]). Un compte qui détient `*` est refusé (409) : un
//!   autre administrateur doit d'abord lui retirer ce rôle, si bien que
//!   l'instance n'est jamais laissée sans administrateur par ce chemin.
//! - La **vérification de l'email** (#231, cf. [`crate::email_verification`]),
//!   quand elle est active : l'inscription crée un compte non vérifié, lui
//!   envoie un lien **en tâche de fond** et répond 202 **sans jetons** ; la
//!   connexion d'un compte non vérifié est refusée (403, après vérification
//!   du mot de passe : seul le titulaire l'apprend). Le renvoi du lien suit
//!   le modèle du mot de passe oublié (toujours 202, quotas par email et par
//!   IP). Une réinitialisation du mot de passe réussie vérifie aussi
//!   l'adresse : elle prouve la même possession de la boîte, et rend son
//!   compte à qui se serait vu « réserver » son adresse par un tiers.
//! - Un compte **invité** par l'administration (#241, cf.
//!   [`crate::invitation`]) n'a pas de mot de passe utilisable : à la
//!   connexion, il est vérifié contre le hash leurre (même durée, même 401
//!   qu'un mauvais mot de passe : rien ne révèle une invitation en attente).
//!   L'acceptation de l'invitation vérifie le lien (HMAC, avant Argon2),
//!   écrit le mot de passe choisi à la place du hash inutilisable (écriture
//!   conditionnelle : usage unique), marque l'adresse vérifiée (le lien a été
//!   reçu) et connecte. Un mot de passe oublié sur un compte invité suit le
//!   même chemin que la réinitialisation : le lien reçu prouve la boîte.
//!   Les inscriptions publiques peuvent être fermées (`AUTOME_SIGNUP=
//!   invite_only`) : 403 `signup closed`.
//! - Un mot de passe **choisi** (inscription, changement, réinitialisation,
//!   invitation) est refusé s'il est courant ou divulgué (#245, cf.
//!   [`crate::password_policy`]) : 400 avec un message unique. Jamais à la
//!   connexion.
//! - Second facteur (#245, cf. [`crate::mfa`]) : quand il est actif, la
//!   connexion par mot de passe répond 202 avec un défi, sans session.
//! - Chaque inscription/connexion/déconnexion/changement ou réinitialisation
//!   de mot de passe produit un event d'audit `auth.signup` / `auth.login` /
//!   `auth.logout` / `auth.logout_all` / `auth.password_change` /
//!   `auth.password_reset_request` / `auth.password_reset` /
//!   `auth.email_verify_request` / `auth.email_verify` /
//!   `auth.invitation_accept` ; la suppression,
//!   `auth.account_delete` sous le pseudonyme du compte effacé.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use autome_core::{
    email_throttle_key, has_usable_password, normalize_email, role_of, validate_email,
    validate_password, AuditEvent, AuditOutcome, Config, CreateUser, InMemoryLoginThrottle,
    LoginAttempt, LoginThrottleStore, PasswordHasher, Role, TokenKind, UserStore,
    COMPROMISED_PASSWORD_MESSAGE, PERM_WILDCARD,
};
pub use autome_core::{LOGIN_WINDOW_SECONDS, MAX_LOGIN_FAILURES};
use axum::{
    extract::{ConnectInfo, FromRef, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{bearer_token, AuthState, AuthenticatedUser};
use crate::client_ip::{ClientIpPolicy, IpKeyer};
use crate::email_verification::{EmailVerification, VerifyTokens};
use crate::erasure::{erase_account, Erasure};
use crate::invitation::InviteTokens;
use crate::mfa::{login_challenge, mfa_router, Mfa, MfaChallenge};
use crate::password_policy::PasswordPolicy;
use crate::password_reset::{PasswordReset, ResetTokens};

/// Durée de vie d'un access token (15 min).
pub const ACCESS_TTL_SECONDS: i64 = 15 * 60;
/// Durée de vie d'un refresh token (7 jours).
pub const REFRESH_TTL_SECONDS: i64 = 7 * 24 * 3600;
/// Mot de passe du hash leurre (jamais attribué à un compte).
const DUMMY_PASSWORD: &str = "autome-dummy-password-for-timing";

/// Quota par défaut des échecs de connexion par adresse IP et par fenêtre.
pub const DEFAULT_LOGIN_IP_MAX_ATTEMPTS: u32 = 50;
/// Quota par défaut des tentatives d'inscription par adresse IP et par fenêtre.
pub const DEFAULT_SIGNUP_IP_MAX_ATTEMPTS: u32 = 10;
/// Quota par défaut des demandes de réinitialisation par adresse IP et par
/// fenêtre (#204).
pub const DEFAULT_RESET_IP_MAX_ATTEMPTS: u32 = 10;
/// Quota par défaut des renvois du lien de vérification par adresse IP et
/// par fenêtre (#231).
pub const DEFAULT_VERIFY_IP_MAX_ATTEMPTS: u32 = 10;

/// Anti-bruteforce par adresse IP cliente (#190). Un quota à 0 désactive le
/// compteur correspondant.
#[derive(Debug, Clone)]
pub struct IpThrottle {
    /// Résolution de l'IP cliente (proxys de confiance).
    pub policy: ClientIpPolicy,
    /// Pseudonymisation de l'IP en clé de compteur.
    pub keyer: IpKeyer,
    /// Échecs de connexion tolérés par IP et par fenêtre.
    pub login_max: u32,
    /// Tentatives d'inscription tolérées par IP et par fenêtre.
    pub signup_max: u32,
    /// Demandes de réinitialisation du mot de passe tolérées par IP et par
    /// fenêtre (#204).
    pub reset_max: u32,
    /// Renvois du lien de vérification tolérés par IP et par fenêtre (#231).
    pub verify_max: u32,
}

impl IpThrottle {
    /// Depuis l'environnement : `AUTOME_TRUSTED_PROXIES`,
    /// `AUTOME_LOGIN_IP_MAX_ATTEMPTS` (défaut 50), `AUTOME_SIGNUP_IP_MAX_ATTEMPTS`
    /// (défaut 10), `AUTOME_RESET_IP_MAX_ATTEMPTS` (défaut 10),
    /// `AUTOME_VERIFY_IP_MAX_ATTEMPTS` (défaut 10) ; clé HMAC dérivée
    /// d'`ENCRYPTION_KEY`.
    pub fn from_env(config: &Config) -> Result<Self, String> {
        Self::from_source(config, |name| std::env::var(name).ok())
    }

    /// Comme [`from_env`](Self::from_env), variables lues via `get` (tests).
    pub fn from_source(
        config: &Config,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let quota = |name: &str, default: u32| match get(name) {
            None => Ok(default),
            Some(v) if v.trim().is_empty() => Ok(default),
            Some(v) => v
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{name} : entier ≥ 0 attendu, obtenu `{v}`")),
        };
        Ok(Self {
            policy: ClientIpPolicy::parse(&get("AUTOME_TRUSTED_PROXIES").unwrap_or_default())?,
            keyer: IpKeyer::from_config(config),
            login_max: quota(
                "AUTOME_LOGIN_IP_MAX_ATTEMPTS",
                DEFAULT_LOGIN_IP_MAX_ATTEMPTS,
            )?,
            signup_max: quota(
                "AUTOME_SIGNUP_IP_MAX_ATTEMPTS",
                DEFAULT_SIGNUP_IP_MAX_ATTEMPTS,
            )?,
            reset_max: quota(
                "AUTOME_RESET_IP_MAX_ATTEMPTS",
                DEFAULT_RESET_IP_MAX_ATTEMPTS,
            )?,
            verify_max: quota(
                "AUTOME_VERIFY_IP_MAX_ATTEMPTS",
                DEFAULT_VERIFY_IP_MAX_ATTEMPTS,
            )?,
        })
    }
}

/// État des handlers de comptes.
#[derive(Clone)]
pub struct AccountsState {
    /// État d'authentification (émission/rotation des jetons, audit, horloge).
    pub auth: AuthState,
    /// Persistance des comptes.
    pub users: Arc<dyn UserStore + Send + Sync>,
    /// Hash/vérification des mots de passe.
    pub hasher: Arc<dyn PasswordHasher + Send + Sync>,
    /// Anti-bruteforce du login (en mémoire par défaut ; partagé en
    /// PostgreSQL via [`with_throttle`](Self::with_throttle)).
    pub throttle: Arc<dyn LoginThrottleStore>,
    /// Anti-bruteforce par IP (#190) ; `None` = désactivé (tests unitaires).
    pub ip_throttle: Option<IpThrottle>,
    /// Permissions accordées à l'inscription (rôle `editor` par défaut).
    pub signup_permissions: HashSet<String>,
    /// Mot de passe oublié (#204) ; `None` = indisponible (503), faute
    /// d'envoi d'email configuré.
    pub password_reset: Option<PasswordReset>,
    /// Suppression de compte (#229) ; `None` = indisponible (503).
    pub erasure: Option<Erasure>,
    /// Vérification de l'email (#231) ; `None` = désactivée : les comptes
    /// sont créés vérifiés, la connexion n'est pas conditionnée.
    pub email_verification: Option<EmailVerification>,
    /// Acceptation des invitations (#241) ; `None` = indisponible (503).
    pub invitations: Option<InviteTokens>,
    /// Inscriptions publiques ouvertes (`AUTOME_SIGNUP`, #241) ; fermées, les
    /// comptes ne naissent que par invitation.
    pub signup_open: bool,
    /// Refus des mots de passe courants ou compromis (#245) ; listes
    /// intégrées par défaut.
    pub password_policy: PasswordPolicy,
    /// Second facteur TOTP (#245) ; store en mémoire et clé aléatoire par
    /// défaut (cf. [`with_mfa`](Self::with_mfa)).
    pub mfa: Mfa,
    /// Hash leurre vérifié quand l'email est inconnu (égalisation du temps).
    dummy_hash: Arc<str>,
}

impl AccountsState {
    /// Construit l'état ; les comptes créés reçoivent le rôle `editor`.
    /// Échoue seulement si le hash leurre ne peut être calculé.
    pub fn new(
        auth: AuthState,
        users: Arc<dyn UserStore + Send + Sync>,
        hasher: Arc<dyn PasswordHasher + Send + Sync>,
    ) -> autome_core::Result<Self> {
        let dummy_hash = hasher.hash(DUMMY_PASSWORD)?;
        Ok(Self {
            auth,
            users,
            hasher,
            throttle: Arc::new(InMemoryLoginThrottle::new()),
            ip_throttle: None,
            signup_permissions: Role::editor().permissions,
            password_reset: None,
            erasure: None,
            email_verification: None,
            invitations: None,
            signup_open: true,
            password_policy: PasswordPolicy::default(),
            mfa: Mfa::in_memory(),
            dummy_hash: dummy_hash.into(),
        })
    }

    /// Remplace le compteur anti-bruteforce (ex. PostgreSQL, partagé entre
    /// répliques).
    pub fn with_throttle(mut self, throttle: Arc<dyn LoginThrottleStore>) -> Self {
        self.throttle = throttle;
        self
    }

    /// Active l'anti-bruteforce par adresse IP (#190).
    pub fn with_ip_throttle(mut self, ip_throttle: IpThrottle) -> Self {
        self.ip_throttle = Some(ip_throttle);
        self
    }

    /// Active la réinitialisation du mot de passe par email (#204).
    pub fn with_password_reset(mut self, password_reset: PasswordReset) -> Self {
        self.password_reset = Some(password_reset);
        self
    }

    /// Active la suppression de compte en libre-service (#229).
    pub fn with_erasure(mut self, erasure: Erasure) -> Self {
        self.erasure = Some(erasure);
        self
    }

    /// Exige la vérification de l'email à l'inscription (#231).
    pub fn with_email_verification(mut self, verification: EmailVerification) -> Self {
        self.email_verification = Some(verification);
        self
    }

    /// Accepte les liens d'invitation émis par l'administration (#241).
    pub fn with_invitations(mut self, tokens: InviteTokens) -> Self {
        self.invitations = Some(tokens);
        self
    }

    /// Ouvre (`true`, défaut) ou ferme les inscriptions publiques (#241).
    pub fn with_signup_open(mut self, open: bool) -> Self {
        self.signup_open = open;
        self
    }

    /// Remplace le contrôle des mots de passe choisis (#245), ex. fichier de
    /// l'opérateur et corpus Have I Been Pwned.
    pub fn with_password_policy(mut self, policy: PasswordPolicy) -> Self {
        self.password_policy = policy;
        self
    }

    /// Branche le second facteur (#245) sur un store partagé (PostgreSQL) et
    /// les clés dérivées d'`ENCRYPTION_KEY`.
    pub fn with_mfa(mut self, mfa: Mfa) -> Self {
        self.mfa = mfa;
        self
    }
}

// Permet l'extracteur [`AuthenticatedUser`] sur `/auth/logout-all`.
impl FromRef<AccountsState> for AuthState {
    fn from_ref(input: &AccountsState) -> Self {
        input.auth.clone()
    }
}

/// Body de `/auth/signup` et `/auth/login`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CredentialsBody {
    /// Adresse email (normalisée côté serveur : trim + minuscules).
    pub email: String,
    /// Mot de passe en clair (8 à 1024 caractères).
    pub password: String,
}

/// Body de `/auth/refresh` et `/auth/logout`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshBody {
    /// Refresh token obtenu au login/signup ou au refresh précédent.
    pub refresh_token: String,
}

/// Body de `/auth/password`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ChangePasswordBody {
    /// Mot de passe actuel (preuve de possession du compte).
    pub current_password: String,
    /// Nouveau mot de passe (8 à 1024 caractères, différent de l'actuel).
    pub new_password: String,
}

/// Body de `DELETE /auth/me`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteAccountBody {
    /// Mot de passe actuel (confirmation de la suppression).
    pub current_password: String,
}

/// Body de `/auth/password/forgot`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ForgotPasswordBody {
    /// Adresse email du compte (normalisée côté serveur).
    pub email: String,
}

/// Body de `/auth/password/reset`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetPasswordBody {
    /// Jeton reçu par email (fragment `#reset=` du lien).
    pub token: String,
    /// Nouveau mot de passe (8 à 1024 caractères).
    pub new_password: String,
}

/// Body de `/auth/email/verify`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct VerifyEmailBody {
    /// Jeton reçu par email (fragment `#verify=` du lien).
    pub token: String,
}

/// Body de `/auth/invitation/accept`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AcceptInvitationBody {
    /// Jeton reçu par email (fragment `#invite=` du lien).
    pub token: String,
    /// Mot de passe choisi (8 à 1024 caractères).
    pub password: String,
}

/// Body de `/auth/email/resend`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResendVerificationBody {
    /// Adresse email du compte (normalisée côté serveur).
    pub email: String,
}

/// Réponse 202 de `/auth/signup` quand l'adresse doit être vérifiée (#231) :
/// aucun jeton, un lien de vérification part vers `email`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SignupPending {
    /// Adresse normalisée à laquelle le lien est envoyé.
    pub email: String,
    /// Toujours `true` : connexion possible après vérification.
    pub verification_required: bool,
}

/// Paire de jetons émise par signup/login/refresh.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TokenResponse {
    /// JWT d'accès, à présenter en `Authorization: Bearer`.
    pub access_token: String,
    /// JWT de rafraîchissement (usage unique).
    pub refresh_token: String,
    /// Toujours `Bearer`.
    pub token_type: String,
    /// Durée de vie de l'access token, en secondes.
    pub expires_in: i64,
}

/// Réponse de [`me`] : l'utilisateur authentifié et ses droits.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    /// Identifiant (`claims.sub`).
    pub id: String,
    /// Email normalisé.
    pub email: String,
    /// Rôle déduit des permissions : `admin`, `editor`, `viewer` ou `custom`.
    pub role: String,
    /// Permissions accordées, triées (`*` = toutes).
    pub permissions: Vec<String>,
    /// Second facteur actif (#245).
    pub mfa_enabled: bool,
}

/// Erreurs des handlers de comptes.
#[derive(Debug)]
pub enum AccountError {
    /// Saisie invalide (email, politique de mot de passe).
    BadRequest(String),
    /// Identifiants ou jeton refusés — message volontairement générique.
    Unauthorized,
    /// Conflit : email déjà associé à un compte, ou mot de passe changé
    /// entre-temps.
    Conflict(&'static str),
    /// Trop d'échecs de connexion : réessayer après `n` secondes.
    TooManyAttempts(i64),
    /// Authentifié mais refusé (mot de passe actuel incorrect).
    Forbidden(&'static str),
    /// Fonction non configurée sur ce déploiement (ex. aucun envoi d'email).
    Unavailable(&'static str),
    /// Erreur interne (store, hash).
    Internal(String),
}

impl IntoResponse for AccountError {
    fn into_response(self) -> Response {
        match self {
            AccountError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            AccountError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "invalid credentials").into_response()
            }
            AccountError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
            AccountError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
            AccountError::Unavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, msg).into_response()
            }
            AccountError::TooManyAttempts(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                "too many attempts",
            )
                .into_response(),
            AccountError::Internal(msg) => {
                tracing::error!(error = %msg, "erreur interne des comptes");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

pub(crate) fn internal(e: impl std::fmt::Display) -> AccountError {
    AccountError::Internal(e.to_string())
}

/// Exécute un calcul Argon2 hors du runtime async.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> autome_core::Result<T> + Send + 'static,
) -> Result<T, AccountError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(internal)?
        .map_err(internal)
}

async fn audit(state: &AccountsState, actor: &str, action: &str, outcome: AuditOutcome) {
    let now_ms = state.auth.clock.now_unix_seconds().saturating_mul(1_000);
    let event = AuditEvent::new(now_ms, actor, action, outcome);
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(action, error = %e, "événement d'audit non enregistré");
    }
}

/// Compteur IP `scope` de la requête : `(clé, quota)`, ou `None` si la
/// limitation par IP est désactivée (quota 0) ou l'adresse du pair inconnue.
pub(crate) fn ip_counter(
    state: &AccountsState,
    scope: &str,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
) -> Option<(String, u32)> {
    let ip = state.ip_throttle.as_ref()?;
    let max = match scope {
        LOGIN_IP_SCOPE => ip.login_max,
        RESET_IP_SCOPE => ip.reset_max,
        VERIFY_IP_SCOPE => ip.verify_max,
        _ => ip.signup_max,
    };
    if max == 0 {
        return None;
    }
    let Some(peer) = peer else {
        // Serveur monté sans `into_make_service_with_connect_info` : rien à
        // compter. Le binaire le monte toujours.
        tracing::warn!(
            scope,
            "adresse du pair inconnue : limitation par IP inactive"
        );
        return None;
    };
    let client = ip.policy.client_ip(peer.ip(), headers);
    Some((ip.keyer.key(scope, client), max))
}

/// Réserve une tentative sur le compteur IP ; `Err(429)` si son quota est
/// atteint. Ne journalise jamais l'adresse.
pub(crate) async fn check_ip(
    state: &AccountsState,
    counter: &Option<(String, u32)>,
    action: &str,
    email: &str,
    now: i64,
) -> Result<(), AccountError> {
    let Some((key, max)) = counter else {
        return Ok(());
    };
    match state
        .throttle
        .begin_attempt(key, *max, now)
        .await
        .map_err(internal)?
    {
        LoginAttempt::Allowed => Ok(()),
        LoginAttempt::Blocked { retry_after_secs } => {
            tracing::info!(action, "quota de tentatives de l'adresse IP atteint");
            audit(state, email, action, AuditOutcome::Denied).await;
            Err(AccountError::TooManyAttempts(retry_after_secs))
        }
    }
}

pub(crate) const LOGIN_IP_SCOPE: &str = "login-ip";
const SIGNUP_IP_SCOPE: &str = "signup-ip";
const RESET_IP_SCOPE: &str = "reset-ip";
const VERIFY_IP_SCOPE: &str = "verify-ip";

fn token_response(access_token: String, refresh_token: String) -> TokenResponse {
    TokenResponse {
        access_token,
        refresh_token,
        token_type: "Bearer".into(),
        expires_in: ACCESS_TTL_SECONDS,
    }
}

/// Message du refus de connexion d'un compte suspendu (#208).
pub(crate) const ACCOUNT_DISABLED: &str = "account disabled";
/// Message du refus de connexion d'un compte dont l'email n'est pas vérifié
/// (#231).
pub(crate) const EMAIL_NOT_VERIFIED: &str = "email not verified";
/// Message du refus d'inscription quand elles sont fermées (#241).
const SIGNUP_CLOSED: &str = "signup closed";

/// Émet une paire à la version de session courante de l'utilisateur (#186).
async fn issue_pair(state: &AccountsState, user_id: &str) -> Result<TokenResponse, AccountError> {
    let sv = state
        .auth
        .revocation
        .session_version(user_id)
        .await
        .map_err(internal)?;
    issue_pair_at(state, user_id, sv)
}

/// Émet une paire à la version de session `sv`, lue par l'appelant.
pub(crate) fn issue_pair_at(
    state: &AccountsState,
    user_id: &str,
    sv: i64,
) -> Result<TokenResponse, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let (access, refresh) = state
        .auth
        .token_service
        .issue_pair(user_id, sv, ACCESS_TTL_SECONDS, REFRESH_TTL_SECONDS, now)
        .map_err(internal)?;
    Ok(token_response(access, refresh))
}

/// `true` si le compte existe et n'est pas suspendu (#208). À appeler
/// **après** avoir lu la version de session des jetons à émettre.
pub(crate) async fn is_enabled(state: &AccountsState, user_id: &str) -> Result<bool, AccountError> {
    Ok(state
        .users
        .find_by_id(user_id)
        .await
        .map_err(internal)?
        .is_some_and(|u| !u.disabled))
}

/// Refuse (400) un mot de passe choisi courant ou compromis pour le compte
/// `email` (#245). La raison est journalisée, jamais le mot de passe ; le
/// client reçoit un message unique.
async fn screen_password(
    state: &AccountsState,
    password: &str,
    email: &str,
) -> Result<(), AccountError> {
    match state.password_policy.weakness(password, Some(email)).await {
        None => Ok(()),
        Some(weakness) => {
            tracing::info!(
                ?weakness,
                "mot de passe choisi refusé : courant ou compromis"
            );
            Err(AccountError::BadRequest(
                COMPROMISED_PASSWORD_MESSAGE.into(),
            ))
        }
    }
}

/// `POST /auth/signup` — crée un compte (rôle `editor`) et le connecte ; si
/// la vérification de l'email est exigée (#231), il reste non vérifié, un
/// lien lui est envoyé et la réponse (202) ne porte aucun jeton.
#[utoipa::path(
    post,
    path = "/auth/signup",
    tag = "auth",
    request_body = CredentialsBody,
    responses(
        (status = 201, description = "Compte créé et connecté (vérification de l'email désactivée)", body = TokenResponse),
        (status = 202, description = "Compte créé, email de vérification envoyé : connexion possible après vérification", body = SignupPending),
        (status = 400, description = "Email ou mot de passe invalide, ou mot de passe courant ou divulgué dans une fuite"),
        (status = 403, description = "Inscriptions fermées (`signup closed`) : comptes créés par invitation seulement"),
        (status = 409, description = "Email déjà enregistré"),
        (status = 429, description = "Trop d'inscriptions depuis cette adresse — voir `Retry-After`")
    )
)]
pub async fn signup(
    State(state): State<AccountsState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<CredentialsBody>,
) -> Result<Response, AccountError> {
    let email = normalize_email(&body.email);
    if !state.signup_open {
        audit(&state, &email, "auth.signup", AuditOutcome::Denied).await;
        return Err(AccountError::Forbidden(SIGNUP_CLOSED));
    }
    validate_email(&email).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    validate_password(&body.password).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    // Toute inscription compte (réussie ou non) : création de comptes en masse
    // et Argon2 à volonté. Avant le hash, fail closed.
    let ip = ip_counter(&state, SIGNUP_IP_SCOPE, peer.map(|c| c.0), &headers);
    let now = state.auth.clock.now_unix_seconds();
    check_ip(&state, &ip, "auth.signup", &email, now).await?;
    // Après le quota IP : un anonyme ne déclenche pas d'appels au corpus
    // distant à volonté.
    screen_password(&state, &body.password, &email).await?;

    let hasher = Arc::clone(&state.hasher);
    let password = body.password;
    let hash = blocking(move || hasher.hash(&password)).await?;

    let user = match state
        .users
        .create(
            &email,
            &hash,
            &state.signup_permissions,
            state.email_verification.is_none(),
        )
        .await
        .map_err(internal)?
    {
        CreateUser::Created(user) => user,
        CreateUser::EmailTaken => {
            audit(&state, &email, "auth.signup", AuditOutcome::Denied).await;
            return Err(AccountError::Conflict("email already registered"));
        }
    };
    audit(&state, &user.id, "auth.signup", AuditOutcome::Success).await;
    let Some(verification) = state.email_verification.clone() else {
        return Ok((
            StatusCode::CREATED,
            Json(issue_pair(&state, &user.id).await?),
        )
            .into_response());
    };
    let pending = SignupPending {
        email: user.email.clone(),
        verification_required: true,
    };
    // En tâche de fond : la durée de la réponse ne dépend pas du SMTP.
    tokio::spawn(async move { send_verification_link(&state, &verification, &user, now).await });
    Ok((StatusCode::ACCEPTED, Json(pending)).into_response())
}

/// `POST /auth/login` — échange email + mot de passe contre une paire de
/// jetons ; si le second facteur est actif (#245), contre un défi (202) à
/// présenter avec un code à `/auth/mfa/verify`.
#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "auth",
    request_body = CredentialsBody,
    responses(
        (status = 200, description = "Connecté", body = TokenResponse),
        (status = 202, description = "Mot de passe correct, second facteur exigé : défi à présenter avec un code à `/auth/mfa/verify`", body = MfaChallenge),
        (status = 401, description = "Identifiants invalides"),
        (status = 403, description = "Compte suspendu par un administrateur (`account disabled`), ou adresse email pas encore vérifiée (`email not verified`)"),
        (status = 429, description = "Trop d'échecs (compte ou adresse IP) — voir `Retry-After`")
    )
)]
pub async fn login(
    State(state): State<AccountsState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<CredentialsBody>,
) -> Result<Response, AccountError> {
    let email = normalize_email(&body.email);
    let email_key = email_throttle_key(&email);
    let now = state.auth.clock.now_unix_seconds();
    // Fail closed : sans compteurs, pas de vérification de mot de passe. L'IP
    // d'abord : un attaquant bloqué n'use plus les compteurs des emails visés.
    let ip = ip_counter(&state, LOGIN_IP_SCOPE, peer.map(|c| c.0), &headers);
    check_ip(&state, &ip, "auth.login", &email, now).await?;
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&email_key, MAX_LOGIN_FAILURES, now)
        .await
        .map_err(internal)?
    {
        audit(&state, &email, "auth.login", AuditOutcome::Denied).await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }

    // Compte invité sans mot de passe (#241) : traité comme inconnu.
    let user = state
        .users
        .find_by_email(&email)
        .await
        .map_err(internal)?
        .filter(|u| has_usable_password(&u.password_hash));
    // Email inconnu : on vérifie quand même contre le hash leurre pour que la
    // durée de réponse ne révèle pas l'existence du compte.
    let phc = match &user {
        Some(u) => u.password_hash.clone(),
        None => state.dummy_hash.to_string(),
    };
    let hasher = Arc::clone(&state.hasher);
    let password = body.password;
    // Un hash PHC mal formé en base (compte non migré) vaut un refus, pas une 500.
    let matches = blocking(move || Ok(hasher.verify(&password, &phc).unwrap_or(false))).await?;

    match user {
        Some(user) if matches => {
            // Version lue avant le drapeau de suspension (cf. doc du module).
            let sv = state
                .auth
                .revocation
                .session_version(&user.id)
                .await
                .map_err(internal)?;
            if !is_enabled(&state, &user.id).await? {
                audit(&state, &user.id, "auth.login", AuditOutcome::Denied).await;
                return Err(AccountError::Forbidden(ACCOUNT_DISABLED));
            }
            // La connexion a réussi : un compteur non remis à zéro ne doit pas
            // la faire échouer (il expirera avec sa fenêtre).
            if let Err(e) = state.throttle.reset(&email_key).await {
                tracing::warn!(error = %e, "remise à zéro du compteur de connexion impossible");
            }
            // Une connexion réussie ne compte pas dans le quota de son IP (NAT
            // d'entreprise) ; seule sa propre réservation est rendue.
            if let Some((key, _)) = &ip {
                if let Err(e) = state.throttle.refund(key).await {
                    tracing::warn!(error = %e, "remboursement du compteur IP impossible");
                }
            }
            // Bon mot de passe, adresse non vérifiée (#231) : pas de session.
            // La vérification ne fait que passer à vrai : la lecture du
            // compte ci-dessus suffit.
            if state.email_verification.is_some() && !user.email_verified {
                audit(&state, &user.id, "auth.login", AuditOutcome::Denied).await;
                return Err(AccountError::Forbidden(EMAIL_NOT_VERIFIED));
            }
            // Second facteur actif (#245) : un défi, pas de session.
            if let Some(challenge) = login_challenge(&state, &user, sv).await? {
                return Ok((StatusCode::ACCEPTED, Json(challenge)).into_response());
            }
            audit(&state, &user.id, "auth.login", AuditOutcome::Success).await;
            Ok(Json(issue_pair_at(&state, &user.id, sv)?).into_response())
        }
        _ => {
            audit(&state, &email, "auth.login", AuditOutcome::Failure).await;
            Err(AccountError::Unauthorized)
        }
    }
}

/// `POST /auth/refresh` — rotation : le refresh présenté est révoqué et une
/// nouvelle paire est émise. Un refresh déjà utilisé est refusé (rejeu).
#[utoipa::path(
    post,
    path = "/auth/refresh",
    tag = "auth",
    request_body = RefreshBody,
    responses(
        (status = 200, description = "Nouvelle paire", body = TokenResponse),
        (status = 401, description = "Refresh invalide, expiré ou déjà utilisé")
    )
)]
pub async fn refresh(
    State(state): State<AccountsState>,
    Json(body): Json<RefreshBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let (access_token, refresh_token) = state
        .auth
        .token_service
        .rotate(
            &body.refresh_token,
            state.auth.revocation.as_ref(),
            ACCESS_TTL_SECONDS,
            REFRESH_TTL_SECONDS,
            now,
        )
        .await
        .map_err(|_| AccountError::Unauthorized)?;
    Ok(Json(token_response(access_token, refresh_token)))
}

/// `POST /auth/logout` — ferme la session côté serveur.
///
/// Révoque le refresh présenté (plus de rotation possible) et, s'il est fourni
/// en `Authorization: Bearer`, l'access token **du même utilisateur** (refusé
/// partout jusqu'à son expiration). Idempotent : un refresh déjà révoqué
/// répond aussi 204. Un Bearer absent, invalide ou d'un autre utilisateur
/// n'empêche pas la déconnexion : il est simplement ignoré.
#[utoipa::path(
    post,
    path = "/auth/logout",
    tag = "auth",
    request_body = RefreshBody,
    params(
        ("Authorization" = Option<String>, Header,
         description = "`Bearer <access_token>` de la session, révoqué aussi s'il est fourni")
    ),
    responses(
        (status = 204, description = "Session fermée (ou déjà fermée)"),
        (status = 401, description = "Refresh invalide ou expiré")
    )
)]
pub async fn logout(
    State(state): State<AccountsState>,
    headers: HeaderMap,
    Json(body): Json<RefreshBody>,
) -> Result<StatusCode, AccountError> {
    let now = state.auth.clock.now_unix_seconds();
    let svc = &state.auth.token_service;
    let refresh = match svc.verify(&body.refresh_token, now) {
        Ok(claims) if claims.kind == TokenKind::Refresh => claims,
        _ => return Err(AccountError::Unauthorized),
    };
    let revocation = &state.auth.revocation;
    revocation
        .revoke(&refresh.jti, refresh.exp.saturating_mul(1_000))
        .await
        .map_err(internal)?;

    if let Ok(token) = bearer_token(&headers) {
        match svc.verify(token, now) {
            Ok(access) if access.kind == TokenKind::Access && access.sub == refresh.sub => {
                revocation
                    .revoke(&access.jti, access.exp.saturating_mul(1_000))
                    .await
                    .map_err(internal)?;
            }
            _ => tracing::debug!("logout : Bearer ignoré (invalide ou autre utilisateur)"),
        }
    }

    audit(&state, &refresh.sub, "auth.logout", AuditOutcome::Success).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/logout-all` — ferme toutes les sessions de l'utilisateur
/// authentifié, sur tous les appareils, y compris celle de la requête.
///
/// Incrémente sa version de session : tous ses access et refresh tokens émis
/// jusqu'ici sont refusés (401) ; une nouvelle connexion ouvre une session
/// valide. Réponse 204.
#[utoipa::path(
    post,
    path = "/auth/logout-all",
    tag = "auth",
    responses(
        (status = 204, description = "Toutes les sessions sont fermées"),
        (status = 401, description = "Access token absent, invalide ou déjà révoqué")
    ),
    security(("bearer" = []))
)]
pub async fn logout_all(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
) -> Result<StatusCode, AccountError> {
    let version = state
        .auth
        .revocation
        .bump_session_version(user.user_id())
        .await
        .map_err(internal)?;
    tracing::info!(
        user_id = user.user_id(),
        session_version = version,
        "toutes les sessions fermées"
    );
    audit(
        &state,
        user.user_id(),
        "auth.logout_all",
        AuditOutcome::Success,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/password` — change le mot de passe de l'utilisateur
/// authentifié et ferme **toutes** ses sessions, puis renvoie une nouvelle
/// paire de jetons pour l'appareil courant.
#[utoipa::path(
    post,
    path = "/auth/password",
    tag = "auth",
    request_body = ChangePasswordBody,
    responses(
        (status = 200, description = "Mot de passe changé ; autres sessions fermées", body = TokenResponse),
        (status = 400, description = "Nouveau mot de passe invalide, identique à l'actuel, courant ou divulgué dans une fuite"),
        (status = 401, description = "Access token absent, invalide ou révoqué"),
        (status = 403, description = "Mot de passe actuel incorrect"),
        (status = 409, description = "Mot de passe changé entre-temps (requête concurrente)"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`")
    ),
    security(("bearer" = []))
)]
pub async fn change_password(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<ChangePasswordBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    let user_id = user.user_id().to_string();
    validate_password(&body.new_password).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    if body.new_password == body.current_password {
        return Err(AccountError::BadRequest(
            "le nouveau mot de passe doit différer de l'actuel".into(),
        ));
    }
    // Un jeton volé ne doit pas permettre de deviner le mot de passe actuel :
    // essais limités par compte, réservés avant Argon2 (fail closed).
    let key = format!("password:{user_id}");
    let now = state.auth.clock.now_unix_seconds();
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&key, MAX_LOGIN_FAILURES, now)
        .await
        .map_err(internal)?
    {
        audit(
            &state,
            &user_id,
            "auth.password_change",
            AuditOutcome::Denied,
        )
        .await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }
    // Compte disparu (jeton d'un compte supprimé) ou suspendu : la session ne
    // vaut rien.
    let Some(account) = state
        .users
        .find_by_id(&user_id)
        .await
        .map_err(internal)?
        .filter(|a| !a.disabled)
    else {
        return Err(AccountError::Unauthorized);
    };

    let hasher = Arc::clone(&state.hasher);
    let (current, phc) = (body.current_password, account.password_hash.clone());
    let matches = blocking(move || Ok(hasher.verify(&current, &phc).unwrap_or(false))).await?;
    if !matches {
        audit(
            &state,
            &user_id,
            "auth.password_change",
            AuditOutcome::Failure,
        )
        .await;
        return Err(AccountError::Forbidden("current password is incorrect"));
    }
    // Mot de passe actuel prouvé : un nouveau mot de passe refusé ne compte
    // pas comme un essai (le titulaire ne doit pas se bloquer en cherchant).
    if let Err(e) = screen_password(&state, &body.new_password, &account.email).await {
        if let Err(refund) = state.throttle.refund(&key).await {
            tracing::warn!(error = %refund, "remboursement du compteur de mot de passe impossible");
        }
        return Err(e);
    }

    let hasher = Arc::clone(&state.hasher);
    let new_password = body.new_password;
    let new_hash = blocking(move || hasher.hash(&new_password)).await?;
    if !state
        .users
        .update_password(&user_id, &account.password_hash, &new_hash)
        .await
        .map_err(internal)?
    {
        // Changé par une autre requête depuis la vérification.
        audit(
            &state,
            &user_id,
            "auth.password_change",
            AuditOutcome::Failure,
        )
        .await;
        return Err(AccountError::Conflict("password changed concurrently"));
    }
    if let Err(e) = state.throttle.reset(&key).await {
        tracing::warn!(error = %e, "remise à zéro du compteur de mot de passe impossible");
    }

    // Toutes les sessions, y compris celle de la requête, sont fermées ; la
    // nouvelle paire porte la nouvelle version.
    let version = state
        .auth
        .revocation
        .bump_session_version(&user_id)
        .await
        .map_err(internal)?;
    tracing::info!(
        user_id = %user_id,
        session_version = version,
        "mot de passe changé, sessions fermées"
    );
    audit(
        &state,
        &user_id,
        "auth.password_change",
        AuditOutcome::Success,
    )
    .await;
    // Suspendu entre-temps (#208) : pas de nouvelle session.
    if !is_enabled(&state, &user_id).await? {
        return Err(AccountError::Unauthorized);
    }
    Ok(Json(issue_pair_at(&state, &user_id, version)?))
}

/// Message des jetons de réinitialisation refusés (même réponse quelle que
/// soit la raison : mal formé, expiré, falsifié, déjà utilisé, compte disparu).
const INVALID_RESET_TOKEN: &str = "invalid or expired reset link";

/// `POST /auth/password/forgot` — envoie un lien de réinitialisation à
/// l'adresse, **si** un compte lui correspond.
///
/// Toujours 202 pour une adresse bien formée : la recherche du compte et
/// l'envoi ont lieu en tâche de fond, la réponse ne révèle donc ni par son
/// code ni par sa durée si le compte existe.
#[utoipa::path(
    post,
    path = "/auth/password/forgot",
    tag = "auth",
    request_body = ForgotPasswordBody,
    responses(
        (status = 202, description = "Demande prise en compte (email envoyé si le compte existe)"),
        (status = 400, description = "Email invalide"),
        (status = 429, description = "Trop de demandes (adresse email ou IP) — voir `Retry-After`"),
        (status = 503, description = "Réinitialisation indisponible : aucun envoi d'email configuré")
    )
)]
pub async fn forgot_password(
    State(state): State<AccountsState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<ForgotPasswordBody>,
) -> Result<StatusCode, AccountError> {
    let Some(reset) = state.password_reset.clone() else {
        return Err(AccountError::Unavailable(
            "password reset is not configured",
        ));
    };
    let email = normalize_email(&body.email);
    validate_email(&email).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    let now = state.auth.clock.now_unix_seconds();
    // Chaque demande compte : par IP (emails en masse), puis par adresse (une
    // même boîte inondée). Indépendant de l'existence du compte.
    let ip = ip_counter(&state, RESET_IP_SCOPE, peer.map(|c| c.0), &headers);
    check_ip(&state, &ip, "auth.password_reset_request", &email, now).await?;
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&format!("reset:{email}"), reset.email_max, now)
        .await
        .map_err(internal)?
    {
        audit(
            &state,
            &email,
            "auth.password_reset_request",
            AuditOutcome::Denied,
        )
        .await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }

    tokio::spawn(async move { send_reset_link(&state, &reset, &email, now).await });
    Ok(StatusCode::ACCEPTED)
}

/// Tâche de fond de [`forgot_password`] : lien envoyé seulement si le compte
/// existe. Les erreurs sont journalisées (jamais le jeton).
async fn send_reset_link(state: &AccountsState, reset: &PasswordReset, email: &str, now: i64) {
    let user = match state.users.find_by_email(email).await {
        Ok(user) => user,
        Err(e) => {
            tracing::error!(error = %e, "réinitialisation : recherche du compte impossible");
            return;
        }
    };
    let Some(user) = user else {
        audit(
            state,
            email,
            "auth.password_reset_request",
            AuditOutcome::Failure,
        )
        .await;
        return;
    };
    // Compte suspendu (#208) : le lien ne lui rendrait pas l'accès.
    if user.disabled {
        audit(
            state,
            &user.id,
            "auth.password_reset_request",
            AuditOutcome::Denied,
        )
        .await;
        return;
    }
    let token = reset.tokens.issue(&user.id, &user.password_hash, now);
    match reset
        .mail
        .mailer
        .send(&reset.mail_for(&user.email, &token))
        .await
    {
        Ok(()) => {
            tracing::info!(user_id = %user.id, "lien de réinitialisation envoyé");
            audit(
                state,
                &user.id,
                "auth.password_reset_request",
                AuditOutcome::Success,
            )
            .await;
        }
        Err(e) => {
            tracing::error!(user_id = %user.id, error = %e, "envoi du lien de réinitialisation impossible");
            audit(
                state,
                &user.id,
                "auth.password_reset_request",
                AuditOutcome::Failure,
            )
            .await;
        }
    }
}

/// `POST /auth/password/reset` — consomme un lien de réinitialisation : le
/// mot de passe est remplacé, **toutes** les sessions sont fermées et le
/// blocage de connexion du compte est levé. Réponse 204 : l'utilisateur se
/// connecte ensuite avec son nouveau mot de passe.
#[utoipa::path(
    post,
    path = "/auth/password/reset",
    tag = "auth",
    request_body = ResetPasswordBody,
    responses(
        (status = 204, description = "Mot de passe remplacé ; sessions fermées"),
        (status = 400, description = "Lien invalide, expiré ou déjà utilisé, ou nouveau mot de passe invalide, courant ou divulgué"),
        (status = 503, description = "Réinitialisation indisponible : aucun envoi d'email configuré")
    )
)]
pub async fn reset_password(
    State(state): State<AccountsState>,
    Json(body): Json<ResetPasswordBody>,
) -> Result<StatusCode, AccountError> {
    let Some(reset) = state.password_reset.as_ref() else {
        return Err(AccountError::Unavailable(
            "password reset is not configured",
        ));
    };
    let invalid = || AccountError::BadRequest(INVALID_RESET_TOKEN.into());
    let token = ResetTokens::parse(&body.token).ok_or_else(invalid)?;
    validate_password(&body.new_password).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    let now = state.auth.clock.now_unix_seconds();
    let account = state
        .users
        .find_by_id(token.user_id)
        .await
        .map_err(internal)?;
    // Le jeton est vérifié (HMAC, bon marché) avant tout calcul Argon2.
    let Some(account) = account.filter(|a| reset.tokens.verify(&token, &a.password_hash, now))
    else {
        audit(
            &state,
            token.user_id,
            "auth.password_reset",
            AuditOutcome::Failure,
        )
        .await;
        return Err(invalid());
    };
    // Après le jeton : un lien valide est exigé avant d'interroger le corpus.
    screen_password(&state, &body.new_password, &account.email).await?;

    let hasher = Arc::clone(&state.hasher);
    let new_password = body.new_password;
    let new_hash = blocking(move || hasher.hash(&new_password)).await?;
    // Écriture conditionnelle sur le hash lié au jeton : de deux
    // utilisations concurrentes du même lien, une seule réussit.
    if !state
        .users
        .update_password(&account.id, &account.password_hash, &new_hash)
        .await
        .map_err(internal)?
    {
        audit(
            &state,
            &account.id,
            "auth.password_reset",
            AuditOutcome::Failure,
        )
        .await;
        return Err(invalid());
    }

    let version = state
        .auth
        .revocation
        .bump_session_version(&account.id)
        .await
        .map_err(internal)?;
    // Le lien a été reçu dans la boîte du compte : l'adresse est prouvée
    // (#231). Sans effet si elle a changé entre-temps.
    if !account.email_verified {
        if let Err(e) = state
            .users
            .mark_email_verified(&account.id, &account.email)
            .await
        {
            tracing::warn!(error = %e, "vérification de l'email après réinitialisation impossible");
        }
    }
    // Le compte a pu être bloqué par les échecs qui ont motivé la demande.
    for key in [
        email_throttle_key(&account.email),
        format!("password:{}", account.id),
    ] {
        if let Err(e) = state.throttle.reset(&key).await {
            tracing::warn!(error = %e, "remise à zéro d'un compteur impossible");
        }
    }
    tracing::info!(
        user_id = %account.id,
        session_version = version,
        "mot de passe réinitialisé, sessions fermées"
    );
    audit(
        &state,
        &account.id,
        "auth.password_reset",
        AuditOutcome::Success,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Message des liens de vérification refusés (même réponse quelle que soit la
/// raison : mal formé, expiré, falsifié, adresse changée, compte disparu).
const INVALID_VERIFY_TOKEN: &str = "invalid or expired verification link";

/// Envoie un lien de vérification à `user` (tâche de fond de [`signup`] et
/// [`resend_verification`]). Les erreurs sont journalisées (jamais le jeton).
async fn send_verification_link(
    state: &AccountsState,
    verification: &EmailVerification,
    user: &autome_core::UserRecord,
    now: i64,
) {
    const ACTION: &str = "auth.email_verify_request";
    let token = verification.tokens.issue(&user.id, &user.email, now);
    match verification
        .mail
        .mailer
        .send(&verification.mail_for(&user.email, &token))
        .await
    {
        Ok(()) => {
            tracing::info!(user_id = %user.id, "lien de vérification de l'email envoyé");
            audit(state, &user.id, ACTION, AuditOutcome::Success).await;
        }
        Err(e) => {
            tracing::error!(user_id = %user.id, error = %e, "envoi du lien de vérification impossible");
            audit(state, &user.id, ACTION, AuditOutcome::Failure).await;
        }
    }
}

/// `POST /auth/email/resend` — renvoie un lien de vérification à l'adresse,
/// **si** un compte non vérifié lui correspond.
///
/// Toujours 202 pour une adresse bien formée : comme pour le mot de passe
/// oublié, la recherche du compte et l'envoi ont lieu en tâche de fond.
#[utoipa::path(
    post,
    path = "/auth/email/resend",
    tag = "auth",
    request_body = ResendVerificationBody,
    responses(
        (status = 202, description = "Demande prise en compte (email envoyé si un compte non vérifié existe)"),
        (status = 400, description = "Email invalide"),
        (status = 429, description = "Trop de demandes (adresse email ou IP) — voir `Retry-After`"),
        (status = 503, description = "Vérification de l'email désactivée sur ce déploiement")
    )
)]
pub async fn resend_verification(
    State(state): State<AccountsState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<ResendVerificationBody>,
) -> Result<StatusCode, AccountError> {
    const ACTION: &str = "auth.email_verify_request";
    let Some(verification) = state.email_verification.clone() else {
        return Err(AccountError::Unavailable(
            "email verification is not enabled",
        ));
    };
    let email = normalize_email(&body.email);
    validate_email(&email).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    let now = state.auth.clock.now_unix_seconds();
    // Par IP (emails en masse), puis par adresse (une même boîte inondée) ;
    // indépendant de l'existence du compte.
    let ip = ip_counter(&state, VERIFY_IP_SCOPE, peer.map(|c| c.0), &headers);
    check_ip(&state, &ip, ACTION, &email, now).await?;
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&format!("verify:{email}"), verification.email_max, now)
        .await
        .map_err(internal)?
    {
        audit(&state, &email, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }

    tokio::spawn(async move {
        let user = match state.users.find_by_email(&email).await {
            Ok(user) => user,
            Err(e) => {
                tracing::error!(error = %e, "vérification : recherche du compte impossible");
                return;
            }
        };
        match user {
            None => audit(&state, &email, ACTION, AuditOutcome::Failure).await,
            // Déjà vérifié (rien à faire), suspendu ou invité sans mot de passe
            // (#241) : le lien ne lui rendrait pas l'accès.
            Some(user)
                if user.email_verified
                    || user.disabled
                    || !has_usable_password(&user.password_hash) =>
            {
                audit(&state, &user.id, ACTION, AuditOutcome::Denied).await
            }
            Some(user) => send_verification_link(&state, &verification, &user, now).await,
        }
    });
    Ok(StatusCode::ACCEPTED)
}

/// `POST /auth/email/verify` — consomme un lien de vérification : l'adresse
/// du compte est marquée vérifiée et la connexion devient possible. Réponse
/// 204 : l'utilisateur se connecte ensuite (le lien n'ouvre pas de session).
/// Un lien déjà utilisé reste accepté tant qu'il n'a pas expiré (idempotent).
#[utoipa::path(
    post,
    path = "/auth/email/verify",
    tag = "auth",
    request_body = VerifyEmailBody,
    responses(
        (status = 204, description = "Adresse vérifiée"),
        (status = 400, description = "Lien invalide ou expiré"),
        (status = 503, description = "Vérification de l'email désactivée sur ce déploiement")
    )
)]
pub async fn verify_email(
    State(state): State<AccountsState>,
    Json(body): Json<VerifyEmailBody>,
) -> Result<StatusCode, AccountError> {
    const ACTION: &str = "auth.email_verify";
    let Some(verification) = state.email_verification.as_ref() else {
        return Err(AccountError::Unavailable(
            "email verification is not enabled",
        ));
    };
    let invalid = || AccountError::BadRequest(INVALID_VERIFY_TOKEN.into());
    let token = VerifyTokens::parse(&body.token).ok_or_else(invalid)?;
    let now = state.auth.clock.now_unix_seconds();
    let account = state
        .users
        .find_by_id(token.user_id)
        .await
        .map_err(internal)?
        .filter(|a| verification.tokens.verify(&token, &a.email, now));
    // Écriture conditionnelle sur l'adresse liée au jeton.
    let verified = match &account {
        Some(a) => state
            .users
            .mark_email_verified(&a.id, &a.email)
            .await
            .map_err(internal)?,
        None => false,
    };
    if !verified {
        audit(&state, token.user_id, ACTION, AuditOutcome::Failure).await;
        return Err(invalid());
    }
    tracing::info!(user_id = %token.user_id, "adresse email vérifiée");
    audit(&state, token.user_id, ACTION, AuditOutcome::Success).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Message des liens d'invitation refusés (même réponse quelle que soit la
/// raison : mal formé, expiré, falsifié, déjà utilisé ou renvoyé depuis,
/// compte disparu).
const INVALID_INVITE_TOKEN: &str = "invalid or expired invitation link";

/// `POST /auth/invitation/accept` — consomme un lien d'invitation (#241) : le
/// mot de passe choisi remplace le hash inutilisable, l'adresse est marquée
/// vérifiée et une paire de jetons est émise (connexion directe).
#[utoipa::path(
    post,
    path = "/auth/invitation/accept",
    tag = "auth",
    request_body = AcceptInvitationBody,
    responses(
        (status = 200, description = "Invitation acceptée, connecté", body = TokenResponse),
        (status = 400, description = "Lien invalide, expiré, déjà utilisé ou remplacé par un renvoi, ou mot de passe invalide, courant ou divulgué"),
        (status = 403, description = "Compte suspendu par un administrateur (`account disabled`)"),
        (status = 503, description = "Invitations indisponibles : aucun envoi d'email configuré")
    )
)]
pub async fn accept_invitation(
    State(state): State<AccountsState>,
    Json(body): Json<AcceptInvitationBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    const ACTION: &str = "auth.invitation_accept";
    let Some(tokens) = state.invitations.as_ref() else {
        return Err(AccountError::Unavailable("invitations are not configured"));
    };
    let invalid = || AccountError::BadRequest(INVALID_INVITE_TOKEN.into());
    let token = InviteTokens::parse(&body.token).ok_or_else(invalid)?;
    validate_password(&body.password).map_err(|e| AccountError::BadRequest(e.to_string()))?;
    let now = state.auth.clock.now_unix_seconds();
    // Le jeton est vérifié (HMAC, bon marché) avant tout calcul Argon2 ; il
    // n'est lié qu'à un hash inutilisable (compte encore invité).
    let Some(account) = state
        .users
        .find_by_id(token.user_id)
        .await
        .map_err(internal)?
        .filter(|a| {
            !has_usable_password(&a.password_hash) && tokens.verify(&token, &a.password_hash, now)
        })
    else {
        audit(&state, token.user_id, ACTION, AuditOutcome::Failure).await;
        return Err(invalid());
    };
    // Suspendu avant d'avoir accepté : le lien reste valable s'il est réactivé.
    if account.disabled {
        audit(&state, &account.id, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::Forbidden(ACCOUNT_DISABLED));
    }
    screen_password(&state, &body.password, &account.email).await?;

    let hasher = Arc::clone(&state.hasher);
    let password = body.password;
    let hash = blocking(move || hasher.hash(&password)).await?;
    // Écriture conditionnelle sur le hash lié au jeton : de deux acceptations
    // concurrentes (ou d'une acceptation et d'un renvoi), une seule gagne.
    if !state
        .users
        .update_password(&account.id, &account.password_hash, &hash)
        .await
        .map_err(internal)?
    {
        audit(&state, &account.id, ACTION, AuditOutcome::Failure).await;
        return Err(invalid());
    }
    // Le lien a été reçu dans la boîte du compte : l'adresse est prouvée.
    if let Err(e) = state
        .users
        .mark_email_verified(&account.id, &account.email)
        .await
    {
        tracing::warn!(error = %e, "vérification de l'email après invitation impossible");
    }
    // Version lue avant le drapeau de suspension (cf. doc du module).
    let sv = state
        .auth
        .revocation
        .session_version(&account.id)
        .await
        .map_err(internal)?;
    if !is_enabled(&state, &account.id).await? {
        audit(&state, &account.id, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::Forbidden(ACCOUNT_DISABLED));
    }
    tracing::info!(user_id = %account.id, "invitation acceptée");
    audit(&state, &account.id, ACTION, AuditOutcome::Success).await;
    Ok(Json(issue_pair_at(&state, &account.id, sv)?))
}

/// `GET /auth/me` — l'utilisateur authentifié, son rôle et ses permissions
/// (le frontend en déduit les écrans à proposer, ex. l'administration).
#[utoipa::path(
    get,
    path = "/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "Utilisateur courant", body = MeResponse),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu")
    ),
    security(("bearer" = []))
)]
pub async fn me(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
) -> Result<Json<MeResponse>, AccountError> {
    let Some(account) = state
        .users
        .find_by_id(user.user_id())
        .await
        .map_err(internal)?
        .filter(|a| !a.disabled)
    else {
        return Err(AccountError::Unauthorized);
    };
    let granted = state
        .auth
        .permissions
        .permissions_for(&account.id)
        .await
        .map_err(internal)?;
    let mut permissions: Vec<String> = granted.iter().cloned().collect();
    permissions.sort();
    let mfa_enabled = state
        .mfa
        .active(&account.id)
        .await
        .map_err(internal)?
        .is_some();
    Ok(Json(MeResponse {
        role: role_of(&granted).to_string(),
        id: account.id,
        email: account.email,
        permissions,
        mfa_enabled,
    }))
}

/// Refus de la suppression d'un compte administrateur (#229).
const ADMIN_CANNOT_DELETE: &str =
    "an administrator account cannot be deleted: another administrator must first remove its role";

/// `DELETE /auth/me` — efface le compte de l'utilisateur authentifié et tout
/// ce qui lui est rattaché (RGPD, droit à l'effacement), après vérification
/// du mot de passe actuel. Toutes ses sessions sont fermées.
#[utoipa::path(
    delete,
    path = "/auth/me",
    tag = "auth",
    request_body = DeleteAccountBody,
    responses(
        (status = 204, description = "Compte et données effacés ; sessions fermées"),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu"),
        (status = 403, description = "Mot de passe actuel incorrect"),
        (status = 409, description = "Compte administrateur (`*`) : un autre administrateur doit d'abord retirer son rôle"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`"),
        (status = 503, description = "Suppression indisponible sur ce déploiement")
    ),
    security(("bearer" = []))
)]
pub async fn delete_account(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<DeleteAccountBody>,
) -> Result<StatusCode, AccountError> {
    let Some(erasure) = state.erasure.clone() else {
        return Err(AccountError::Unavailable(
            "account deletion is not configured",
        ));
    };
    let user_id = user.user_id().to_string();
    // Même compteur que le changement de mot de passe : un jeton volé ne
    // permet pas de deviner le mot de passe en tentant des suppressions.
    let key = format!("password:{user_id}");
    let now = state.auth.clock.now_unix_seconds();
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&key, MAX_LOGIN_FAILURES, now)
        .await
        .map_err(internal)?
    {
        audit(
            &state,
            &user_id,
            "auth.account_delete",
            AuditOutcome::Denied,
        )
        .await;
        return Err(AccountError::TooManyAttempts(retry_after_secs));
    }
    let Some(account) = state
        .users
        .find_by_id(&user_id)
        .await
        .map_err(internal)?
        .filter(|a| !a.disabled)
    else {
        return Err(AccountError::Unauthorized);
    };
    if state
        .auth
        .permissions
        .permissions_for(&user_id)
        .await
        .map_err(internal)?
        .contains(PERM_WILDCARD)
    {
        audit(
            &state,
            &user_id,
            "auth.account_delete",
            AuditOutcome::Denied,
        )
        .await;
        return Err(AccountError::Conflict(ADMIN_CANNOT_DELETE));
    }

    let hasher = Arc::clone(&state.hasher);
    let (current, phc) = (body.current_password, account.password_hash.clone());
    let matches = blocking(move || Ok(hasher.verify(&current, &phc).unwrap_or(false))).await?;
    if !matches {
        audit(
            &state,
            &user_id,
            "auth.account_delete",
            AuditOutcome::Failure,
        )
        .await;
        return Err(AccountError::Forbidden("current password is incorrect"));
    }

    let Some(erased) = erase_account(&state.auth, &erasure, &account)
        .await
        .map_err(internal)?
    else {
        // Effacé entre-temps (requête concurrente ou administration).
        return Err(AccountError::Unauthorized);
    };
    // En PostgreSQL, le second facteur est déjà parti avec le compte
    // (cascade) ; le store en mémoire, lui, ne connaît pas les comptes.
    if let Err(e) = state.mfa.store.disable(&account.id).await {
        tracing::warn!(error = %e, "second facteur du compte effacé non supprimé");
    }
    audit(
        &state,
        &erased.pseudonym,
        "auth.account_delete",
        AuditOutcome::Success,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Sous-router `/signup`, `/login`, `/refresh`, `/logout`,
/// `/password/forgot`, `/password/reset`, `/email/verify`, `/email/resend`,
/// `/mfa/verify` (publics), `/logout-all`, `/password`, `/me` et `/mfa*`
/// (Bearer, cf. [`crate::mfa`]) — à imbriquer sous `/auth` avec
/// [`crate::auth::protected_router`].
pub fn accounts_router(state: AccountsState) -> Router {
    let mfa = mfa_router(state.clone());
    Router::new()
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/refresh", post(refresh))
        .route("/logout", post(logout))
        .route("/logout-all", post(logout_all))
        .route("/password", post(change_password))
        .route("/password/forgot", post(forgot_password))
        .route("/password/reset", post(reset_password))
        .route("/email/verify", post(verify_email))
        .route("/email/resend", post(resend_verification))
        .route("/invitation/accept", post(accept_invitation))
        .route("/me", get(me).delete(delete_account))
        .with_state(state)
        .merge(mfa)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{protected_router, Clock};
    use crate::erasure::InMemoryAccountEraser;
    use crate::mail::{MailSettings, MemoryMailer, OutgoingMail};
    use crate::password_reset::RESET_TTL_SECONDS;
    use autome_connectors::oauth::InMemoryConnectorTokenStore;
    use autome_core::{
        Argon2PasswordHasher, AuditSink, EnvelopeCipher, InMemoryAuditSink, InMemoryKeyProvider,
        InMemoryPermissionStore, InMemoryRevocationStore, InMemoryRunStateStore,
        InMemoryTriggerScheduleStore, InMemoryUserStore, InMemoryWebhookEndpointStore,
        InMemoryWorkflowRepository, Pagination, PermissionStore, RunCheckpoint, RunStateStore,
        TokenKind, TokenService, WorkflowGraph, WorkflowId, WorkflowRepository, ERASED_PREFIX,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const START: i64 = 1_700_000_000;

    /// Horloge ajustable (fenêtre anti-bruteforce, expirations).
    struct MovableClock(Mutex<i64>);
    impl MovableClock {
        fn advance(&self, secs: i64) {
            *self.0.lock().unwrap() += secs;
        }
    }
    impl Clock for MovableClock {
        fn now_unix_seconds(&self) -> i64 {
            *self.0.lock().unwrap()
        }
    }

    /// Hasher factice (rapide) : `plain:<mdp>`. Argon2 réel testé à part.
    struct PlainHasher;
    impl PasswordHasher for PlainHasher {
        fn hash(&self, plain: &str) -> autome_core::Result<String> {
            Ok(format!("plain:{plain}"))
        }
        fn verify(&self, plain: &str, phc: &str) -> autome_core::Result<bool> {
            match phc.strip_prefix("plain:") {
                Some(stored) => Ok(stored == plain),
                None => Err(autome_core::Error::Invalid("hash mal formé".into())),
            }
        }
    }

    struct Harness {
        app: Router,
        clock: Arc<MovableClock>,
        perms: Arc<InMemoryPermissionStore>,
        users: Arc<InMemoryUserStore>,
        audit: Arc<InMemoryAuditSink>,
        mailer: MemoryMailer,
        workflows: Arc<InMemoryWorkflowRepository>,
        runs: Arc<InMemoryRunStateStore>,
    }

    fn harness_with(hasher: Arc<dyn PasswordHasher + Send + Sync>) -> Harness {
        harness_full(hasher, None)
    }

    fn harness_full(
        hasher: Arc<dyn PasswordHasher + Send + Sync>,
        ip_throttle: Option<IpThrottle>,
    ) -> Harness {
        harness_build(hasher, ip_throttle, false, true)
    }

    /// Harness où la vérification de l'email est exigée (#231).
    fn harness_verify(ip_throttle: Option<IpThrottle>) -> Harness {
        harness_build(Arc::new(PlainHasher), ip_throttle, true, true)
    }

    fn harness_build(
        hasher: Arc<dyn PasswordHasher + Send + Sync>,
        ip_throttle: Option<IpThrottle>,
        email_verification: bool,
        signup_open: bool,
    ) -> Harness {
        let clock = Arc::new(MovableClock(Mutex::new(START)));
        let perms = Arc::new(InMemoryPermissionStore::new());
        let users = Arc::new(InMemoryUserStore::new(perms.clone()));
        let audit = Arc::new(InMemoryAuditSink::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            perms.clone(),
            audit.clone(),
        )
        .with_clock(clock.clone());
        let mailer = MemoryMailer::new();
        let mail = MailSettings {
            mailer: Arc::new(mailer.clone()),
            public_url: "https://app.example.io".into(),
        };
        let reset = PasswordReset::new(ResetTokens::new(b"test-encryption-key"), mail.clone());
        let mut accounts = AccountsState::new(auth.clone(), users.clone(), hasher)
            .unwrap()
            .with_password_reset(reset)
            .with_invitations(invite_tokens())
            .with_signup_open(signup_open);
        if email_verification {
            accounts = accounts.with_email_verification(EmailVerification::new(
                VerifyTokens::new(b"test-encryption-key"),
                mail,
            ));
        }
        if let Some(ip) = ip_throttle {
            accounts = accounts.with_ip_throttle(ip);
        }
        let workflows = Arc::new(InMemoryWorkflowRepository::new());
        let runs = Arc::new(InMemoryRunStateStore::new());
        let eraser = InMemoryAccountEraser::new(
            users.clone(),
            workflows.clone(),
            runs.clone(),
            Arc::new(InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
                InMemoryKeyProvider::with_key("v1", [9u8; 32]),
            ))),
            Arc::new(InMemoryWebhookEndpointStore::new()),
            Arc::new(InMemoryTriggerScheduleStore::new()),
            Some(audit.clone()),
        );
        let erasure = Erasure::new(Arc::new(eraser), accounts.throttle.clone());
        let accounts = accounts.with_erasure(erasure);
        let app = protected_router(auth).merge(accounts_router(accounts));
        Harness {
            app,
            clock,
            perms,
            users,
            audit,
            mailer,
            workflows,
            runs,
        }
    }

    fn harness() -> Harness {
        harness_with(Arc::new(PlainHasher))
    }

    /// Harness avec anti-bruteforce par IP (#190).
    fn harness_ip(login_max: u32, signup_max: u32, trusted: &str) -> Harness {
        let ip = IpThrottle {
            policy: ClientIpPolicy::parse(trusted).unwrap(),
            keyer: IpKeyer::new(b"test-ip-key"),
            login_max,
            signup_max,
            reset_max: 0,
            verify_max: 0,
        };
        harness_full(Arc::new(PlainHasher), Some(ip))
    }

    /// POST depuis le pair `peer`, avec un `X-Forwarded-For` éventuel.
    async fn post_from(
        app: &Router,
        path: &str,
        peer: &str,
        xff: Option<&str>,
        body: serde_json::Value,
    ) -> Response {
        let mut req = Request::post(path).header("content-type", "application/json");
        if let Some(xff) = xff {
            req = req.header("x-forwarded-for", xff);
        }
        let mut req = req.body(Body::from(body.to_string())).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 40_000)));
        app.clone().oneshot(req).await.unwrap()
    }

    async fn post(app: &Router, path: &str, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_bearer(
        app: &Router,
        path: &str,
        bearer: &str,
        body: serde_json::Value,
    ) -> Response {
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .header("Authorization", format!("Bearer {bearer}"))
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete_me(app: &Router, bearer: &str, password: &str) -> Response {
        app.clone()
            .oneshot(
                Request::delete("/me")
                    .header("content-type", "application/json")
                    .header("Authorization", format!("Bearer {bearer}"))
                    .body(Body::from(
                        json!({ "current_password": password }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn whoami_status(app: &Router, access: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::get("/whoami")
                    .header("Authorization", format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn tokens(resp: Response) -> TokenResponse {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn creds(email: &str, password: &str) -> serde_json::Value {
        json!({ "email": email, "password": password })
    }

    fn verify(token: &str) -> autome_core::Claims {
        TokenService::new(SECRET).verify(token, START + 1).unwrap()
    }

    #[tokio::test]
    async fn signup_creates_editor_account_and_returns_token_pair() {
        let h = harness();
        let resp = post(
            &h.app,
            "/signup",
            creds(" Alice@Example.io ", "s3cret-pass"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let pair = tokens(resp).await;
        assert_eq!(pair.token_type, "Bearer");
        assert_eq!(pair.expires_in, ACCESS_TTL_SECONDS);

        let access = verify(&pair.access_token);
        assert_eq!(access.kind, TokenKind::Access);
        assert_eq!(access.exp, START + ACCESS_TTL_SECONDS);
        let refresh = verify(&pair.refresh_token);
        assert_eq!(refresh.kind, TokenKind::Refresh);
        assert_eq!(refresh.sub, access.sub);

        // Email normalisé, mot de passe jamais stocké en clair.
        let user = h
            .users
            .find_by_email("alice@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, access.sub);
        assert_eq!(user.password_hash, "plain:s3cret-pass");
        assert_eq!(
            h.perms.permissions_for(&user.id).await.unwrap(),
            Role::editor().permissions
        );
    }

    #[tokio::test]
    async fn signup_rejects_duplicate_email_case_insensitively() {
        let h = harness();
        post(
            &h.app,
            "/signup",
            creds("bob@example.io", "bob-first-secret"),
        )
        .await;
        let resp = post(
            &h.app,
            "/signup",
            creds("BOB@example.io", "bob-second-secret"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        // Le premier mot de passe reste le bon.
        let ok = post(
            &h.app,
            "/login",
            creds("bob@example.io", "bob-first-secret"),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn signup_rejects_invalid_input() {
        let h = harness();
        let bad_email = post(&h.app, "/signup", creds("not-an-email", "long-enough")).await;
        assert_eq!(bad_email.status(), StatusCode::BAD_REQUEST);
        let short = post(&h.app, "/signup", creds("c@example.io", "short")).await;
        assert_eq!(short.status(), StatusCode::BAD_REQUEST);
        assert!(text(short).await.contains("trop court"));
        let missing_field = post(&h.app, "/signup", json!({ "email": "c@example.io" })).await;
        assert!(missing_field.status().is_client_error());
    }

    #[tokio::test]
    async fn login_succeeds_with_right_password() {
        let h = harness();
        post(&h.app, "/signup", creds("dan@example.io", "correct-horse")).await;
        let resp = post(&h.app, "/login", creds("DAN@example.io", "correct-horse")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let pair = tokens(resp).await;
        let user = h
            .users
            .find_by_email("dan@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verify(&pair.access_token).sub, user.id);
    }

    #[tokio::test]
    async fn login_failures_are_indistinguishable() {
        let h = harness();
        post(&h.app, "/signup", creds("eve@example.io", "correct-horse")).await;
        let wrong = post(&h.app, "/login", creds("eve@example.io", "wrong-horse")).await;
        let unknown = post(&h.app, "/login", creds("nobody@example.io", "whatever-1")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(text(wrong).await, text(unknown).await);
    }

    #[tokio::test]
    async fn login_with_malformed_stored_hash_is_401_not_500() {
        let h = harness();
        h.users
            .create(
                "legacy@example.io",
                "argon2-placeholder",
                &HashSet::new(),
                true,
            )
            .await
            .unwrap();
        let resp = post(&h.app, "/login", creds("legacy@example.io", "anything-1")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login_is_throttled_after_repeated_failures() {
        let h = harness();
        post(&h.app, "/signup", creds("fay@example.io", "correct-horse")).await;
        for _ in 0..MAX_LOGIN_FAILURES {
            let r = post(&h.app, "/login", creds("fay@example.io", "wrong-horse")).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Bloqué, même avec le bon mot de passe.
        let blocked = post(&h.app, "/login", creds("fay@example.io", "correct-horse")).await;
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            blocked.headers()[header::RETRY_AFTER],
            LOGIN_WINDOW_SECONDS.to_string()
        );
        // Un autre compte n'est pas affecté.
        let other = post(&h.app, "/login", creds("other@example.io", "whatever-1")).await;
        assert_eq!(other.status(), StatusCode::UNAUTHORIZED);

        h.clock.advance(LOGIN_WINDOW_SECONDS);
        let ok = post(&h.app, "/login", creds("fay@example.io", "correct-horse")).await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn successful_login_resets_the_counter() {
        let h = harness();
        post(&h.app, "/signup", creds("gus@example.io", "correct-horse")).await;
        for _ in 0..MAX_LOGIN_FAILURES - 1 {
            post(&h.app, "/login", creds("gus@example.io", "wrong-horse")).await;
        }
        let ok = post(&h.app, "/login", creds("gus@example.io", "correct-horse")).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let wrong = post(&h.app, "/login", creds("gus@example.io", "wrong-horse")).await;
        assert_eq!(
            wrong.status(),
            StatusCode::UNAUTHORIZED,
            "compteur remis à zéro"
        );
    }

    #[tokio::test]
    async fn refresh_rotates_and_rejects_replay() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("hal@example.io", "correct-horse")).await).await;

        let resp = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let next = tokens(resp).await;
        assert_ne!(next.refresh_token, pair.refresh_token);
        assert_eq!(
            verify(&next.access_token).sub,
            verify(&pair.access_token).sub
        );

        let replay = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        let with_access = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": next.access_token }),
        )
        .await;
        assert_eq!(with_access.status(), StatusCode::UNAUTHORIZED);
        let garbage = post(&h.app, "/refresh", json!({ "refresh_token": "x.y.z" })).await;
        assert_eq!(garbage.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_revokes_refresh_and_access_of_the_session() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("kim@example.io", "correct-horse")).await).await;
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );

        let resp = post_bearer(
            &h.app,
            "/logout",
            &pair.access_token,
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::UNAUTHORIZED
        );
        let refresh = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(refresh.status(), StatusCode::UNAUTHORIZED);

        // Une nouvelle connexion ouvre une nouvelle session.
        let again =
            tokens(post(&h.app, "/login", creds("kim@example.io", "correct-horse")).await).await;
        assert_eq!(
            whoami_status(&h.app, &again.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_is_idempotent() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("lea@example.io", "correct-horse")).await).await;
        let body = json!({ "refresh_token": pair.refresh_token });
        assert_eq!(
            post(&h.app, "/logout", body.clone()).await.status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            post(&h.app, "/logout", body).await.status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn logout_without_bearer_revokes_only_the_refresh() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("max@example.io", "correct-horse")).await).await;
        let resp = post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let refresh = post(
            &h.app,
            "/refresh",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(refresh.status(), StatusCode::UNAUTHORIZED);
        // L'access non présenté reste valide jusqu'à son expiration (15 min).
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_ignores_a_bearer_of_another_user() {
        let h = harness();
        let alice =
            tokens(post(&h.app, "/signup", creds("ann@example.io", "correct-horse")).await).await;
        let bob =
            tokens(post(&h.app, "/signup", creds("bob@example.io", "correct-horse")).await).await;
        let resp = post_bearer(
            &h.app,
            "/logout",
            &bob.access_token,
            json!({ "refresh_token": alice.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            whoami_status(&h.app, &bob.access_token).await,
            StatusCode::OK
        );
        // Un Bearer illisible n'empêche pas non plus la déconnexion.
        let resp = post_bearer(
            &h.app,
            "/logout",
            "x.y.z",
            json!({ "refresh_token": bob.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn logout_rejects_what_is_not_a_valid_refresh() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("ned@example.io", "correct-horse")).await).await;
        for bad in [pair.access_token.as_str(), "x.y.z", ""] {
            let resp = post(&h.app, "/logout", json!({ "refresh_token": bad })).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad:?}");
        }
        // Refresh expiré : plus rien à révoquer, refusé comme invalide.
        h.clock.advance(REFRESH_TTL_SECONDS + 1);
        let resp = post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let events = h.audit.list().await.unwrap();
        assert!(!events.iter().any(|e| e.action == "auth.logout"));
    }

    #[tokio::test]
    async fn logout_is_audited_under_the_user() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("oli@example.io", "correct-horse")).await).await;
        post(
            &h.app,
            "/logout",
            json!({ "refresh_token": pair.refresh_token }),
        )
        .await;
        let user = verify(&pair.access_token).sub;
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "auth.logout"
            && e.actor == user
            && e.outcome == AuditOutcome::Success));
    }

    async fn refresh_status(app: &Router, refresh_token: &str) -> StatusCode {
        post(app, "/refresh", json!({ "refresh_token": refresh_token }))
            .await
            .status()
    }

    #[tokio::test]
    async fn logout_all_closes_every_session_of_the_user() {
        let h = harness();
        let laptop =
            tokens(post(&h.app, "/signup", creds("pia@example.io", "correct-horse")).await).await;
        let phone =
            tokens(post(&h.app, "/login", creds("pia@example.io", "correct-horse")).await).await;
        // Session rafraîchie avant la déconnexion générale : concernée aussi.
        let rotated = tokens(
            post(
                &h.app,
                "/refresh",
                json!({ "refresh_token": phone.refresh_token }),
            )
            .await,
        )
        .await;
        let bob =
            tokens(post(&h.app, "/signup", creds("bob@example.io", "correct-horse")).await).await;

        let resp = post_bearer(&h.app, "/logout-all", &laptop.access_token, json!({})).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        for pair in [&laptop, &rotated] {
            assert_eq!(
                whoami_status(&h.app, &pair.access_token).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                refresh_status(&h.app, &pair.refresh_token).await,
                StatusCode::UNAUTHORIZED
            );
        }
        // Les autres utilisateurs ne sont pas touchés.
        assert_eq!(
            whoami_status(&h.app, &bob.access_token).await,
            StatusCode::OK
        );
        assert_eq!(
            refresh_status(&h.app, &bob.refresh_token).await,
            StatusCode::OK
        );

        // Une nouvelle connexion ouvre une session valide, à la nouvelle version.
        let again =
            tokens(post(&h.app, "/login", creds("pia@example.io", "correct-horse")).await).await;
        assert_eq!(verify(&again.access_token).sv, 1);
        assert_eq!(
            whoami_status(&h.app, &again.access_token).await,
            StatusCode::OK
        );
        let next = tokens(
            post(
                &h.app,
                "/refresh",
                json!({ "refresh_token": again.refresh_token }),
            )
            .await,
        )
        .await;
        assert_eq!(verify(&next.refresh_token).sv, 1);
        assert_eq!(
            whoami_status(&h.app, &next.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn logout_all_requires_an_access_token() {
        let h = harness();
        let pair = tokens(
            post(
                &h.app,
                "/signup",
                creds("quinn@example.io", "correct-horse"),
            )
            .await,
        )
        .await;
        let missing = post(&h.app, "/logout-all", json!({})).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        let with_refresh = post_bearer(&h.app, "/logout-all", &pair.refresh_token, json!({})).await;
        assert_eq!(with_refresh.status(), StatusCode::UNAUTHORIZED);
        // Rien n'a été fermé.
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
        // Un second logout-all avec un jeton déjà fermé est refusé.
        post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        let again = post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logout_all_is_audited_under_the_user() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("rae@example.io", "correct-horse")).await).await;
        post_bearer(&h.app, "/logout-all", &pair.access_token, json!({})).await;
        let user = verify(&pair.access_token).sub;
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "auth.logout_all"
            && e.actor == user
            && e.outcome == AuditOutcome::Success));
    }

    // ───── Changement de mot de passe (#196) ─────

    fn pw(current: &str, new: &str) -> serde_json::Value {
        json!({ "current_password": current, "new_password": new })
    }

    #[tokio::test]
    async fn changing_the_password_closes_every_session_and_returns_a_new_pair() {
        let h = harness();
        let laptop =
            tokens(post(&h.app, "/signup", creds("sam@example.io", "old-password")).await).await;
        let phone =
            tokens(post(&h.app, "/login", creds("sam@example.io", "old-password")).await).await;

        let r = post_bearer(
            &h.app,
            "/password",
            &laptop.access_token,
            pw("old-password", "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let fresh = tokens(r).await;

        // Toutes les sessions d'avant sont fermées, sur tous les appareils…
        for pair in [&laptop, &phone] {
            assert_eq!(
                whoami_status(&h.app, &pair.access_token).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                refresh_status(&h.app, &pair.refresh_token).await,
                StatusCode::UNAUTHORIZED
            );
        }
        // … l'appareil courant continue avec la nouvelle paire.
        assert_eq!(
            whoami_status(&h.app, &fresh.access_token).await,
            StatusCode::OK
        );
        assert_eq!(
            refresh_status(&h.app, &fresh.refresh_token).await,
            StatusCode::OK
        );
        // L'ancien mot de passe ne connecte plus, le nouveau si.
        let old = post(&h.app, "/login", creds("sam@example.io", "old-password")).await;
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
        let new = post(&h.app, "/login", creds("sam@example.io", "new-password")).await;
        assert_eq!(new.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_wrong_current_password_is_403_and_attempts_are_limited() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("tia@example.io", "real-password")).await).await;
        for _ in 0..MAX_LOGIN_FAILURES {
            let r = post_bearer(
                &h.app,
                "/password",
                &pair.access_token,
                pw("guess-guess", "new-password"),
            )
            .await;
            // 403, pas 401 : le client ne doit pas se croire déconnecté.
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }
        let r = post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("real-password", "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
        // Rien n'a changé, la session est intacte.
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
        // La fenêtre passée (l'access token, de même durée, a expiré : on se
        // reconnecte), le bon mot de passe est accepté.
        h.clock.advance(LOGIN_WINDOW_SECONDS);
        let again =
            tokens(post(&h.app, "/login", creds("tia@example.io", "real-password")).await).await;
        let r = post_bearer(
            &h.app,
            "/password",
            &again.access_token,
            pw("real-password", "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_requests_are_rejected_before_any_change() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("uma@example.io", "real-password")).await).await;
        for (body, expected) in [
            (pw("real-password", "short"), StatusCode::BAD_REQUEST),
            (
                pw("real-password", "real-password"),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let r = post_bearer(&h.app, "/password", &pair.access_token, body).await;
            assert_eq!(r.status(), expected);
        }
        let anonymous = post(&h.app, "/password", pw("real-password", "new-password")).await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let with_refresh = post_bearer(
            &h.app,
            "/password",
            &pair.refresh_token,
            pw("real-password", "new-password"),
        )
        .await;
        assert_eq!(with_refresh.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_token_of_an_unknown_account_cannot_change_a_password() {
        let h = harness();
        let ghost = TokenService::new(SECRET)
            .issue_access("ghost", ACCESS_TTL_SECONDS, START)
            .unwrap();
        let r = post_bearer(&h.app, "/password", &ghost, pw("whatever-1", "whatever-2")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn concurrent_changes_from_the_same_password_have_a_single_winner() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("vic@example.io", "real-password")).await).await;
        let change = |new: &'static str| {
            let (app, access) = (h.app.clone(), pair.access_token.clone());
            async move {
                post_bearer(&app, "/password", &access, pw("real-password", new))
                    .await
                    .status()
            }
        };
        let (a, b) = tokio::join!(change("first-password"), change("second-password"));
        let statuses = [a, b];
        assert_eq!(
            statuses.iter().filter(|s| **s == StatusCode::OK).count(),
            1,
            "{statuses:?}"
        );
        // Le perdant, selon l'entrelacement : son jeton vérifié après que le
        // gagnant a fermé toutes les sessions (401), son mot de passe vérifié
        // après le changement (403), ou son écriture devancée (409).
        assert!(
            statuses.iter().all(|s| [
                StatusCode::OK,
                StatusCode::UNAUTHORIZED,
                StatusCode::FORBIDDEN,
                StatusCode::CONFLICT
            ]
            .contains(s)),
            "{statuses:?}"
        );
        // Un seul nouveau mot de passe est en place.
        let logins = [
            post(&h.app, "/login", creds("vic@example.io", "first-password"))
                .await
                .status(),
            post(&h.app, "/login", creds("vic@example.io", "second-password"))
                .await
                .status(),
        ];
        assert_eq!(
            logins.iter().filter(|s| **s == StatusCode::OK).count(),
            1,
            "{logins:?}"
        );
    }

    #[tokio::test]
    async fn password_changes_are_audited() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("wes@example.io", "real-password")).await).await;
        post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("wrong-pass", "new-password"),
        )
        .await;
        post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("real-password", "new-password"),
        )
        .await;
        let user = verify(&pair.access_token).sub;
        let events = h.audit.list().await.unwrap();
        for outcome in [AuditOutcome::Failure, AuditOutcome::Success] {
            assert!(events.iter().any(|e| e.action == "auth.password_change"
                && e.actor == user
                && e.outcome == outcome));
        }
        assert!(!format!("{events:?}").contains("real-password"));
    }

    // ───── Anti-bruteforce par adresse IP (#190) ─────

    const IP_A: &str = "198.51.100.7";
    const IP_B: &str = "203.0.113.9";

    #[tokio::test]
    async fn password_spraying_from_one_ip_is_blocked_across_emails() {
        let h = harness_ip(3, 50, "");
        for i in 0..3 {
            let r = post_from(
                &h.app,
                "/login",
                IP_A,
                None,
                creds(&format!("victim{i}@example.io"), "Password123!"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Nouvel email, aucun échec sur ce compte : l'IP est bloquée.
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("victim9@example.io", "Password123!"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
        // Une autre adresse n'est pas concernée.
        let r = post_from(
            &h.app,
            "/login",
            IP_B,
            None,
            creds("victim9@example.io", "Password123!"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // La fenêtre passée, l'adresse retrouve son quota.
        h.clock.advance(LOGIN_WINDOW_SECONDS);
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("victim9@example.io", "Password123!"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn successful_logins_do_not_consume_the_ip_quota() {
        let h = harness_ip(2, 50, "");
        post_from(
            &h.app,
            "/signup",
            IP_B,
            None,
            creds("nat@example.io", "correct-horse"),
        )
        .await;
        // Bureau derrière un NAT : beaucoup de connexions réussies, une IP.
        for _ in 0..10 {
            let r = post_from(
                &h.app,
                "/login",
                IP_A,
                None,
                creds("nat@example.io", "correct-horse"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::OK);
        }
        // Les échecs, eux, comptent — et un succès n'efface pas les précédents.
        for _ in 0..2 {
            let r = post_from(
                &h.app,
                "/login",
                IP_A,
                None,
                creds("x@example.io", "wrong-horse"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("nat@example.io", "correct-horse"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn signup_is_limited_per_ip() {
        let h = harness_ip(50, 2, "");
        // Saisie invalide : refusée avant le compteur, ne compte pas.
        let r = post_from(
            &h.app,
            "/signup",
            IP_A,
            None,
            creds("not-an-email", "correct-horse"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        for i in 0..2 {
            let r = post_from(
                &h.app,
                "/signup",
                IP_A,
                None,
                creds(&format!("bulk{i}@example.io"), "correct-horse"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::CREATED);
        }
        let r = post_from(
            &h.app,
            "/signup",
            IP_A,
            None,
            creds("bulk9@example.io", "correct-horse"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(h
            .users
            .find_by_email("bulk9@example.io")
            .await
            .unwrap()
            .is_none());
        let r = post_from(
            &h.app,
            "/signup",
            IP_B,
            None,
            creds("bulk9@example.io", "correct-horse"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        // Quotas séparés : l'inscription n'use pas celui de la connexion.
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("bulk0@example.io", "correct-horse"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_forged_forwarded_for_does_not_reset_the_quota() {
        let h = harness_ip(1, 50, "");
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            Some("1.1.1.1"),
            creds("a@example.io", "nope-nope"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // Le pair n'est pas un proxy de confiance : l'en-tête est ignoré.
        let r = post_from(
            &h.app,
            "/login",
            IP_A,
            Some("2.2.2.2"),
            creds("b@example.io", "nope-nope"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn behind_a_trusted_proxy_each_client_has_its_own_quota() {
        let h = harness_ip(1, 50, "10.0.0.0/8");
        let ingress = "10.0.0.5";
        let r = post_from(
            &h.app,
            "/login",
            ingress,
            Some(IP_A),
            creds("a@example.io", "nope-nope"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = post_from(
            &h.app,
            "/login",
            ingress,
            Some(IP_A),
            creds("b@example.io", "nope-nope"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        // Autre client derrière le même ingress : non bloqué.
        let r = post_from(
            &h.app,
            "/login",
            ingress,
            Some(IP_B),
            creds("c@example.io", "nope-nope"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_zero_quota_disables_the_ip_counter() {
        let h = harness_ip(0, 0, "");
        for i in 0..20 {
            let r = post_from(
                &h.app,
                "/signup",
                IP_A,
                None,
                creds(&format!("z{i}@example.io"), "correct-horse"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::CREATED);
        }
    }

    #[tokio::test]
    async fn the_ip_address_never_reaches_the_audit_trail() {
        let h = harness_ip(1, 1, "");
        post_from(
            &h.app,
            "/signup",
            IP_A,
            None,
            creds("p1@example.io", "correct-horse"),
        )
        .await;
        post_from(
            &h.app,
            "/signup",
            IP_A,
            None,
            creds("p2@example.io", "correct-horse"),
        )
        .await;
        post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("p1@example.io", "wrong-horse"),
        )
        .await;
        post_from(
            &h.app,
            "/login",
            IP_A,
            None,
            creds("p1@example.io", "wrong-horse"),
        )
        .await;
        let events = h.audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "auth.signup" && e.outcome == AuditOutcome::Denied));
        assert!(events
            .iter()
            .any(|e| e.action == "auth.login" && e.outcome == AuditOutcome::Denied));
        let dump = format!("{events:?}");
        assert!(!dump.contains("198.51.100"), "IP présente dans l'audit");
    }

    #[test]
    fn ip_throttle_configuration_from_the_environment() {
        let config = Config::load(|_| None).unwrap();
        let defaults = IpThrottle::from_source(&config, |_| None).unwrap();
        assert_eq!(
            (defaults.login_max, defaults.signup_max, defaults.reset_max),
            (
                DEFAULT_LOGIN_IP_MAX_ATTEMPTS,
                DEFAULT_SIGNUP_IP_MAX_ATTEMPTS,
                DEFAULT_RESET_IP_MAX_ATTEMPTS
            )
        );
        let custom = IpThrottle::from_source(&config, |name| match name {
            "AUTOME_LOGIN_IP_MAX_ATTEMPTS" => Some("100".into()),
            "AUTOME_SIGNUP_IP_MAX_ATTEMPTS" => Some(" 0 ".into()),
            "AUTOME_RESET_IP_MAX_ATTEMPTS" => Some("4".into()),
            "AUTOME_TRUSTED_PROXIES" => Some("10.0.0.0/8".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            (custom.login_max, custom.signup_max, custom.reset_max),
            (100, 0, 4)
        );
        for (name, value) in [
            ("AUTOME_LOGIN_IP_MAX_ATTEMPTS", "-1"),
            ("AUTOME_SIGNUP_IP_MAX_ATTEMPTS", "lots"),
            ("AUTOME_RESET_IP_MAX_ATTEMPTS", "1.5"),
            ("AUTOME_TRUSTED_PROXIES", "10.0.0.0/33"),
        ] {
            let err = IpThrottle::from_source(&config, |n| (n == name).then(|| value.into()))
                .unwrap_err();
            assert!(err.contains(name), "{err}");
        }
    }

    #[tokio::test]
    async fn signup_and_login_are_audited() {
        let h = harness();
        post(&h.app, "/signup", creds("ivy@example.io", "correct-horse")).await;
        post(&h.app, "/login", creds("ivy@example.io", "wrong-horse")).await;
        let events = h.audit.list().await.unwrap();
        let actions: Vec<_> = events
            .iter()
            .map(|e| (e.action.as_str(), e.outcome))
            .collect();
        assert!(actions.contains(&("auth.signup", AuditOutcome::Success)));
        assert!(actions.contains(&("auth.login", AuditOutcome::Failure)));
    }

    // ───── Mot de passe oublié (#204) ─────

    fn forgot(email: &str) -> serde_json::Value {
        json!({ "email": email })
    }

    fn reset_body(token: &str, new_password: &str) -> serde_json::Value {
        json!({ "token": token, "new_password": new_password })
    }

    /// Attend que la tâche de fond ait envoyé `n` emails.
    async fn wait_mails(h: &Harness, n: usize) -> Vec<OutgoingMail> {
        for _ in 0..400 {
            if h.mailer.sent().len() >= n {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        h.mailer.sent()
    }

    /// Attend l'événement d'audit `action`/`outcome` (fin de la tâche de fond).
    async fn wait_audit(h: &Harness, action: &str, outcome: AuditOutcome) {
        for _ in 0..400 {
            let events = h.audit.list().await.unwrap();
            if events
                .iter()
                .any(|e| e.action == action && e.outcome == outcome)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("événement d'audit {action} {outcome:?} attendu");
    }

    /// Jeton du lien `…/#reset=<jeton>` contenu dans l'email.
    fn token_in(mail: &OutgoingMail) -> String {
        let (_, rest) = mail.body.split_once("#reset=").expect("lien attendu");
        rest.split_whitespace().next().unwrap().to_string()
    }

    /// Inscrit `email` et renvoie le jeton de réinitialisation reçu.
    async fn reset_token_for(h: &Harness, email: &str, password: &str) -> String {
        post(&h.app, "/signup", creds(email, password)).await;
        let before = h.mailer.sent().len();
        let r = post(&h.app, "/password/forgot", forgot(email)).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let mails = wait_mails(h, before + 1).await;
        token_in(mails.last().expect("email envoyé"))
    }

    #[tokio::test]
    async fn a_reset_link_replaces_the_password_and_closes_every_session() {
        let h = harness();
        let laptop =
            tokens(post(&h.app, "/signup", creds("ana@example.io", "old-password")).await).await;
        let phone =
            tokens(post(&h.app, "/login", creds("ANA@example.io", "old-password")).await).await;

        let r = post(&h.app, "/password/forgot", forgot(" Ana@Example.io ")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let mails = wait_mails(&h, 1).await;
        assert_eq!(mails.len(), 1);
        assert_eq!(mails[0].to, "ana@example.io");
        assert!(mails[0].body.contains("https://app.example.io/#reset="));
        let token = token_in(&mails[0]);

        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        for pair in [&laptop, &phone] {
            assert_eq!(
                whoami_status(&h.app, &pair.access_token).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                refresh_status(&h.app, &pair.refresh_token).await,
                StatusCode::UNAUTHORIZED
            );
        }
        let old = post(&h.app, "/login", creds("ana@example.io", "old-password")).await;
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
        let new = post(&h.app, "/login", creds("ana@example.io", "new-password")).await;
        assert_eq!(new.status(), StatusCode::OK);

        // Usage unique : le hash a changé, le lien ne vaut plus rien.
        let again = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "third-password"),
        )
        .await;
        assert_eq!(again.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(again).await, INVALID_RESET_TOKEN);
    }

    #[tokio::test]
    async fn an_unknown_email_gets_the_same_answer_and_no_mail() {
        let h = harness();
        post(&h.app, "/signup", creds("bea@example.io", "some-password")).await;
        let r = post(&h.app, "/password/forgot", forgot("nobody@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert_eq!(text(r).await, "");
        wait_audit(&h, "auth.password_reset_request", AuditOutcome::Failure).await;
        assert!(h.mailer.sent().is_empty());
    }

    #[tokio::test]
    async fn a_malformed_email_is_rejected() {
        let h = harness();
        let r = post(&h.app, "/password/forgot", forgot("pas-une-adresse")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_reset_link_expires() {
        let h = harness();
        let token = reset_token_for(&h, "cid@example.io", "old-password").await;
        h.clock.advance(RESET_TTL_SECONDS);
        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let old = post(&h.app, "/login", creds("cid@example.io", "old-password")).await;
        assert_eq!(old.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_password_change_invalidates_pending_links() {
        let h = harness();
        let token = reset_token_for(&h, "dan@example.io", "old-password").await;
        let pair =
            tokens(post(&h.app, "/login", creds("dan@example.io", "old-password")).await).await;
        let r = post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("old-password", "changed-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn forged_tokens_and_weak_passwords_are_refused() {
        let h = harness();
        let token = reset_token_for(&h, "eve@example.io", "old-password").await;
        let user = h
            .users
            .find_by_email("eve@example.io")
            .await
            .unwrap()
            .unwrap();

        // Jeton signé par une autre clé, ou falsifié, ou absurde.
        let foreign = ResetTokens::new(b"other-key").issue(&user.id, &user.password_hash, START);
        // Dernier chiffre du MAC toujours modifié (`0` ⇒ `1`, sinon `0`).
        let last = if token.ends_with('0') { '1' } else { '0' };
        let tampered = format!("{}{last}", &token[..token.len() - 1]);
        for bad in [foreign.as_str(), tampered.as_str(), "", "x.y.z"] {
            let r = post(&h.app, "/password/reset", reset_body(bad, "new-password")).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        }

        // Mot de passe trop court : refusé, et le lien reste utilisable.
        let r = post(&h.app, "/password/reset", reset_body(&token, "short")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn concurrent_uses_of_one_link_have_a_single_winner() {
        let h = harness();
        let token = reset_token_for(&h, "fay@example.io", "old-password").await;
        let (a, b) = tokio::join!(
            post(
                &h.app,
                "/password/reset",
                reset_body(&token, "first-password")
            ),
            post(
                &h.app,
                "/password/reset",
                reset_body(&token, "second-password")
            ),
        );
        let mut statuses = [a.status(), b.status()];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::NO_CONTENT, StatusCode::BAD_REQUEST]);
    }

    #[tokio::test]
    async fn requests_are_limited_per_email() {
        let h = harness();
        post(&h.app, "/signup", creds("gus@example.io", "some-password")).await;
        for _ in 0..crate::password_reset::DEFAULT_RESET_EMAIL_MAX_REQUESTS {
            let r = post(&h.app, "/password/forgot", forgot("gus@example.io")).await;
            assert_eq!(r.status(), StatusCode::ACCEPTED);
        }
        let r = post(&h.app, "/password/forgot", forgot("gus@example.io")).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
        // Le quota vaut aussi pour une adresse sans compte (rien n'est révélé).
        for _ in 0..crate::password_reset::DEFAULT_RESET_EMAIL_MAX_REQUESTS {
            post(&h.app, "/password/forgot", forgot("ghost@example.io")).await;
        }
        let r = post(&h.app, "/password/forgot", forgot("ghost@example.io")).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            wait_mails(&h, 3).await.len(),
            3,
            "un email par demande acceptée pour le compte existant"
        );
    }

    #[tokio::test]
    async fn requests_are_limited_per_ip() {
        let ip = IpThrottle {
            policy: ClientIpPolicy::parse("").unwrap(),
            keyer: IpKeyer::new(b"test-ip-key"),
            login_max: 0,
            signup_max: 0,
            reset_max: 2,
            verify_max: 0,
        };
        let h = harness_full(Arc::new(PlainHasher), Some(ip));
        for email in ["a@example.io", "b@example.io"] {
            let r = post_from(&h.app, "/password/forgot", IP_A, None, forgot(email)).await;
            assert_eq!(r.status(), StatusCode::ACCEPTED);
        }
        let r = post_from(
            &h.app,
            "/password/forgot",
            IP_A,
            None,
            forgot("c@example.io"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        let r = post_from(
            &h.app,
            "/password/forgot",
            IP_B,
            None,
            forgot("c@example.io"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn a_reset_lifts_the_login_lockout() {
        let h = harness();
        let token = reset_token_for(&h, "hal@example.io", "old-password").await;
        for _ in 0..MAX_LOGIN_FAILURES {
            post(&h.app, "/login", creds("hal@example.io", "wrong-password")).await;
        }
        let locked = post(&h.app, "/login", creds("hal@example.io", "old-password")).await;
        assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);

        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let r = post(&h.app, "/login", creds("hal@example.io", "new-password")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn without_a_mailer_the_endpoints_answer_503() {
        let perms = Arc::new(InMemoryPermissionStore::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            perms.clone(),
            Arc::new(InMemoryAuditSink::new()),
        );
        let accounts = AccountsState::new(
            auth,
            Arc::new(InMemoryUserStore::new(perms)),
            Arc::new(PlainHasher),
        )
        .unwrap();
        let app = accounts_router(accounts);
        let r = post(&app, "/password/forgot", forgot("ivy@example.io")).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let r = post(&app, "/password/reset", reset_body("x.1.y", "new-password")).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn resets_are_audited_without_secrets() {
        let h = harness();
        let token = reset_token_for(&h, "jo@example.io", "old-password").await;
        wait_audit(&h, "auth.password_reset_request", AuditOutcome::Success).await;
        let user = h
            .users
            .find_by_email("jo@example.io")
            .await
            .unwrap()
            .unwrap();
        let forged = ResetTokens::new(b"other-key").issue(&user.id, &user.password_hash, START);
        post(
            &h.app,
            "/password/reset",
            reset_body(&forged, "new-password"),
        )
        .await;
        post(
            &h.app,
            "/password/reset",
            reset_body(&token, "new-password"),
        )
        .await;
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "auth.password_reset"
            && e.actor == user.id
            && e.outcome == AuditOutcome::Success));
        assert!(events.iter().any(|e| e.action == "auth.password_reset"
            && e.actor == user.id
            && e.outcome == AuditOutcome::Failure));
        let dump = format!("{events:?}");
        assert!(!dump.contains(&token[token.len() - 64..]), "MAC du jeton");
        assert!(!dump.contains("new-password"));
    }

    // ───── Comptes suspendus et `/auth/me` (#208) ─────

    async fn get_bearer(app: &Router, path: &str, bearer: &str) -> Response {
        app.clone()
            .oneshot(
                Request::get(path)
                    .header("Authorization", format!("Bearer {bearer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Inscrit `email` puis suspend le compte ; renvoie la paire d'avant.
    async fn disabled_account(h: &Harness, email: &str, password: &str) -> TokenResponse {
        let pair = tokens(post(&h.app, "/signup", creds(email, password)).await).await;
        let id = h.users.find_by_email(email).await.unwrap().unwrap().id;
        assert!(h.users.set_disabled(&id, true).await.unwrap());
        pair
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_log_in_and_only_its_owner_learns_why() {
        let h = harness();
        disabled_account(&h, "eve@example.io", "right-password").await;
        let r = post(&h.app, "/login", creds("eve@example.io", "right-password")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, ACCOUNT_DISABLED);
        // Mauvais mot de passe : la suspension n'est pas révélée.
        let r = post(&h.app, "/login", creds("eve@example.io", "wrong-password")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let denied = h
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "auth.login" && e.outcome == AuditOutcome::Denied)
            .count();
        assert_eq!(denied, 1);

        // Réactivé : la connexion fonctionne de nouveau.
        let id = h
            .users
            .find_by_email("eve@example.io")
            .await
            .unwrap()
            .unwrap()
            .id;
        h.users.set_disabled(&id, false).await.unwrap();
        let r = post(&h.app, "/login", creds("eve@example.io", "right-password")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn me_reports_identity_role_and_permissions() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("fay@example.io", "some-password")).await).await;
        let r = get_bearer(&h.app, "/me", &pair.access_token).await;
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let me: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(me.email, "fay@example.io");
        assert_eq!(me.id, verify(&pair.access_token).sub);
        assert_eq!(me.role, "editor");
        let mut expected: Vec<String> = Role::editor().permissions.into_iter().collect();
        expected.sort();
        assert_eq!(me.permissions, expected);

        h.perms.grant(&me.id, autome_core::PERM_WILDCARD).unwrap();
        let bytes = get_bearer(&h.app, "/me", &pair.access_token)
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let me: MeResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(me.role, "admin");

        let r = get_bearer(&h.app, "/me", "not-a-token").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_disabled_account_token_is_worthless_for_me_and_password_change() {
        let h = harness();
        // Jeton émis avant la suspension, sans fermeture des sessions : cas
        // d'une suspension concurrente (l'administration incrémente aussi la
        // version de session).
        let pair = disabled_account(&h, "gus@example.io", "old-password").await;
        let r = get_bearer(&h.app, "/me", &pair.access_token).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("old-password", "new-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let stored = h
            .users
            .find_by_email("gus@example.io")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.password_hash, "plain:old-password");
    }

    #[tokio::test]
    async fn a_disabled_account_gets_no_reset_link() {
        let h = harness();
        disabled_account(&h, "hal@example.io", "some-password").await;
        let r = post(&h.app, "/password/forgot", forgot("hal@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "réponse inchangée");
        wait_audit(&h, "auth.password_reset_request", AuditOutcome::Denied).await;
        assert!(h.mailer.sent().is_empty());
    }

    // ───── Suppression de compte (#229) ─────

    /// Inscrit `email` avec un workflow et un run ; renvoie `(id, paire)`.
    async fn account_with_data(h: &Harness, email: &str) -> (String, TokenResponse) {
        let pair = tokens(post(&h.app, "/signup", creds(email, "the-password")).await).await;
        let id = h.users.find_by_email(email).await.unwrap().unwrap().id;
        h.workflows
            .create(
                &id,
                WorkflowGraph {
                    id: WorkflowId::new("wf-1"),
                    name: "wf".into(),
                    nodes: vec![],
                    edges: vec![],
                },
            )
            .await
            .unwrap();
        h.runs
            .checkpoint(
                &RunCheckpoint::queued(format!("run-{email}"), "wf-1", 1, "manual", 1)
                    .with_owner(Some(id.clone())),
            )
            .await
            .unwrap();
        (id, pair)
    }

    async fn workflow_count(h: &Harness, owner: &str) -> usize {
        h.workflows
            .list(owner, Pagination::default_paging())
            .await
            .unwrap()
            .total
    }

    #[tokio::test]
    async fn deleting_the_account_erases_its_data_and_closes_every_session() {
        let h = harness();
        let (sam, laptop) = account_with_data(&h, "sam@example.io").await;
        let phone =
            tokens(post(&h.app, "/login", creds("sam@example.io", "the-password")).await).await;
        // Un échec de connexion journalisé sous l'email.
        post(&h.app, "/login", creds("sam@example.io", "wrong-password")).await;
        let (bob, _) = account_with_data(&h, "bob@example.io").await;

        let r = delete_me(&h.app, &laptop.access_token, "the-password").await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        // Audit lu tout de suite : les requêtes suivantes en ajoutent.
        let events = h.audit.list().await.unwrap();

        // Plus aucune session, plus de compte.
        for pair in [&laptop, &phone] {
            assert_eq!(
                whoami_status(&h.app, &pair.access_token).await,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                refresh_status(&h.app, &pair.refresh_token).await,
                StatusCode::UNAUTHORIZED
            );
        }
        let login = post(&h.app, "/login", creds("sam@example.io", "the-password")).await;
        assert_eq!(login.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(h.users.find_by_id(&sam).await.unwrap(), None);
        assert!(h.perms.permissions_for(&sam).await.unwrap().is_empty());
        assert_eq!(workflow_count(&h, &sam).await, 0);
        assert!(h.runs.load("run-sam@example.io").await.unwrap().is_none());

        // Audit : ni id ni email, l'effacement tracé sous un pseudonyme.
        assert!(events.iter().all(|e| e.actor != sam
            && e.actor != "sam@example.io"
            && e.resource.as_deref() != Some(sam.as_str())));
        let deleted = events
            .iter()
            .find(|e| e.action == "auth.account_delete")
            .expect("effacement audité");
        assert_eq!(deleted.outcome, AuditOutcome::Success);
        assert!(deleted.actor.starts_with(ERASED_PREFIX));
        assert!(
            events.iter().filter(|e| e.actor == deleted.actor).count() > 1,
            "événements du compte regroupés sous le pseudonyme"
        );

        // Bob intact ; l'email de sam redevient libre.
        assert!(h.users.find_by_id(&bob).await.unwrap().is_some());
        assert_eq!(workflow_count(&h, &bob).await, 1);
        assert!(h.runs.load("run-bob@example.io").await.unwrap().is_some());
        let again = post(&h.app, "/signup", creds("sam@example.io", "new-password")).await;
        assert_eq!(again.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_wrong_password_keeps_the_account_and_attempts_are_limited() {
        let h = harness();
        let (sam, pair) = account_with_data(&h, "sam@example.io").await;
        for _ in 0..MAX_LOGIN_FAILURES {
            let r = delete_me(&h.app, &pair.access_token, "wrong-password").await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }
        let r = delete_me(&h.app, &pair.access_token, "the-password").await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
        assert!(h.users.find_by_id(&sam).await.unwrap().is_some());
        assert_eq!(workflow_count(&h, &sam).await, 1);
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK,
            "session intacte"
        );
        let failures = h
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "auth.account_delete" && e.actor == sam)
            .count();
        assert_eq!(failures, MAX_LOGIN_FAILURES as usize + 1);
    }

    #[tokio::test]
    async fn an_administrator_cannot_delete_its_own_account() {
        let h = harness();
        let (root, pair) = account_with_data(&h, "root@example.io").await;
        h.perms
            .replace(&root, &Role::super_admin().permissions)
            .unwrap();
        let r = delete_me(&h.app, &pair.access_token, "the-password").await;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(text(r).await, ADMIN_CANNOT_DELETE);
        assert!(h.users.find_by_id(&root).await.unwrap().is_some());
        assert_eq!(workflow_count(&h, &root).await, 1);
    }

    #[tokio::test]
    async fn a_suspended_or_vanished_account_cannot_delete_itself() {
        let h = harness();
        let pair = disabled_account(&h, "eve@example.io", "the-password").await;
        let r = delete_me(&h.app, &pair.access_token, "the-password").await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(h
            .users
            .find_by_email("eve@example.io")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn without_an_eraser_deletion_answers_503() {
        let h = harness();
        let pair =
            tokens(post(&h.app, "/signup", creds("sam@example.io", "the-password")).await).await;
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            h.perms.clone(),
            h.audit.clone(),
        )
        .with_clock(h.clock.clone());
        let accounts =
            AccountsState::new(auth.clone(), h.users.clone(), Arc::new(PlainHasher)).unwrap();
        let app = protected_router(auth).merge(accounts_router(accounts));
        let r = delete_me(&app, &pair.access_token, "the-password").await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(h
            .users
            .find_by_email("sam@example.io")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn real_argon2_end_to_end_signup_login_whoami() {
        let h = harness_with(Arc::new(Argon2PasswordHasher::new()));
        let signup = post(&h.app, "/signup", creds("joe@example.io", "correct-horse")).await;
        assert_eq!(signup.status(), StatusCode::CREATED);
        let stored = h
            .users
            .find_by_email("joe@example.io")
            .await
            .unwrap()
            .unwrap();
        assert!(stored.password_hash.starts_with("$argon2id$"));

        let pair =
            tokens(post(&h.app, "/login", creds("joe@example.io", "correct-horse")).await).await;
        let whoami = h
            .app
            .clone()
            .oneshot(
                Request::get("/whoami")
                    .header("Authorization", format!("Bearer {}", pair.access_token))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(whoami.status(), StatusCode::OK);
        assert_eq!(text(whoami).await, format!("hello {}", stored.id));

        let wrong = post(&h.app, "/login", creds("joe@example.io", "wrong-horse")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }

    // ── Vérification de l'email (#231) ──────────────────────────────────

    /// Jeton du lien `…/#verify=<jeton>` contenu dans l'email.
    fn verify_token_in(mail: &OutgoingMail) -> String {
        let (_, rest) = mail.body.split_once("#verify=").expect("lien attendu");
        rest.split_whitespace().next().unwrap().to_string()
    }

    fn verify_body(token: &str) -> serde_json::Value {
        json!({ "token": token })
    }

    /// Inscrit `email` (vérification exigée) et renvoie le jeton reçu.
    async fn signup_pending(h: &Harness, email: &str, password: &str) -> String {
        let before = h.mailer.sent().len();
        let r = post(&h.app, "/signup", creds(email, password)).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let mails = wait_mails(h, before + 1).await;
        verify_token_in(mails.last().expect("email envoyé"))
    }

    #[tokio::test]
    async fn signup_sends_a_link_and_login_waits_for_it() {
        let h = harness_verify(None);
        let r = post(&h.app, "/signup", creds(" Eva@Example.io ", "eva-password")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let body: serde_json::Value = serde_json::from_str(&text(r).await).unwrap();
        assert_eq!(
            body,
            json!({ "email": "eva@example.io", "verification_required": true }),
            "aucun jeton avant vérification"
        );
        let mails = wait_mails(&h, 1).await;
        assert_eq!(mails[0].to, "eva@example.io");
        assert_eq!(
            mails[0].subject,
            crate::email_verification::VERIFY_MAIL_SUBJECT
        );
        assert!(mails[0].body.contains("https://app.example.io/#verify="));
        let token = verify_token_in(&mails[0]);
        let user = h
            .users
            .find_by_email("eva@example.io")
            .await
            .unwrap()
            .unwrap();
        assert!(!user.email_verified);
        wait_audit(&h, "auth.email_verify_request", AuditOutcome::Success).await;

        // Bon mot de passe, adresse non vérifiée : 403 explicite. Mauvais mot
        // de passe : le 401 habituel (rien n'est révélé à un tiers).
        let r = post(&h.app, "/login", creds("eva@example.io", "eva-password")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, EMAIL_NOT_VERIFIED);
        let r = post(&h.app, "/login", creds("eva@example.io", "wrong-password")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let r = post(&h.app, "/email/verify", verify_body(&token)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(
            h.users
                .find_by_id(&user.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        let r = post(&h.app, "/login", creds("eva@example.io", "eva-password")).await;
        assert_eq!(r.status(), StatusCode::OK);
        let pair = tokens(r).await;
        assert_eq!(verify(&pair.access_token).sub, user.id);

        // Lien rouvert : sans effet, toujours accepté avant expiration.
        let r = post(&h.app, "/email/verify", verify_body(&token)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        let events = h.audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "auth.login" && e.outcome == AuditOutcome::Denied));
        assert!(events.iter().any(|e| e.action == "auth.email_verify"
            && e.outcome == AuditOutcome::Success
            && e.actor == user.id));
    }

    #[tokio::test]
    async fn bad_expired_or_foreign_verification_links_are_refused() {
        let h = harness_verify(None);
        let token = signup_pending(&h, "fay@example.io", "fay-password").await;
        let user = h
            .users
            .find_by_email("fay@example.io")
            .await
            .unwrap()
            .unwrap();

        // Mal formé, falsifié, ou jeton de réinitialisation du même compte.
        let mut forged = token.clone();
        forged.replace_range(
            forged.len() - 1..,
            if token.ends_with('0') { "1" } else { "0" },
        );
        let reset = ResetTokens::new(b"test-encryption-key").issue(&user.id, &user.email, START);
        for bad in ["", "pas-un-jeton", forged.as_str(), reset.as_str()] {
            let r = post(&h.app, "/email/verify", verify_body(bad)).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad:?}");
            assert_eq!(text(r).await, INVALID_VERIFY_TOKEN);
        }

        h.clock
            .advance(crate::email_verification::VERIFY_TTL_SECONDS);
        let r = post(&h.app, "/email/verify", verify_body(&token)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "expiré");
        assert!(
            !h.users
                .find_by_id(&user.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        wait_audit(&h, "auth.email_verify", AuditOutcome::Failure).await;
    }

    #[tokio::test]
    async fn the_link_can_be_resent_only_to_unverified_accounts() {
        let h = harness_verify(None);
        let first = signup_pending(&h, "gus@example.io", "gus-password").await;

        // Inconnu : même réponse, aucun email.
        let r = post(&h.app, "/email/resend", forgot("nobody@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        wait_audit(&h, "auth.email_verify_request", AuditOutcome::Failure).await;
        assert_eq!(h.mailer.sent().len(), 1);

        // Non vérifié : nouveau lien, l'ancien reste valable.
        h.clock.advance(1);
        let r = post(&h.app, "/email/resend", forgot("GUS@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let mails = wait_mails(&h, 2).await;
        let second = verify_token_in(&mails[1]);
        assert_ne!(first, second);
        let r = post(&h.app, "/email/verify", verify_body(&first)).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        // Vérifié : plus rien n'est envoyé.
        let r = post(&h.app, "/email/resend", forgot("gus@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        wait_audit(&h, "auth.email_verify_request", AuditOutcome::Denied).await;
        assert_eq!(h.mailer.sent().len(), 2);

        // Adresse invalide : 400 ; quota par adresse épuisé : 429.
        let r = post(&h.app, "/email/resend", forgot("pas-une-adresse")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        for _ in 0..crate::email_verification::DEFAULT_VERIFY_EMAIL_MAX_REQUESTS {
            post(&h.app, "/email/resend", forgot("flood@example.io")).await;
        }
        let r = post(&h.app, "/email/resend", forgot("flood@example.io")).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn resending_is_limited_per_ip() {
        let ip = IpThrottle {
            policy: ClientIpPolicy::parse("").unwrap(),
            keyer: IpKeyer::new(b"test-ip-key"),
            login_max: 0,
            signup_max: 0,
            reset_max: 0,
            verify_max: 2,
        };
        let h = harness_verify(Some(ip));
        for email in ["a@example.io", "b@example.io"] {
            let r = post_from(&h.app, "/email/resend", "203.0.113.7", None, forgot(email)).await;
            assert_eq!(r.status(), StatusCode::ACCEPTED);
        }
        let r = post_from(
            &h.app,
            "/email/resend",
            "203.0.113.7",
            None,
            forgot("c@example.io"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        let r = post_from(
            &h.app,
            "/email/resend",
            "198.51.100.1",
            None,
            forgot("c@example.io"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "autre adresse IP");
    }

    #[tokio::test]
    async fn a_password_reset_also_verifies_the_address() {
        let h = harness_verify(None);
        // Adresse « réservée » par un tiers qui ne la possède pas : la
        // titulaire reprend le compte par le mot de passe oublié.
        signup_pending(&h, "hal@example.io", "squatter-password").await;
        let r = post(&h.app, "/password/forgot", forgot("hal@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let mails = wait_mails(&h, 2).await;
        let token = token_in(&mails[1]);
        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "owner-password"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let r = post(&h.app, "/login", creds("hal@example.io", "owner-password")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn without_verification_signup_logs_in_and_the_endpoints_are_unavailable() {
        let h = harness();
        let r = post(&h.app, "/signup", creds("ivy@example.io", "ivy-password")).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        tokens(r).await;
        assert!(
            h.users
                .find_by_email("ivy@example.io")
                .await
                .unwrap()
                .unwrap()
                .email_verified,
            "créé vérifié"
        );
        assert!(h.mailer.sent().is_empty());
        let r = post(&h.app, "/email/verify", verify_body("x")).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let r = post(&h.app, "/email/resend", forgot("ivy@example.io")).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // --- Invitations (#241) -------------------------------------------------

    fn invite_tokens() -> InviteTokens {
        InviteTokens::new(b"test-encryption-key")
    }

    /// Compte invité (comme `POST /admin/users`) : `(id, hash inutilisable)`.
    async fn invited(h: &Harness, email: &str) -> (String, String) {
        let hash = autome_core::unusable_password_hash();
        let CreateUser::Created(user) = h
            .users
            .create(email, &hash, &Role::viewer().permissions, false)
            .await
            .unwrap()
        else {
            panic!("création");
        };
        (user.id, hash)
    }

    fn accept_body(token: &str, password: &str) -> serde_json::Value {
        json!({ "token": token, "password": password })
    }

    #[tokio::test]
    async fn an_invitation_is_accepted_once_and_logs_in() {
        let h = harness_verify(None);
        let (id, hash) = invited(&h, "ivy@example.io").await;
        let token = invite_tokens().issue(&id, &hash, START);

        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let pair = tokens(r).await;
        assert_eq!(verify(&pair.access_token).sub, id);
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
        let account = h.users.find_by_id(&id).await.unwrap().unwrap();
        assert_eq!(account.password_hash, "plain:chosen-pw");
        assert!(account.email_verified, "le lien prouve la boîte");
        // Rôle choisi par l'administration conservé.
        assert_eq!(
            h.perms.permissions_for(&id).await.unwrap(),
            Role::viewer().permissions
        );
        // Connexion ordinaire ensuite (vérification exigée : déjà faite).
        let r = post(&h.app, "/login", creds("ivy@example.io", "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::OK);

        // Usage unique.
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "other-pw!"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(r).await, INVALID_INVITE_TOKEN);
        let actions: Vec<_> = h
            .audit
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "auth.invitation_accept")
            .map(|e| (e.actor, e.outcome))
            .collect();
        assert_eq!(
            actions,
            [
                (id.clone(), AuditOutcome::Success),
                (id, AuditOutcome::Failure)
            ]
        );
    }

    #[tokio::test]
    async fn a_resent_invitation_kills_the_previous_link() {
        let h = harness();
        let (id, first) = invited(&h, "ivy@example.io").await;
        let old = invite_tokens().issue(&id, &first, START);
        // Renvoi (`POST /admin/users/{id}/invitation`) : nouveau hash.
        let second = autome_core::unusable_password_hash();
        assert!(h.users.update_password(&id, &first, &second).await.unwrap());
        let r = post(&h.app, "/invitation/accept", accept_body(&old, "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let new = invite_tokens().issue(&id, &second, START);
        let r = post(&h.app, "/invitation/accept", accept_body(&new, "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn expired_forged_or_foreign_links_are_refused() {
        let h = harness();
        let (id, hash) = invited(&h, "ivy@example.io").await;
        let good = invite_tokens().issue(&id, &hash, START);

        // Mot de passe trop court : refusé avant tout, le lien reste valable.
        let r = post(&h.app, "/invitation/accept", accept_body(&good, "short")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        // Autres liens (réinitialisation), clé étrangère, jeton mal formé.
        let reset = ResetTokens::new(b"test-encryption-key").issue(&id, &hash, START);
        let foreign = InviteTokens::new(b"other-key").issue(&id, &hash, START);
        for bad in [reset.as_str(), foreign.as_str(), "nope"] {
            let r = post(&h.app, "/invitation/accept", accept_body(bad, "chosen-pw")).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        // Un compte qui a déjà un mot de passe n'accepte aucune invitation.
        post(&h.app, "/signup", creds("bob@example.io", "bob-password")).await;
        let bob = h
            .users
            .find_by_email("bob@example.io")
            .await
            .unwrap()
            .unwrap();
        let t = invite_tokens().issue(&bob.id, &bob.password_hash, START);
        let r = post(&h.app, "/invitation/accept", accept_body(&t, "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            h.users
                .find_by_id(&bob.id)
                .await
                .unwrap()
                .unwrap()
                .password_hash,
            "plain:bob-password"
        );
        // Expiré.
        h.clock.advance(crate::invitation::INVITE_TTL_SECONDS);
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&good, "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            h.users
                .find_by_id(&id)
                .await
                .unwrap()
                .unwrap()
                .password_hash,
            hash
        );
    }

    #[tokio::test]
    async fn a_suspended_invitee_cannot_accept_until_reactivated() {
        let h = harness();
        let (id, hash) = invited(&h, "ivy@example.io").await;
        let token = invite_tokens().issue(&id, &hash, START);
        h.users.set_disabled(&id, true).await.unwrap();
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, ACCOUNT_DISABLED);
        assert_eq!(
            h.users
                .find_by_id(&id)
                .await
                .unwrap()
                .unwrap()
                .password_hash,
            hash
        );
        h.users.set_disabled(&id, false).await.unwrap();
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    /// Hasher qui compte ses vérifications et refuse les hashs non PHC (comme
    /// Argon2) : un compte invité doit payer une vérification comme un autre.
    struct CountingHasher(std::sync::atomic::AtomicUsize);
    impl PasswordHasher for CountingHasher {
        fn hash(&self, plain: &str) -> autome_core::Result<String> {
            PlainHasher.hash(plain)
        }
        fn verify(&self, plain: &str, phc: &str) -> autome_core::Result<bool> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            PlainHasher.verify(plain, phc)
        }
    }

    #[tokio::test]
    async fn an_invited_account_cannot_log_in_and_looks_unknown() {
        let hasher = Arc::new(CountingHasher(Default::default()));
        let h = harness_with(hasher.clone());
        let (_, hash) = invited(&h, "ivy@example.io").await;
        for (email, password) in [
            ("ivy@example.io", hash.as_str()),
            ("nobody@example.io", "x"),
        ] {
            let before = hasher.0.load(std::sync::atomic::Ordering::SeqCst);
            let r = post(&h.app, "/login", creds(email, password)).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{email}");
            assert_eq!(text(r).await, "invalid credentials");
            assert_eq!(
                hasher.0.load(std::sync::atomic::Ordering::SeqCst),
                before + 1,
                "une vérification (hash leurre) pour {email}"
            );
        }
    }

    #[tokio::test]
    async fn a_forgotten_invitation_is_recovered_by_a_password_reset() {
        let h = harness_verify(None);
        let (id, _) = invited(&h, "ivy@example.io").await;
        let r = post(&h.app, "/password/forgot", forgot("ivy@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let token = token_in(wait_mails(&h, 1).await.last().unwrap());
        let r = post(&h.app, "/password/reset", reset_body(&token, "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(
            h.users
                .find_by_id(&id)
                .await
                .unwrap()
                .unwrap()
                .email_verified
        );
        let r = post(&h.app, "/login", creds("ivy@example.io", "chosen-pw")).await;
        assert_eq!(r.status(), StatusCode::OK);
        // Le renvoi du lien de vérification, lui, ne lui sert à rien.
        let before = h.mailer.sent().len();
        let (_, _) = invited(&h, "joe@example.io").await;
        let r = post(&h.app, "/email/resend", forgot("joe@example.io")).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        wait_audit(&h, "auth.email_verify_request", AuditOutcome::Denied).await;
        assert_eq!(h.mailer.sent().len(), before);
    }

    #[tokio::test]
    async fn closed_signup_is_refused_before_any_work() {
        let h = harness_build(Arc::new(PlainHasher), None, false, false);
        let r = post(&h.app, "/signup", creds("eve@example.io", "eve-password")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, SIGNUP_CLOSED);
        assert!(h
            .users
            .find_by_email("eve@example.io")
            .await
            .unwrap()
            .is_none());
        let events = h.audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "auth.signup" && e.outcome == AuditOutcome::Denied));
        // Les invitations, elles, restent acceptées.
        let (id, hash) = invited(&h, "ivy@example.io").await;
        let token = invite_tokens().issue(&id, &hash, START);
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn without_invitations_acceptance_is_503() {
        let perms = Arc::new(InMemoryPermissionStore::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            perms.clone(),
            Arc::new(InMemoryAuditSink::new()),
        );
        let users = Arc::new(InMemoryUserStore::new(perms));
        let state = AccountsState::new(auth, users, Arc::new(PlainHasher)).unwrap();
        let r = post(
            &accounts_router(state),
            "/invitation/accept",
            accept_body("x.1.y", "chosen-pw"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ───── Mots de passe courants ou compromis (#245) ─────

    #[tokio::test]
    async fn signup_refuses_common_and_contextual_passwords() {
        let h = harness();
        for weak in [
            "Password123!",
            "azertyuiop",
            "zoe.martin2026",
            "Autome2026!",
        ] {
            let r = post(&h.app, "/signup", creds("zoe.martin@example.io", weak)).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{weak}");
            assert_eq!(text(r).await, COMPROMISED_PASSWORD_MESSAGE);
        }
        assert!(h
            .users
            .find_by_email("zoe.martin@example.io")
            .await
            .unwrap()
            .is_none());
        let r = post(
            &h.app,
            "/signup",
            creds("zoe.martin@example.io", "vélo rouge sous la pluie"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn a_refused_new_password_does_not_count_as_a_guess() {
        let h = harness();
        let pair = tokens(
            post(
                &h.app,
                "/signup",
                creds("tom@example.io", "tom-real-secret"),
            )
            .await,
        )
        .await;
        // Le titulaire cherche un mot de passe accepté : jamais bloqué.
        for _ in 0..=MAX_LOGIN_FAILURES {
            let r = post_bearer(
                &h.app,
                "/password",
                &pair.access_token,
                pw("tom-real-secret", "Qwerty2024!"),
            )
            .await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            assert_eq!(text(r).await, COMPROMISED_PASSWORD_MESSAGE);
        }
        // Rien n'a changé : la session est intacte, l'ancien mot de passe aussi.
        assert_eq!(
            whoami_status(&h.app, &pair.access_token).await,
            StatusCode::OK
        );
        let r = post_bearer(
            &h.app,
            "/password",
            &pair.access_token,
            pw("tom-real-secret", "tom-new-long-secret"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reset_and_invitation_refuse_compromised_passwords_and_keep_the_link() {
        let h = harness();
        let token = reset_token_for(&h, "una@example.io", "una-old-secret").await;
        let r = post(&h.app, "/password/reset", reset_body(&token, "iloveyou2")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(r).await, COMPROMISED_PASSWORD_MESSAGE);
        // Le lien n'a pas été consommé.
        let r = post(
            &h.app,
            "/password/reset",
            reset_body(&token, "una-new-long-secret"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        let (id, hash) = invited(&h, "ivo@example.io").await;
        let token = invite_tokens().issue(&id, &hash, START);
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "12345678"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(r).await, COMPROMISED_PASSWORD_MESSAGE);
        let r = post(
            &h.app,
            "/invitation/accept",
            accept_body(&token, "ivo-chosen-secret"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    /// Corpus distant factice : un seul mot de passe divulgué.
    struct OneLeak(&'static str);

    #[async_trait::async_trait]
    impl autome_core::BreachedPasswordSource for OneLeak {
        async fn is_breached(&self, password: &str) -> autome_core::Result<bool> {
            Ok(password == self.0)
        }
    }

    #[tokio::test]
    async fn the_breach_corpus_applies_when_choosing_never_when_logging_in() {
        let perms = Arc::new(InMemoryPermissionStore::new());
        let auth = AuthState::new(
            TokenService::new(SECRET),
            Arc::new(InMemoryRevocationStore::new()),
            perms.clone(),
            Arc::new(InMemoryAuditSink::new()),
        );
        let users = Arc::new(InMemoryUserStore::new(perms));
        let leaked = "a long but leaked phrase";
        let state = AccountsState::new(auth, users.clone(), Arc::new(PlainHasher))
            .unwrap()
            .with_password_policy(
                PasswordPolicy::default().with_breached_source(Arc::new(OneLeak(leaked))),
            );
        let app = accounts_router(state);
        let r = post(&app, "/signup", creds("leo@example.io", leaked)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(r).await, COMPROMISED_PASSWORD_MESSAGE);

        // Un compte existant dont le mot de passe vient d'apparaître dans une
        // fuite se connecte toujours : seul un nouveau choix est contrôlé.
        users
            .create(
                "old@example.io",
                &format!("plain:{leaked}"),
                &Role::editor().permissions,
                true,
            )
            .await
            .unwrap();
        let r = post(&app, "/login", creds("old@example.io", leaked)).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}
