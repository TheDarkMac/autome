//! Second facteur TOTP et codes de secours (#245).
//!
//! | Méthode | Path                      | Body                              | Succès |
//! |---------|---------------------------|-----------------------------------|--------|
//! | GET     | `/auth/mfa`               | — (Bearer)                        | 200 + [`MfaStatus`] |
//! | POST    | `/auth/mfa/setup`         | `{ current_password }` (Bearer)   | 200 + [`MfaSetup`] |
//! | POST    | `/auth/mfa/enable`        | `{ code }` (Bearer)               | 200 + [`MfaEnabled`] |
//! | POST    | `/auth/mfa/disable`       | `{ current_password, code }` (Bearer) | 204 |
//! | POST    | `/auth/mfa/recovery-codes`| `{ code }` (Bearer)               | 200 + [`RecoveryCodes`] |
//! | POST    | `/auth/mfa/verify`        | `{ mfa_token, code }`             | 200 + [`TokenResponse`] |
//!
//! **Facultative** : chaque utilisateur l'active pour son compte. Une fois
//! active, `/auth/login` ne délivre plus de jetons sur le seul mot de passe :
//! il répond **202** [`MfaChallenge`], un jeton signé de 5 minutes qui
//! n'ouvre aucune session, à échanger avec un code contre une paire sur
//! `/auth/mfa/verify`.
//!
//! Choix de sécurité :
//! - **TOTP RFC 6238** (HMAC-SHA-1, 6 chiffres, pas de 30 s, ±1 pas de
//!   tolérance d'horloge) : le profil que toutes les applications
//!   d'authentification acceptent. Secret de 160 bits (RFC 4226 §4).
//! - Le secret est **chiffré au repos** (AES-256-GCM, clé dérivée
//!   d'`ENCRYPTION_KEY` sous un domaine propre) : une copie de la base ne
//!   suffit pas à produire des codes.
//! - **Anti-rejeu** : un code n'est accepté que pour un pas de temps
//!   strictement postérieur au dernier accepté (écriture conditionnelle,
//!   cf. [`MfaStore::consume_step`]) — un code vu par-dessus l'épaule ou
//!   intercepté ne sert pas une seconde fois, même en concurrence.
//! - **Codes de secours** : 10 codes de 50 bits à usage unique, montrés une
//!   seule fois, stockés sous forme d'empreinte HMAC-SHA-256 à clé dérivée
//!   d'`ENCRYPTION_KEY` (la base seule ne permet pas de les retrouver par
//!   force brute). Saisie tolérante (casse, tirets, espaces, `o`/`i`/`l`).
//! - **Essais limités** : par compte (`mfa:<id>`, comme la connexion) et,
//!   pour `/auth/mfa/verify`, par adresse IP (quota de connexion). Le jeton de
//!   défi est vérifié **avant** de compter un essai sur le compte : sans
//!   connaître le mot de passe, on ne peut pas bloquer le second facteur
//!   d'autrui.
//! - Le jeton de défi est lié à la **version de session** et au **hash du
//!   mot de passe** : un changement de mot de passe ou une fermeture de toutes
//!   les sessions le périme.
//! - **Activer** la MFA ferme toutes les autres sessions (ouvertes sur le
//!   seul mot de passe, peut-être par un tiers) et renvoie une paire neuve.
//!   **Désactiver** exige le mot de passe **et** un code. Sur les routes
//!   authentifiées, un code ou un mot de passe faux répond **403** (pas 401 :
//!   le client se croirait déconnecté).
//! - Appareil perdu sans codes de secours : un administrateur réinitialise la
//!   MFA du compte (`DELETE /admin/users/{id}/mfa`, cf. [`crate::admin`]), ou
//!   l'opérateur (`autome-api reset-mfa <email>`).
//! - Audit : `auth.mfa_setup`, `auth.mfa_enable`, `auth.mfa_disable`,
//!   `auth.mfa_recovery_codes`, `auth.mfa_challenge` (mot de passe correct,
//!   code demandé) et `auth.login` à la vérification (méthode en
//!   métadonnées). Jamais un code ni un secret.

use std::net::SocketAddr;
use std::sync::Arc;

use autome_connectors::sdk::http::percent_encode;
use autome_core::{
    has_usable_password, AuditEvent, AuditOutcome, Config, EnvelopeCipher, InMemoryKeyProvider,
    InMemoryMfaStore, LoginAttempt, MfaRecord, MfaStore, MAX_LOGIN_FAILURES,
};
use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use crate::accounts::{
    blocking, check_ip, internal, ip_counter, is_enabled, issue_pair_at, AccountError,
    AccountsState, TokenResponse, ACCOUNT_DISABLED, EMAIL_NOT_VERIFIED, LOGIN_IP_SCOPE,
};
use crate::auth::AuthenticatedUser;
use crate::signed_link::{hex, LinkTokens, ParsedLinkToken};

/// Durée d'un pas TOTP (secondes).
pub const TOTP_PERIOD: i64 = 30;
/// Chiffres d'un code TOTP.
pub const TOTP_DIGITS: u32 = 6;
/// Pas de tolérance de part et d'autre (dérive d'horloge du téléphone).
const TOTP_SKEW: i64 = 1;
/// Taille du secret TOTP (160 bits, RFC 4226 §4).
pub const SECRET_LEN: usize = 20;
/// Nombre de codes de secours émis.
pub const RECOVERY_CODE_COUNT: usize = 10;
/// Symboles d'un code de secours (base32 de Crockford : ni `i`, `l`, `o`, `u`).
const RECOVERY_ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
/// Symboles d'un code de secours (2 × 5, affichés `xxxxx-xxxxx`) : 50 bits.
const RECOVERY_CODE_LEN: usize = 10;
/// Durée de validité d'un défi de connexion (5 min).
pub const MFA_CHALLENGE_TTL_SECONDS: i64 = 5 * 60;
/// Émetteur affiché par les applications d'authentification, par défaut.
pub const DEFAULT_ISSUER: &str = "Autome";

const CHALLENGE_DOMAIN: &str = "autome/mfa-challenge/v1";
const SECRET_DOMAIN: &str = "autome/mfa-secret/v1";
const RECOVERY_DOMAIN: &str = "autome/mfa-recovery/v1";

/// Défi refusé (mal formé, expiré, falsifié, mot de passe changé, sessions
/// fermées, MFA désactivée entre-temps) : le client recommence la connexion.
pub const INVALID_MFA_CHALLENGE: &str = "invalid or expired mfa challenge";
/// Code TOTP ou de secours refusé sur une route authentifiée (403).
pub const INVALID_MFA_CODE: &str = "invalid mfa code";
/// MFA déjà active (409).
pub const MFA_ALREADY_ENABLED: &str = "mfa already enabled";
/// Activation sans enrôlement en attente (409).
pub const MFA_SETUP_REQUIRED: &str = "mfa setup required";
/// Désactivation ou codes de secours sans MFA active (409).
pub const MFA_NOT_ENABLED: &str = "mfa not enabled";

// ─────────────────────────── TOTP (RFC 6238) ───────────────────────────

/// HOTP (RFC 4226) : `digits` derniers chiffres de la troncature dynamique
/// de HMAC-SHA-1(`secret`, `counter`).
pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepte toute longueur");
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = usize::from(hash[hash.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        hash[offset] & 0x7f,
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ]);
    binary % 10u32.pow(digits)
}

/// Pas TOTP de l'instant `now` (secondes Unix).
pub fn totp_step(now: i64) -> i64 {
    now.div_euclid(TOTP_PERIOD)
}

/// Pas TOTP dont `code` (6 chiffres, espaces tolérés) est le code, parmi
/// l'instant `now` ± [`TOTP_SKEW`] pas ; `None` si aucun ne correspond. Les
/// trois pas sont toujours calculés (durée indépendante du résultat).
pub fn verify_totp(secret: &[u8], code: &str, now: i64) -> Option<i64> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != TOTP_DIGITS as usize || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let wanted: u32 = digits.parse().ok()?;
    let current = totp_step(now);
    let mut matched = None;
    for step in current - TOTP_SKEW..=current + TOTP_SKEW {
        let Ok(counter) = u64::try_from(step) else {
            continue;
        };
        if hotp(secret, counter, TOTP_DIGITS) == wanted {
            matched = Some(step);
        }
    }
    matched
}

/// Base32 RFC 4648 sans remplissage (format des secrets `otpauth://`).
pub fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u16, 0u8);
    for &b in bytes {
        buffer = (buffer << 8) | u16::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[usize::from((buffer >> bits) & 0x1f)]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(
            ALPHABET[usize::from((buffer << (5 - bits)) & 0x1f)],
        ));
    }
    out
}

/// URI `otpauth://totp/…` d'enrôlement (format *Key Uri* reconnu par les
/// applications d'authentification).
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let issuer = percent_encode(issuer);
    format!(
        "otpauth://totp/{issuer}:{}?secret={secret_b32}&issuer={issuer}\
         &algorithm=SHA1&digits={TOTP_DIGITS}&period={TOTP_PERIOD}",
        percent_encode(account)
    )
}

/// QR code SVG de `uri` (scanné par l'application d'authentification).
pub fn qr_svg(uri: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .build())
}

// ─────────────────────────── codes de secours ───────────────────────────

/// Tire [`RECOVERY_CODE_COUNT`] codes de secours distincts (`xxxxx-xxxxx`).
pub fn generate_recovery_codes() -> Vec<String> {
    let mut rng = rand::thread_rng();
    let mut codes: Vec<String> = Vec::with_capacity(RECOVERY_CODE_COUNT);
    while codes.len() < RECOVERY_CODE_COUNT {
        let mut bytes = [0u8; RECOVERY_CODE_LEN];
        rng.fill_bytes(&mut bytes);
        let symbols: String = bytes
            .iter()
            .map(|b| char::from(RECOVERY_ALPHABET[usize::from(b & 0x1f)]))
            .collect();
        let code = format!("{}-{}", &symbols[..5], &symbols[5..]);
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes
}

/// Forme canonique d'un code de secours saisi : minuscules, sans tirets ni
/// espaces, `o` ⇒ `0`, `i`/`l` ⇒ `1` ; `None` s'il n'en a pas la forme.
pub fn normalize_recovery_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_lowercase() {
            'o' => '0',
            'i' | 'l' => '1',
            other => other,
        })
        .collect();
    (code.len() == RECOVERY_CODE_LEN && code.bytes().all(|b| RECOVERY_ALPHABET.contains(&b)))
        .then_some(code)
}

// ─────────────────────────── service ───────────────────────────

/// Moyen de second facteur accepté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaMethod {
    /// Code TOTP de l'application d'authentification.
    Totp,
    /// Code de secours (brûlé).
    RecoveryCode,
}

impl MfaMethod {
    fn as_str(self) -> &'static str {
        match self {
            MfaMethod::Totp => "totp",
            MfaMethod::RecoveryCode => "recovery_code",
        }
    }
}

struct Keys {
    cipher: EnvelopeCipher<InMemoryKeyProvider>,
    recovery: [u8; 32],
    challenges: LinkTokens,
}

/// Second facteur : store, clés dérivées d'`ENCRYPTION_KEY` et émetteur.
#[derive(Clone)]
pub struct Mfa {
    /// Persistance des enrôlements.
    pub store: Arc<dyn MfaStore>,
    keys: Arc<Keys>,
    issuer: String,
}

impl std::fmt::Debug for Mfa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mfa")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

fn derive(domain: &str, secret: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(secret);
    hasher.finalize().into()
}

impl Mfa {
    /// Clés dérivées de `secret` (séparation de domaine), codes affichés sous
    /// l'émetteur `issuer`.
    pub fn new(store: Arc<dyn MfaStore>, secret: &[u8], issuer: &str) -> Self {
        Self {
            store,
            keys: Arc::new(Keys {
                cipher: EnvelopeCipher::new(InMemoryKeyProvider::with_key(
                    "v1",
                    derive(SECRET_DOMAIN, secret),
                )),
                recovery: derive(RECOVERY_DOMAIN, secret),
                challenges: LinkTokens::new(CHALLENGE_DOMAIN, secret, MFA_CHALLENGE_TTL_SECONDS),
            }),
            issuer: issuer.to_string(),
        }
    }

    /// Clés dérivées d'`ENCRYPTION_KEY` (identiques sur toutes les
    /// répliques) ; émetteur `AUTOME_MFA_ISSUER` (défaut « Autome »). Changer
    /// la clé rend les secrets illisibles : la MFA de chaque compte est alors
    /// à réinitialiser.
    pub fn from_config(store: Arc<dyn MfaStore>, config: &Config) -> Self {
        let issuer = std::env::var("AUTOME_MFA_ISSUER")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_ISSUER.to_string());
        Self::new(store, config.encryption_key.expose().as_bytes(), &issuer)
    }

    /// Store en mémoire et clé aléatoire (tests, état par défaut).
    pub fn in_memory() -> Self {
        let mut secret = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        Self::new(Arc::new(InMemoryMfaStore::new()), &secret, DEFAULT_ISSUER)
    }

    /// Émetteur affiché par les applications d'authentification.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    fn seal(&self, secret: &[u8]) -> autome_core::Result<String> {
        self.keys.cipher.encrypt(secret)
    }

    fn open(&self, sealed: &str) -> autome_core::Result<Vec<u8>> {
        self.keys.cipher.decrypt(sealed)
    }

    /// Empreinte stockée d'un code de secours **normalisé**.
    fn recovery_hash(&self, normalized: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.keys.recovery)
            .expect("HMAC accepte toute longueur");
        mac.update(normalized.as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    fn recovery_hashes(&self, codes: &[String]) -> Vec<String> {
        codes
            .iter()
            .filter_map(|c| normalize_recovery_code(c))
            .map(|c| self.recovery_hash(&c))
            .collect()
    }

    /// Jeton de défi pour `user_id` dans l'état `(session_version, hash)`.
    fn issue_challenge(&self, user_id: &str, sv: i64, password_hash: &str, now: i64) -> String {
        self.keys
            .challenges
            .issue(user_id, &challenge_bound(sv, password_hash), now)
    }

    fn verify_challenge(
        &self,
        token: &ParsedLinkToken<'_>,
        sv: i64,
        password_hash: &str,
        now: i64,
    ) -> bool {
        self.keys
            .challenges
            .verify(token, &challenge_bound(sv, password_hash), now)
    }

    /// Vérifie `code` (TOTP, ou à défaut code de secours) pour la MFA
    /// **active** `record` de `user_id`, et le consomme. `None` si refusé
    /// (faux, rejoué, déjà utilisé).
    pub async fn verify_code(
        &self,
        user_id: &str,
        record: &MfaRecord,
        code: &str,
        now: i64,
    ) -> autome_core::Result<Option<MfaMethod>> {
        if !record.enabled {
            return Ok(None);
        }
        let secret = self.open(&record.secret)?;
        if let Some(step) = verify_totp(&secret, code, now) {
            return Ok(self
                .store
                .consume_step(user_id, step)
                .await?
                .then_some(MfaMethod::Totp));
        }
        let Some(normalized) = normalize_recovery_code(code) else {
            return Ok(None);
        };
        Ok(self
            .store
            .consume_recovery_code(user_id, &self.recovery_hash(&normalized))
            .await?
            .then_some(MfaMethod::RecoveryCode))
    }

    /// MFA **active** de `user_id`, ou `None`.
    pub async fn active(&self, user_id: &str) -> autome_core::Result<Option<MfaRecord>> {
        Ok(self.store.get(user_id).await?.filter(|r| r.enabled))
    }
}

fn challenge_bound(sv: i64, password_hash: &str) -> String {
    format!("{sv}\0{password_hash}")
}

/// Clé du compteur d'essais de codes d'un compte.
pub fn mfa_throttle_key(user_id: &str) -> String {
    format!("mfa:{user_id}")
}

// ─────────────────────────── contrat HTTP ───────────────────────────

/// Réponse 202 de `/auth/login` quand la MFA est active : aucun jeton de
/// session, un défi à présenter avec un code à `/auth/mfa/verify`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MfaChallenge {
    /// Toujours `true`.
    pub mfa_required: bool,
    /// Jeton de défi (5 min), à renvoyer tel quel.
    pub mfa_token: String,
    /// Durée de validité du défi, en secondes.
    pub expires_in: i64,
}

/// Body de `/auth/mfa/verify`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MfaVerifyBody {
    /// Jeton de défi reçu de `/auth/login`.
    pub mfa_token: String,
    /// Code à 6 chiffres de l'application, ou code de secours.
    pub code: String,
}

/// Body de `/auth/mfa/setup`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MfaSetupBody {
    /// Mot de passe actuel (preuve de possession du compte).
    pub current_password: String,
}

/// Body de `/auth/mfa/enable` et `/auth/mfa/recovery-codes`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MfaCodeBody {
    /// Code à 6 chiffres de l'application (ou code de secours, sauf pour
    /// l'activation).
    pub code: String,
}

/// Body de `/auth/mfa/disable`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MfaDisableBody {
    /// Mot de passe actuel.
    pub current_password: String,
    /// Code à 6 chiffres de l'application, ou code de secours.
    pub code: String,
}

/// Enrôlement en attente : à scanner, puis confirmer par un code.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MfaSetup {
    /// Secret en base32 (saisie manuelle dans l'application).
    pub secret: String,
    /// URI `otpauth://totp/…`.
    pub otpauth_uri: String,
    /// QR code SVG de l'URI.
    pub qr_svg: String,
}

/// MFA activée : codes de secours (montrés **une seule fois**) et nouvelle
/// paire (toutes les autres sessions sont fermées).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MfaEnabled {
    /// Codes de secours à usage unique.
    pub recovery_codes: Vec<String>,
    /// Nouvelle paire de jetons de l'appareil courant.
    pub session: TokenResponse,
}

/// Nouveaux codes de secours (les précédents ne valent plus rien).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct RecoveryCodes {
    /// Codes de secours à usage unique.
    pub recovery_codes: Vec<String>,
}

/// État du second facteur du compte courant.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MfaStatus {
    /// MFA active : exigée à la connexion.
    pub enabled: bool,
    /// Enrôlement commencé, pas encore confirmé par un code.
    pub pending: bool,
    /// Codes de secours encore utilisables.
    pub recovery_codes_left: usize,
}

async fn audit(state: &AccountsState, actor: &str, action: &str, outcome: AuditOutcome) {
    record_event(
        state,
        AuditEvent::new(now_ms(state), actor, action, outcome),
    )
    .await;
}

async fn record_event(state: &AccountsState, event: AuditEvent) {
    let action = event.action.clone();
    if let Err(e) = state.auth.audit.record(event).await {
        tracing::error!(action, error = %e, "événement d'audit non enregistré");
    }
}

fn now_ms(state: &AccountsState) -> i64 {
    state.auth.clock.now_unix_seconds().saturating_mul(1_000)
}

/// Réserve un essai sur le compteur `key` ; 429 (audité `action`) si épuisé.
async fn reserve(
    state: &AccountsState,
    key: &str,
    user_id: &str,
    action: &str,
    now: i64,
) -> Result<(), AccountError> {
    match state
        .throttle
        .begin_attempt(key, MAX_LOGIN_FAILURES, now)
        .await
        .map_err(internal)?
    {
        LoginAttempt::Allowed => Ok(()),
        LoginAttempt::Blocked { retry_after_secs } => {
            audit(state, user_id, action, AuditOutcome::Denied).await;
            Err(AccountError::TooManyAttempts(retry_after_secs))
        }
    }
}

async fn reset_counter(state: &AccountsState, key: &str) {
    if let Err(e) = state.throttle.reset(key).await {
        tracing::warn!(error = %e, "remise à zéro d'un compteur impossible");
    }
}

/// Compte du jeton, s'il existe, n'est pas suspendu et a un mot de passe.
async fn current_account(
    state: &AccountsState,
    user_id: &str,
) -> Result<autome_core::UserRecord, AccountError> {
    state
        .users
        .find_by_id(user_id)
        .await
        .map_err(internal)?
        .filter(|a| !a.disabled && has_usable_password(&a.password_hash))
        .ok_or(AccountError::Unauthorized)
}

/// Vérifie le mot de passe actuel (Argon2 hors runtime) ; 403 s'il est faux.
async fn check_password(
    state: &AccountsState,
    account: &autome_core::UserRecord,
    password: String,
    action: &str,
) -> Result<(), AccountError> {
    let hasher = Arc::clone(&state.hasher);
    let phc = account.password_hash.clone();
    let matches = blocking(move || Ok(hasher.verify(&password, &phc).unwrap_or(false))).await?;
    if matches {
        return Ok(());
    }
    audit(state, &account.id, action, AuditOutcome::Failure).await;
    Err(AccountError::Forbidden("current password is incorrect"))
}

/// Défi de connexion pour un compte dont la MFA est active (appelé par
/// `/auth/login` après le mot de passe, la suspension et la vérification de
/// l'adresse) ; `None` si la MFA n'est pas active.
pub(crate) async fn login_challenge(
    state: &AccountsState,
    user: &autome_core::UserRecord,
    sv: i64,
) -> Result<Option<MfaChallenge>, AccountError> {
    if state
        .mfa
        .active(&user.id)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Ok(None);
    }
    let now = state.auth.clock.now_unix_seconds();
    audit(state, &user.id, "auth.mfa_challenge", AuditOutcome::Success).await;
    Ok(Some(MfaChallenge {
        mfa_required: true,
        mfa_token: state
            .mfa
            .issue_challenge(&user.id, sv, &user.password_hash, now),
        expires_in: MFA_CHALLENGE_TTL_SECONDS,
    }))
}

// ─────────────────────────── handlers ───────────────────────────

/// `GET /auth/mfa` — état du second facteur du compte courant.
#[utoipa::path(
    get,
    path = "/auth/mfa",
    tag = "auth",
    responses(
        (status = 200, description = "État du second facteur", body = MfaStatus),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu")
    ),
    security(("bearer" = []))
)]
pub async fn mfa_status(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
) -> Result<Json<MfaStatus>, AccountError> {
    let account = current_account(&state, user.user_id()).await?;
    let record = state.mfa.store.get(&account.id).await.map_err(internal)?;
    Ok(Json(MfaStatus {
        enabled: record.as_ref().is_some_and(|r| r.enabled),
        pending: record.as_ref().is_some_and(|r| !r.enabled),
        recovery_codes_left: record
            .filter(|r| r.enabled)
            .map_or(0, |r| r.recovery_codes_left),
    }))
}

/// `POST /auth/mfa/setup` — commence l'enrôlement : nouveau secret en
/// attente (remplace un enrôlement non confirmé), à scanner puis confirmer
/// par `/auth/mfa/enable`.
#[utoipa::path(
    post,
    path = "/auth/mfa/setup",
    tag = "auth",
    request_body = MfaSetupBody,
    responses(
        (status = 200, description = "Secret en attente de confirmation", body = MfaSetup),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu"),
        (status = 403, description = "Mot de passe actuel incorrect"),
        (status = 409, description = "MFA déjà active (`mfa already enabled`)"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`")
    ),
    security(("bearer" = []))
)]
pub async fn mfa_setup(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<MfaSetupBody>,
) -> Result<Json<MfaSetup>, AccountError> {
    const ACTION: &str = "auth.mfa_setup";
    let user_id = user.user_id().to_string();
    let now = state.auth.clock.now_unix_seconds();
    // Même compteur que le changement de mot de passe (#196).
    let key = format!("password:{user_id}");
    reserve(&state, &key, &user_id, ACTION, now).await?;
    let account = current_account(&state, &user_id).await?;
    check_password(&state, &account, body.current_password, ACTION).await?;
    reset_counter(&state, &key).await;

    let mut secret = [0u8; SECRET_LEN];
    rand::thread_rng().fill_bytes(&mut secret);
    let sealed = state.mfa.seal(&secret).map_err(internal)?;
    if !state
        .mfa
        .store
        .begin_enrollment(&account.id, &sealed)
        .await
        .map_err(internal)?
    {
        audit(&state, &account.id, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::Conflict(MFA_ALREADY_ENABLED));
    }
    let encoded = base32(&secret);
    let uri = otpauth_uri(state.mfa.issuer(), &account.email, &encoded);
    let qr = qr_svg(&uri).map_err(internal)?;
    audit(&state, &account.id, ACTION, AuditOutcome::Success).await;
    Ok(Json(MfaSetup {
        secret: encoded,
        otpauth_uri: uri,
        qr_svg: qr,
    }))
}

/// `POST /auth/mfa/enable` — confirme l'enrôlement par un premier code :
/// la MFA devient active, toutes les autres sessions sont fermées, les codes
/// de secours sont renvoyés (une seule fois) avec une paire neuve.
#[utoipa::path(
    post,
    path = "/auth/mfa/enable",
    tag = "auth",
    request_body = MfaCodeBody,
    responses(
        (status = 200, description = "MFA active ; autres sessions fermées", body = MfaEnabled),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu"),
        (status = 403, description = "Code incorrect (`invalid mfa code`)"),
        (status = 409, description = "Aucun enrôlement en attente (`mfa setup required`) ou MFA déjà active"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`")
    ),
    security(("bearer" = []))
)]
pub async fn mfa_enable(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<MfaCodeBody>,
) -> Result<Json<MfaEnabled>, AccountError> {
    const ACTION: &str = "auth.mfa_enable";
    let user_id = user.user_id().to_string();
    let now = state.auth.clock.now_unix_seconds();
    let key = mfa_throttle_key(&user_id);
    reserve(&state, &key, &user_id, ACTION, now).await?;
    let account = current_account(&state, &user_id).await?;
    let record = match state.mfa.store.get(&account.id).await.map_err(internal)? {
        None => return Err(AccountError::Conflict(MFA_SETUP_REQUIRED)),
        Some(r) if r.enabled => return Err(AccountError::Conflict(MFA_ALREADY_ENABLED)),
        Some(r) => r,
    };
    let secret = state.mfa.open(&record.secret).map_err(internal)?;
    let Some(step) = verify_totp(&secret, &body.code, now) else {
        audit(&state, &account.id, ACTION, AuditOutcome::Failure).await;
        return Err(AccountError::Forbidden(INVALID_MFA_CODE));
    };
    let codes = generate_recovery_codes();
    if !state
        .mfa
        .store
        .enable(
            &account.id,
            &record.secret,
            step,
            &state.mfa.recovery_hashes(&codes),
        )
        .await
        .map_err(internal)?
    {
        // Nouvel enrôlement ou activation concurrente depuis la lecture.
        return Err(AccountError::Conflict(MFA_SETUP_REQUIRED));
    }
    reset_counter(&state, &key).await;
    // Les sessions ouvertes sur le seul mot de passe (peut-être par un tiers)
    // sont fermées ; la nouvelle paire porte la nouvelle version.
    let version = state
        .auth
        .revocation
        .bump_session_version(&account.id)
        .await
        .map_err(internal)?;
    tracing::info!(user_id = %account.id, "second facteur activé, sessions fermées");
    audit(&state, &account.id, ACTION, AuditOutcome::Success).await;
    if !is_enabled(&state, &account.id).await? {
        return Err(AccountError::Unauthorized);
    }
    Ok(Json(MfaEnabled {
        recovery_codes: codes,
        session: issue_pair_at(&state, &account.id, version)?,
    }))
}

/// `POST /auth/mfa/disable` — désactive la MFA (mot de passe **et** code
/// exigés) ; les codes de secours sont effacés.
#[utoipa::path(
    post,
    path = "/auth/mfa/disable",
    tag = "auth",
    request_body = MfaDisableBody,
    responses(
        (status = 204, description = "MFA désactivée"),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu"),
        (status = 403, description = "Mot de passe actuel ou code incorrect"),
        (status = 409, description = "MFA inactive (`mfa not enabled`)"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`")
    ),
    security(("bearer" = []))
)]
pub async fn mfa_disable(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<MfaDisableBody>,
) -> Result<StatusCode, AccountError> {
    const ACTION: &str = "auth.mfa_disable";
    let user_id = user.user_id().to_string();
    let now = state.auth.clock.now_unix_seconds();
    let password_key = format!("password:{user_id}");
    reserve(&state, &password_key, &user_id, ACTION, now).await?;
    let account = current_account(&state, &user_id).await?;
    check_password(&state, &account, body.current_password, ACTION).await?;
    reset_counter(&state, &password_key).await;
    let Some(record) = state.mfa.active(&account.id).await.map_err(internal)? else {
        return Err(AccountError::Conflict(MFA_NOT_ENABLED));
    };
    let key = mfa_throttle_key(&user_id);
    reserve(&state, &key, &user_id, ACTION, now).await?;
    if state
        .mfa
        .verify_code(&account.id, &record, &body.code, now)
        .await
        .map_err(internal)?
        .is_none()
    {
        audit(&state, &account.id, ACTION, AuditOutcome::Failure).await;
        return Err(AccountError::Forbidden(INVALID_MFA_CODE));
    }
    state
        .mfa
        .store
        .disable(&account.id)
        .await
        .map_err(internal)?;
    reset_counter(&state, &key).await;
    tracing::info!(user_id = %account.id, "second facteur désactivé");
    audit(&state, &account.id, ACTION, AuditOutcome::Success).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/mfa/recovery-codes` — remplace les codes de secours (un code
/// valide exigé) ; les précédents ne valent plus rien.
#[utoipa::path(
    post,
    path = "/auth/mfa/recovery-codes",
    tag = "auth",
    request_body = MfaCodeBody,
    responses(
        (status = 200, description = "Nouveaux codes de secours", body = RecoveryCodes),
        (status = 401, description = "Access token absent, invalide, révoqué, ou compte suspendu"),
        (status = 403, description = "Code incorrect (`invalid mfa code`)"),
        (status = 409, description = "MFA inactive (`mfa not enabled`)"),
        (status = 429, description = "Trop d'essais — voir `Retry-After`")
    ),
    security(("bearer" = []))
)]
pub async fn mfa_recovery_codes(
    user: AuthenticatedUser,
    State(state): State<AccountsState>,
    Json(body): Json<MfaCodeBody>,
) -> Result<Json<RecoveryCodes>, AccountError> {
    const ACTION: &str = "auth.mfa_recovery_codes";
    let user_id = user.user_id().to_string();
    let now = state.auth.clock.now_unix_seconds();
    let key = mfa_throttle_key(&user_id);
    reserve(&state, &key, &user_id, ACTION, now).await?;
    let account = current_account(&state, &user_id).await?;
    let Some(record) = state.mfa.active(&account.id).await.map_err(internal)? else {
        return Err(AccountError::Conflict(MFA_NOT_ENABLED));
    };
    if state
        .mfa
        .verify_code(&account.id, &record, &body.code, now)
        .await
        .map_err(internal)?
        .is_none()
    {
        audit(&state, &account.id, ACTION, AuditOutcome::Failure).await;
        return Err(AccountError::Forbidden(INVALID_MFA_CODE));
    }
    let codes = generate_recovery_codes();
    if !state
        .mfa
        .store
        .replace_recovery_codes(&account.id, &state.mfa.recovery_hashes(&codes))
        .await
        .map_err(internal)?
    {
        return Err(AccountError::Conflict(MFA_NOT_ENABLED));
    }
    reset_counter(&state, &key).await;
    audit(&state, &account.id, ACTION, AuditOutcome::Success).await;
    Ok(Json(RecoveryCodes {
        recovery_codes: codes,
    }))
}

/// `POST /auth/mfa/verify` — seconde étape de la connexion : échange le défi
/// de `/auth/login` et un code (TOTP ou de secours) contre une paire.
#[utoipa::path(
    post,
    path = "/auth/mfa/verify",
    tag = "auth",
    request_body = MfaVerifyBody,
    responses(
        (status = 200, description = "Connecté", body = TokenResponse),
        (status = 400, description = "Défi invalide ou expiré (`invalid or expired mfa challenge`) : recommencer la connexion"),
        (status = 401, description = "Code incorrect, déjà utilisé ou rejoué"),
        (status = 403, description = "Compte suspendu (`account disabled`) ou adresse non vérifiée (`email not verified`)"),
        (status = 429, description = "Trop d'essais (compte ou adresse IP) — voir `Retry-After`")
    )
)]
pub async fn mfa_verify(
    State(state): State<AccountsState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<MfaVerifyBody>,
) -> Result<Json<TokenResponse>, AccountError> {
    const ACTION: &str = "auth.mfa_verify";
    let invalid = || AccountError::BadRequest(INVALID_MFA_CHALLENGE.into());
    let token = LinkTokens::parse(&body.mfa_token).ok_or_else(invalid)?;
    let user_id = token.user_id.to_string();
    let now = state.auth.clock.now_unix_seconds();
    // Quota de connexion de l'IP d'abord : des défis forgés en rafale l'usent.
    let ip = ip_counter(&state, LOGIN_IP_SCOPE, peer.map(|c| c.0), &headers);
    check_ip(&state, &ip, ACTION, &user_id, now).await?;
    // Version lue avant le compte et son drapeau de suspension (cf.
    // `accounts`) : un défi émis avant une fermeture des sessions est périmé.
    let sv = state
        .auth
        .revocation
        .session_version(&user_id)
        .await
        .map_err(internal)?;
    let Some(account) = state
        .users
        .find_by_id(&user_id)
        .await
        .map_err(internal)?
        .filter(|a| {
            state
                .mfa
                .verify_challenge(&token, sv, &a.password_hash, now)
        })
    else {
        audit(&state, &user_id, ACTION, AuditOutcome::Failure).await;
        return Err(invalid());
    };
    // Défi authentique : seulement maintenant, un essai compte sur le compte.
    let key = mfa_throttle_key(&account.id);
    reserve(&state, &key, &account.id, ACTION, now).await?;
    if account.disabled {
        audit(&state, &account.id, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::Forbidden(ACCOUNT_DISABLED));
    }
    if state.email_verification.is_some() && !account.email_verified {
        audit(&state, &account.id, ACTION, AuditOutcome::Denied).await;
        return Err(AccountError::Forbidden(EMAIL_NOT_VERIFIED));
    }
    // MFA réinitialisée depuis le défi : recommencer la connexion.
    let Some(record) = state.mfa.active(&account.id).await.map_err(internal)? else {
        return Err(invalid());
    };
    let Some(method) = state
        .mfa
        .verify_code(&account.id, &record, &body.code, now)
        .await
        .map_err(internal)?
    else {
        audit(&state, &account.id, ACTION, AuditOutcome::Failure).await;
        return Err(AccountError::Unauthorized);
    };
    reset_counter(&state, &key).await;
    if let Some((ip_key, _)) = &ip {
        if let Err(e) = state.throttle.refund(ip_key).await {
            tracing::warn!(error = %e, "remboursement du compteur IP impossible");
        }
    }
    if method == MfaMethod::RecoveryCode {
        tracing::info!(
            user_id = %account.id,
            left = record.recovery_codes_left.saturating_sub(1),
            "connexion par code de secours"
        );
    }
    record_event(
        &state,
        AuditEvent::new(
            now_ms(&state),
            &account.id,
            "auth.login",
            AuditOutcome::Success,
        )
        .with_metadata(serde_json::json!({ "mfa": method.as_str() })),
    )
    .await;
    Ok(Json(issue_pair_at(&state, &account.id, sv)?))
}

/// Sous-router `/mfa*` — à imbriquer sous `/auth` avec
/// [`crate::accounts::accounts_router`].
pub fn mfa_router(state: AccountsState) -> Router {
    Router::new()
        .route("/mfa", get(mfa_status))
        .route("/mfa/setup", post(mfa_setup))
        .route("/mfa/enable", post(mfa_enable))
        .route("/mfa/disable", post(mfa_disable))
        .route("/mfa/recovery-codes", post(mfa_recovery_codes))
        .route("/mfa/verify", post(mfa_verify))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use autome_core::{
        AuditOutcome, AuditSink, InMemoryAuditSink, InMemoryPermissionStore,
        InMemoryRevocationStore, InMemoryUserStore, PasswordHasher, TokenService, UserStore,
        LOGIN_WINDOW_SECONDS,
    };
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::response::Response;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::accounts::accounts_router;
    use crate::auth::{protected_router, AuthState, Clock};

    // ───── primitives ─────

    /// Secret des vecteurs de test des RFC 4226 et 6238.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn hotp_matches_rfc_4226_vectors() {
        let expected = [
            755_224, 287_082, 359_152, 969_429, 338_314, 254_676, 287_922, 162_583, 399_871,
            520_489,
        ];
        for (counter, code) in expected.iter().enumerate() {
            assert_eq!(
                hotp(RFC_SECRET, counter as u64, 6),
                *code,
                "compteur {counter}"
            );
        }
    }

    #[test]
    fn totp_matches_rfc_6238_sha1_vectors() {
        for (time, code) in [
            (59_i64, 94_287_082),
            (1_111_111_109, 7_081_804),
            (1_111_111_111, 14_050_471),
            (1_234_567_890, 89_005_924),
            (2_000_000_000, 69_279_037),
            (20_000_000_000, 65_353_130),
        ] {
            let step = u64::try_from(totp_step(time)).unwrap();
            assert_eq!(hotp(RFC_SECRET, step, 8), code, "T = {time}");
            // Profil 6 chiffres : les 6 derniers.
            assert_eq!(hotp(RFC_SECRET, step, 6), code % 1_000_000);
        }
    }

    fn code_at(secret: &[u8], now: i64) -> String {
        format!(
            "{:06}",
            hotp(secret, u64::try_from(totp_step(now)).unwrap(), TOTP_DIGITS)
        )
    }

    #[test]
    fn totp_tolerates_one_step_of_clock_drift_only() {
        let now = 1_700_000_000;
        let step = totp_step(now);
        for (offset, accepted) in [
            (-60, false),
            (-30, true),
            (0, true),
            (30, true),
            (60, false),
        ] {
            let code = code_at(RFC_SECRET, now + offset);
            let got = verify_totp(RFC_SECRET, &code, now);
            assert_eq!(got.is_some(), accepted, "décalage {offset} s");
            if accepted {
                assert_eq!(got, Some(step + offset / TOTP_PERIOD));
            }
        }
        // Espaces tolérés ; autres formes refusées.
        let code = code_at(RFC_SECRET, now);
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert_eq!(verify_totp(RFC_SECRET, &spaced, now), Some(step));
        for bad in ["", "12345", "1234567", "abcdef", "12345a", "-12345"] {
            assert_eq!(verify_totp(RFC_SECRET, bad, now), None, "{bad:?}");
        }
    }

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(
            base32(&[0u8; SECRET_LEN]).len(),
            32,
            "160 bits = 32 symboles"
        );
    }

    #[test]
    fn otpauth_uri_and_qr_code() {
        let uri = otpauth_uri("Autome staging", "jo+mfa@example.io", "JBSWY3DPEHPK3PXP");
        assert_eq!(
            uri,
            "otpauth://totp/Autome%20staging:jo%2Bmfa%40example.io?secret=JBSWY3DPEHPK3PXP\
             &issuer=Autome%20staging&algorithm=SHA1&digits=6&period=30"
        );
        let svg = qr_svg(&uri).unwrap();
        assert!(svg.contains("<svg"), "SVG attendu");
        assert!(!svg.contains("<script"));
    }

    #[test]
    fn recovery_codes_are_distinct_and_forgiving_to_type() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), RECOVERY_CODE_COUNT);
        for code in &codes {
            assert_eq!(code.len(), 11);
            assert_eq!(&code[5..6], "-");
            assert_eq!(
                normalize_recovery_code(code).as_deref(),
                Some(code.replace('-', "").as_str())
            );
        }
        assert_eq!(
            normalize_recovery_code(" AB0C1-DEF9Z ").as_deref(),
            Some("ab0c1def9z")
        );
        // `o`, `i`, `l` lus comme `0`, `1`, `1`.
        assert_eq!(
            normalize_recovery_code("abOcI-defLz").as_deref(),
            Some("ab0c1def1z")
        );
        for bad in ["", "abcde", "abcde-fghjk-m", "abcde-fghju", "123456"] {
            assert_eq!(normalize_recovery_code(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn keys_are_separated_and_secrets_sealed() {
        let a = Mfa::new(Arc::new(InMemoryMfaStore::new()), b"key-a", DEFAULT_ISSUER);
        let b = Mfa::new(Arc::new(InMemoryMfaStore::new()), b"key-b", DEFAULT_ISSUER);
        let sealed = a.seal(RFC_SECRET).unwrap();
        assert!(!sealed.contains(&base32(RFC_SECRET)));
        assert_eq!(a.open(&sealed).unwrap(), RFC_SECRET);
        assert!(b.open(&sealed).is_err(), "autre ENCRYPTION_KEY");
        assert_ne!(a.recovery_hash("ab0c1def1z"), b.recovery_hash("ab0c1def1z"));
        assert_eq!(a.recovery_hash("ab0c1def1z").len(), 64);
        assert!(format!("{a:?}").contains("Autome") && !format!("{a:?}").contains("key-a"));
    }

    // ───── parcours HTTP ─────

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const START: i64 = 1_700_000_000;
    const PASSWORD: &str = "kiwi-mango-papaya";

    struct MovableClock(Mutex<i64>);
    impl Clock for MovableClock {
        fn now_unix_seconds(&self) -> i64 {
            *self.0.lock().unwrap()
        }
    }

    /// Hasher factice (rapide) : `plain:<mdp>`.
    struct PlainHasher;
    impl PasswordHasher for PlainHasher {
        fn hash(&self, plain: &str) -> autome_core::Result<String> {
            Ok(format!("plain:{plain}"))
        }
        fn verify(&self, plain: &str, phc: &str) -> autome_core::Result<bool> {
            Ok(phc.strip_prefix("plain:") == Some(plain))
        }
    }

    struct Harness {
        app: Router,
        clock: Arc<MovableClock>,
        users: Arc<InMemoryUserStore>,
        audit: Arc<InMemoryAuditSink>,
        store: Arc<InMemoryMfaStore>,
    }

    impl Harness {
        fn new() -> Self {
            let clock = Arc::new(MovableClock(Mutex::new(START)));
            let perms = Arc::new(InMemoryPermissionStore::new());
            let users = Arc::new(InMemoryUserStore::new(perms.clone()));
            let audit = Arc::new(InMemoryAuditSink::new());
            let auth = AuthState::new(
                TokenService::new(SECRET),
                Arc::new(InMemoryRevocationStore::new()),
                perms,
                audit.clone(),
            )
            .with_clock(clock.clone());
            let store = Arc::new(InMemoryMfaStore::new());
            let state = AccountsState::new(auth.clone(), users.clone(), Arc::new(PlainHasher))
                .unwrap()
                .with_mfa(Mfa::new(store.clone(), b"test-encryption-key", "Autome"));
            Self {
                app: protected_router(auth).merge(accounts_router(state)),
                clock,
                users,
                audit,
                store,
            }
        }

        fn advance(&self, secs: i64) {
            *self.clock.0.lock().unwrap() += secs;
        }

        fn now(&self) -> i64 {
            self.clock.now_unix_seconds()
        }

        async fn call(
            &self,
            method: &str,
            path: &str,
            bearer: Option<&str>,
            body: Value,
        ) -> Response {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if let Some(token) = bearer {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            self.app
                .clone()
                .oneshot(req.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap()
        }

        async fn signup(&self, email: &str) -> TokenResponse {
            let r = self
                .call(
                    "POST",
                    "/signup",
                    None,
                    json!({ "email": email, "password": PASSWORD }),
                )
                .await;
            assert_eq!(r.status(), StatusCode::CREATED);
            parse(r).await
        }

        async fn login(&self, email: &str, password: &str) -> Response {
            self.call(
                "POST",
                "/login",
                None,
                json!({ "email": email, "password": password }),
            )
            .await
        }

        /// Enrôle et active la MFA ; renvoie `(secret, codes de secours,
        /// nouvelle paire)`.
        async fn enroll(&self, access: &str) -> (Vec<u8>, Vec<String>, TokenResponse) {
            let r = self
                .call(
                    "POST",
                    "/mfa/setup",
                    Some(access),
                    json!({ "current_password": PASSWORD }),
                )
                .await;
            assert_eq!(r.status(), StatusCode::OK);
            let setup: MfaSetup = parse(r).await;
            let secret = decode_base32(&setup.secret);
            let r = self
                .call(
                    "POST",
                    "/mfa/enable",
                    Some(access),
                    json!({ "code": code_at(&secret, self.now()) }),
                )
                .await;
            assert_eq!(r.status(), StatusCode::OK);
            let enabled: MfaEnabled = parse(r).await;
            // Pas suivant : le code d'activation est consommé.
            self.advance(TOTP_PERIOD);
            (secret, enabled.recovery_codes, enabled.session)
        }

        async fn challenge(&self, email: &str) -> String {
            let r = self.login(email, PASSWORD).await;
            assert_eq!(r.status(), StatusCode::ACCEPTED);
            let challenge: MfaChallenge = parse(r).await;
            assert!(challenge.mfa_required);
            assert_eq!(challenge.expires_in, MFA_CHALLENGE_TTL_SECONDS);
            challenge.mfa_token
        }

        async fn verify(&self, mfa_token: &str, code: &str) -> Response {
            self.call(
                "POST",
                "/mfa/verify",
                None,
                json!({ "mfa_token": mfa_token, "code": code }),
            )
            .await
        }

        async fn whoami(&self, access: &str) -> StatusCode {
            self.call("GET", "/whoami", Some(access), Value::Null)
                .await
                .status()
        }
    }

    async fn parse<T: serde::de::DeserializeOwned>(r: Response) -> T {
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text(r: Response) -> String {
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn decode_base32(s: &str) -> Vec<u8> {
        const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let (mut buffer, mut bits, mut out) = (0u32, 0u8, Vec::new());
        for c in s.chars() {
            buffer = (buffer << 5) | ALPHABET.find(c).unwrap() as u32;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        out
    }

    #[tokio::test]
    async fn enrollment_then_login_requires_a_fresh_code() {
        let h = Harness::new();
        let first = h.signup("ana@example.io").await;
        let other_device = parse::<TokenResponse>(h.login("ana@example.io", PASSWORD).await).await;

        // État avant : rien.
        let status: MfaStatus = parse(
            h.call("GET", "/mfa", Some(&first.access_token), Value::Null)
                .await,
        )
        .await;
        assert!(!status.enabled && !status.pending);

        let (secret, codes, session) = h.enroll(&first.access_token).await;
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        // L'activation ferme toutes les autres sessions…
        for old in [&first, &other_device] {
            assert_eq!(h.whoami(&old.access_token).await, StatusCode::UNAUTHORIZED);
        }
        // … et l'appareil courant continue avec la paire neuve.
        assert_eq!(h.whoami(&session.access_token).await, StatusCode::OK);
        let status: MfaStatus = parse(
            h.call("GET", "/mfa", Some(&session.access_token), Value::Null)
                .await,
        )
        .await;
        assert!(status.enabled && !status.pending);
        assert_eq!(status.recovery_codes_left, RECOVERY_CODE_COUNT);
        let me: Value = parse(
            h.call("GET", "/me", Some(&session.access_token), Value::Null)
                .await,
        )
        .await;
        assert_eq!(me["mfa_enabled"], true);

        // Le secret est scellé au repos.
        let user = h
            .users
            .find_by_email("ana@example.io")
            .await
            .unwrap()
            .unwrap();
        let stored = h.store.get(&user.id).await.unwrap().unwrap();
        assert!(!stored.secret.contains(&base32(&secret)));

        // Le mot de passe seul ne donne plus de session.
        let token = h.challenge("ana@example.io").await;
        let code = code_at(&secret, h.now());
        let r = h.verify(&token, &code).await;
        assert_eq!(r.status(), StatusCode::OK);
        let pair: TokenResponse = parse(r).await;
        assert_eq!(h.whoami(&pair.access_token).await, StatusCode::OK);

        // Le même code, rejoué avec un nouveau défi : refusé.
        let token = h.challenge("ana@example.io").await;
        let r = h.verify(&token, &code).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // Le pas suivant passe (le défi reste valable jusqu'à son expiration).
        h.advance(TOTP_PERIOD);
        let r = h.verify(&token, &code_at(&secret, h.now())).await;
        assert_eq!(r.status(), StatusCode::OK);

        // Audit : connexion avec sa méthode, jamais le code.
        let events = h.audit.list().await.unwrap();
        let login = events
            .iter()
            .rev()
            .find(|e| e.action == "auth.login" && e.outcome == AuditOutcome::Success)
            .unwrap();
        assert_eq!(login.metadata["mfa"], "totp");
        assert!(events
            .iter()
            .any(|e| e.action == "auth.mfa_challenge" && e.outcome == AuditOutcome::Success));
        let dump = format!("{events:?}");
        assert!(!dump.contains(&code) && !dump.contains(&base32(&secret)));
    }

    #[tokio::test]
    async fn recovery_codes_are_single_use_and_replaceable() {
        let h = Harness::new();
        let first = h.signup("bea@example.io").await;
        let (secret, codes, session) = h.enroll(&first.access_token).await;

        let token = h.challenge("bea@example.io").await;
        // Saisie tolérante : majuscules, sans tiret.
        let typed = codes[0].replace('-', "").to_uppercase();
        let r = h.verify(&token, &typed).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = h.verify(&token, &codes[0]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "déjà utilisé");
        let events = h.audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "auth.login" && e.metadata["mfa"] == "recovery_code"));

        // Régénération : un code exigé, les anciens ne valent plus rien.
        let r = h
            .call(
                "POST",
                "/mfa/recovery-codes",
                Some(&session.access_token),
                json!({ "code": "000000" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, INVALID_MFA_CODE);
        let r = h
            .call(
                "POST",
                "/mfa/recovery-codes",
                Some(&session.access_token),
                json!({ "code": code_at(&secret, h.now()) }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK);
        let fresh: RecoveryCodes = parse(r).await;
        assert_eq!(fresh.recovery_codes.len(), RECOVERY_CODE_COUNT);
        let token = h.challenge("bea@example.io").await;
        assert_eq!(
            h.verify(&token, &codes[1]).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            h.verify(&token, &fresh.recovery_codes[0]).await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn code_guessing_is_limited_and_forged_challenges_cannot_lock_the_account() {
        let h = Harness::new();
        let first = h.signup("cam@example.io").await;
        let (secret, _, _) = h.enroll(&first.access_token).await;
        let user_id = h
            .users
            .find_by_email("cam@example.io")
            .await
            .unwrap()
            .unwrap()
            .id;

        // Défis forgés au nom du compte : 400, et aucun essai compté.
        let forged = format!("{user_id}.{}.{}", START + 300, "ab".repeat(32));
        for _ in 0..2 * MAX_LOGIN_FAILURES {
            let r = h.verify(&forged, "123456").await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            assert_eq!(text(r).await, INVALID_MFA_CHALLENGE);
        }
        let token = h.challenge("cam@example.io").await;
        let good = code_at(&secret, h.now());
        let wrong = if good == "000000" { "111111" } else { "000000" };
        for _ in 0..MAX_LOGIN_FAILURES {
            assert_eq!(
                h.verify(&token, wrong).await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let r = h.verify(&token, &good).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
        // La fenêtre passée, un nouveau défi et un bon code passent.
        h.advance(LOGIN_WINDOW_SECONDS);
        let token = h.challenge("cam@example.io").await;
        assert_eq!(
            h.verify(&token, &code_at(&secret, h.now())).await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn challenges_expire_and_die_with_the_password_or_the_sessions() {
        let h = Harness::new();
        let first = h.signup("dan@example.io").await;
        let (secret, _, _) = h.enroll(&first.access_token).await;

        let token = h.challenge("dan@example.io").await;
        h.advance(MFA_CHALLENGE_TTL_SECONDS);
        let r = h.verify(&token, &code_at(&secret, h.now())).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "expiré");

        // Fermeture de toutes les sessions : le défi en cours est périmé.
        let token = h.challenge("dan@example.io").await;
        let r = h.verify(&token, &code_at(&secret, h.now())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let session: TokenResponse = parse(r).await;
        h.advance(TOTP_PERIOD);
        let token = h.challenge("dan@example.io").await;
        let r = h
            .call(
                "POST",
                "/logout-all",
                Some(&session.access_token),
                Value::Null,
            )
            .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            h.verify(&token, &code_at(&secret, h.now())).await.status(),
            StatusCode::BAD_REQUEST
        );

        // Changement de mot de passe (en base) : idem.
        let token = h.challenge("dan@example.io").await;
        let user = h
            .users
            .find_by_email("dan@example.io")
            .await
            .unwrap()
            .unwrap();
        h.users
            .update_password(&user.id, &user.password_hash, "plain:another-long-secret")
            .await
            .unwrap();
        assert_eq!(
            h.verify(&token, &code_at(&secret, h.now())).await.status(),
            StatusCode::BAD_REQUEST
        );
        // Défis mal formés.
        for bad in ["", "x.y.z", "not-a-token"] {
            assert_eq!(
                h.verify(bad, "123456").await.status(),
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[tokio::test]
    async fn a_suspended_account_gets_no_session_from_its_challenge() {
        let h = Harness::new();
        let first = h.signup("eli@example.io").await;
        let (secret, _, _) = h.enroll(&first.access_token).await;
        let token = h.challenge("eli@example.io").await;
        let user = h
            .users
            .find_by_email("eli@example.io")
            .await
            .unwrap()
            .unwrap();
        h.users.set_disabled(&user.id, true).await.unwrap();
        let r = h.verify(&token, &code_at(&secret, h.now())).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, ACCOUNT_DISABLED);
    }

    #[tokio::test]
    async fn setup_and_enable_guard_their_preconditions() {
        let h = Harness::new();
        let pair = h.signup("fay@example.io").await;
        let access = pair.access_token.as_str();

        // Activation sans enrôlement.
        let r = h
            .call(
                "POST",
                "/mfa/enable",
                Some(access),
                json!({ "code": "123456" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(text(r).await, MFA_SETUP_REQUIRED);
        // Mauvais mot de passe.
        let r = h
            .call(
                "POST",
                "/mfa/setup",
                Some(access),
                json!({ "current_password": "nope-nope-nope" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // Sans jeton.
        let r = h
            .call(
                "POST",
                "/mfa/setup",
                None,
                json!({ "current_password": PASSWORD }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // Enrôlement en attente : la connexion n'exige encore rien.
        let r = h
            .call(
                "POST",
                "/mfa/setup",
                Some(access),
                json!({ "current_password": PASSWORD }),
            )
            .await;
        let setup: MfaSetup = parse(r).await;
        assert!(setup
            .otpauth_uri
            .starts_with("otpauth://totp/Autome:fay%40example.io?"));
        assert!(setup.otpauth_uri.contains(&setup.secret));
        assert!(setup.qr_svg.contains("<svg"));
        let status: MfaStatus = parse(h.call("GET", "/mfa", Some(access), Value::Null).await).await;
        assert!(status.pending && !status.enabled);
        assert_eq!(
            h.login("fay@example.io", PASSWORD).await.status(),
            StatusCode::OK
        );
        // Mauvais code : 403, rien n'est activé.
        let secret = decode_base32(&setup.secret);
        let good = code_at(&secret, h.now());
        let wrong = if good == "000000" { "111111" } else { "000000" };
        let r = h
            .call(
                "POST",
                "/mfa/enable",
                Some(access),
                json!({ "code": wrong }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, INVALID_MFA_CODE);
        // Un nouvel enrôlement remplace le secret : l'ancien code ne vaut rien.
        let r = h
            .call(
                "POST",
                "/mfa/setup",
                Some(access),
                json!({ "current_password": PASSWORD }),
            )
            .await;
        let again: MfaSetup = parse(r).await;
        assert_ne!(again.secret, setup.secret);
        let r = h
            .call("POST", "/mfa/enable", Some(access), json!({ "code": good }))
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = h
            .call(
                "POST",
                "/mfa/enable",
                Some(access),
                json!({ "code": code_at(&decode_base32(&again.secret), h.now()) }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK);
        let session: MfaEnabled = parse(r).await;
        // Active : ni nouvel enrôlement ni seconde activation.
        let fresh = session.session.access_token.as_str();
        let r = h
            .call(
                "POST",
                "/mfa/setup",
                Some(fresh),
                json!({ "current_password": PASSWORD }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(text(r).await, MFA_ALREADY_ENABLED);
    }

    #[tokio::test]
    async fn disabling_requires_the_password_and_a_code() {
        let h = Harness::new();
        let first = h.signup("gus@example.io").await;
        let (secret, codes, session) = h.enroll(&first.access_token).await;
        let access = session.access_token.as_str();

        let r = h
            .call(
                "POST",
                "/mfa/disable",
                Some(access),
                json!({ "current_password": "wrong-password", "code": code_at(&secret, h.now()) }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = h
            .call(
                "POST",
                "/mfa/disable",
                Some(access),
                json!({ "current_password": PASSWORD, "code": "abcde-fghjk" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(text(r).await, INVALID_MFA_CODE);
        // Un code de secours suffit (téléphone perdu, session encore ouverte).
        let r = h
            .call(
                "POST",
                "/mfa/disable",
                Some(access),
                json!({ "current_password": PASSWORD, "code": codes[3] }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            h.login("gus@example.io", PASSWORD).await.status(),
            StatusCode::OK
        );
        let r = h
            .call(
                "POST",
                "/mfa/disable",
                Some(access),
                json!({ "current_password": PASSWORD, "code": "123456" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(text(r).await, MFA_NOT_ENABLED);
    }
}
