//! Dispatch des actions de connecteurs (#159).
//!
//! Traduit le `action_type` d'un node (`"slack.send_message"`,
//! `"gmail.send_email"`, `"notion.create_page"`, `"github.create_issue"`, …)
//! vers l'action de connecteur correspondante (`crate`
//! `autome-connectors`), **injecte le token** du provider résolu par un
//! [`CredentialResolver`], puis exécute l'action.
//!
//! Le **provider** est le préfixe du `action_type` (`slack.*` → `slack`). Le
//! token devient l'auth bearer attendue par chaque connecteur
//! ([`SlackAuth::Bot`], [`GmailAuth::OAuth2`], [`NotionAuth::Integration`],
//! [`GithubAuth::Token`]).
//!
//! ## Résolution des credentials — par propriétaire du run
//!
//! Chaque run porte le `claims.sub` du propriétaire de son workflow
//! (`RunCheckpoint.owner_id`, backlog BOLA). [`OwnerCredentialResolver`] lit le
//! token OAuth **de ce propriétaire** pour le provider dans le
//! [`ConnectorTokenStore`] chiffré (table `connector_tokens` en `postgres`,
//! alimentée par `/credentials/{provider}/callback`). Le `provider_id` OAuth
//! doit donc être le préfixe du connecteur (`slack`, `gmail`, `notion`,
//! `github`). Run sans propriétaire ou token absent : **échec explicite**.
//!
//! Un access token **expiré** (ou qui expire dans moins de
//! [`REFRESH_SKEW_MS`]) est d'abord rafraîchi via le refresh token stocké
//! ([`TokenRefresher`] : client OAuth configuré par `AUTOME_OAUTH_<ID>_*`), et
//! le nouveau `TokenSet` est re-stocké chiffré. Impossible à rafraîchir (pas de
//! refresh token, provider non configuré sur le worker, refus du provider) :
//! échec explicite invitant à reconnecter le compte.
//!
//! Le token **global** par provider ([`EnvCredentialResolver`],
//! `AUTOME_CONNECTOR_TOKEN_<PROVIDER>`) n'est consulté qu'en repli, et
//! seulement si l'opérateur choisit explicitement le mode mono-tenant
//! `AUTOME_CONNECTOR_CREDENTIALS=single_tenant` ([`ConnectorCredentialsMode`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use autome_connectors::oauth::{
    merge_refreshed, needs_refresh, ConnectorTokenStore, TokenRefresher, TokenSet, REFRESH_SKEW_MS,
};
use autome_connectors::{
    GithubAuth, GithubCloseIssue, GithubCommentIssue, GithubCreateIssue, GithubCreatePull,
    GithubListPulls, GithubMergePull, GmailAuth, GmailGetAttachment, GmailListMessages,
    GmailReadMessage, GmailSendEmail, HttpClient, NotionAuth, NotionCreatePage,
    NotionQueryDatabase, NotionReadPage, SlackAuth, SlackReadChannel, SlackSendMessage,
};
use autome_core::{Action, Error, Result as CoreResult};
use serde_json::Value;

/// Résout le token d'un provider de connecteur (`"slack"`, `"gmail"`, …) pour
/// le propriétaire d'un run.
///
/// `Send + Sync` : le résolveur est partagé (`Arc`) et utilisé dans le
/// `spawn_blocking` du worker.
pub trait CredentialResolver: Send + Sync {
    /// Token du `provider` pour `owner` (`None` : run sans propriétaire).
    /// `Err` porte un message explicite (token absent, backend en erreur…) qui
    /// devient l'erreur du node.
    fn token(&self, owner: Option<&str>, provider: &str) -> Result<String, String>;
}

/// Résolveur **mono-tenant** par variables d'environnement :
/// `AUTOME_CONNECTOR_TOKEN_<PROVIDER>` (provider en majuscules). Ignore le
/// propriétaire : **le même token pour tous les runs**. Utilisé uniquement en
/// repli du mode [`ConnectorCredentialsMode::SingleTenant`].
///
/// Ex. `AUTOME_CONNECTOR_TOKEN_SLACK=xoxb-…`,
/// `AUTOME_CONNECTOR_TOKEN_GITHUB=ghp_…`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvCredentialResolver;

impl CredentialResolver for EnvCredentialResolver {
    fn token(&self, _owner: Option<&str>, provider: &str) -> Result<String, String> {
        let var = format!("AUTOME_CONNECTOR_TOKEN_{}", provider.to_ascii_uppercase());
        std::env::var(&var)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("credential `{provider}` absente (définir {var})"))
    }
}

/// Résolveur **par propriétaire** : token OAuth de `owner` pour `provider`
/// dans le [`ConnectorTokenStore`] chiffré.
///
/// Sans repli (défaut), un run sans propriétaire ou un propriétaire qui n'a pas
/// connecté le provider échoue. Avec [`with_fallback`](Self::with_fallback),
/// ces deux cas — et eux seuls — consultent le résolveur de repli ; une
/// **erreur de lecture** du store ou un token **expiré non rafraîchissable**
/// n'y basculent jamais (le propriétaire a un compte connecté : le run ne doit
/// pas agir avec le compte global à sa place).
///
/// Rafraîchissement : avec [`with_refresher`](Self::with_refresher), un token
/// expiré est rafraîchi puis re-stocké. Les rafraîchissements d'un même
/// `(propriétaire, provider)` sont **sérialisés dans le process** et le store
/// est relu sous le verrou : N runs concurrents ⇒ un seul appel au provider
/// (indispensable quand il fait tourner le refresh token).
pub struct OwnerCredentialResolver {
    store: Arc<dyn ConnectorTokenStore + Send + Sync>,
    fallback: Option<Arc<dyn CredentialResolver>>,
    refresher: Option<Arc<dyn TokenRefresher + Send + Sync>>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    refresh_locks: Mutex<RefreshLocks>,
}

/// Verrou de rafraîchissement par `(propriétaire, provider)`.
type RefreshLocks = HashMap<(String, String), Arc<Mutex<()>>>;

/// Horodatage Unix (ms) courant — horloge par défaut.
fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl OwnerCredentialResolver {
    /// Résolveur strict sur `store`, sans rafraîchissement.
    pub fn new(store: Arc<dyn ConnectorTokenStore + Send + Sync>) -> Self {
        Self {
            store,
            fallback: None,
            refresher: None,
            clock: Arc::new(system_now_ms),
            refresh_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Ajoute un résolveur de repli (mode mono-tenant).
    pub fn with_fallback(mut self, fallback: Arc<dyn CredentialResolver>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Active le rafraîchissement des tokens expirés.
    pub fn with_refresher(mut self, refresher: Arc<dyn TokenRefresher + Send + Sync>) -> Self {
        self.refresher = Some(refresher);
        self
    }

    /// Remplace l'horloge (Unix ms) — tests déterministes.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Verrou dédié à `(owner, provider)`, créé à la demande.
    fn refresh_lock(&self, owner: &str, provider: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .refresh_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            locks
                .entry((owner.to_string(), provider.to_string()))
                .or_default(),
        )
    }

    /// Token valide de `owner` pour `provider`, rafraîchi si nécessaire.
    /// `Ok(None)` : aucun token stocké (le repli éventuel s'applique).
    fn fresh_token(&self, owner: &str, provider: &str) -> Result<Option<String>, String> {
        let read_err =
            |e| format!("lecture de la credential `{provider}` de `{owner}` impossible : {e}");
        let usable = |t: TokenSet| Some(t).filter(|t| !t.access_token.is_empty());

        let Some(tokens) = self
            .store
            .load(owner, provider)
            .map_err(read_err)?
            .and_then(usable)
        else {
            return Ok(None);
        };
        if !needs_refresh(&tokens, (self.clock)(), REFRESH_SKEW_MS) {
            return Ok(Some(tokens.access_token));
        }

        // Expiré : un seul rafraîchissement à la fois ; on relit sous le
        // verrou, un run concurrent a pu rafraîchir entre-temps.
        let lock = self.refresh_lock(owner, provider);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(tokens) = self
            .store
            .load(owner, provider)
            .map_err(read_err)?
            .and_then(usable)
        else {
            return Ok(None);
        };
        let now = (self.clock)();
        if !needs_refresh(&tokens, now, REFRESH_SKEW_MS) {
            return Ok(Some(tokens.access_token));
        }

        let reconnect = format!("reconnecter via POST /credentials/{provider}/authorize");
        let Some(refresh_token) = tokens.refresh_token.as_deref() else {
            return Err(format!(
                "credential `{provider}` de `{owner}` expirée sans refresh token ({reconnect})"
            ));
        };
        let Some(refresher) = &self.refresher else {
            return Err(format!(
                "credential `{provider}` de `{owner}` expirée : rafraîchissement non \
                 configuré sur le worker ({reconnect})"
            ));
        };
        let refreshed = refresher
            .refresh(provider, refresh_token, now)
            .map_err(|e| {
                format!(
                    "rafraîchissement de la credential `{provider}` de `{owner}` échoué : \
                     {e} ({reconnect})"
                )
            })?;
        let merged = merge_refreshed(&tokens, refreshed);
        if let Err(e) = self.store.store(owner, provider, &merged) {
            // Le token neuf reste utilisable pour ce run ; le prochain
            // retentera le rafraîchissement depuis l'ancien refresh token.
            tracing::warn!(
                provider,
                owner,
                error = %e,
                "token de connecteur rafraîchi mais non persisté"
            );
        } else {
            tracing::info!(provider, owner, "token de connecteur rafraîchi");
        }
        Ok(Some(merged.access_token))
    }

    fn fallback_or(
        &self,
        owner: Option<&str>,
        provider: &str,
        err: String,
    ) -> Result<String, String> {
        match &self.fallback {
            Some(fallback) => fallback.token(owner, provider),
            None => Err(err),
        }
    }
}

impl CredentialResolver for OwnerCredentialResolver {
    fn token(&self, owner: Option<&str>, provider: &str) -> Result<String, String> {
        let Some(owner) = owner.filter(|o| !o.is_empty()) else {
            return self.fallback_or(
                None,
                provider,
                format!(
                    "credential `{provider}` non résolue : run sans propriétaire \
                     (antérieur à l'isolation par propriétaire)"
                ),
            );
        };
        match self.fresh_token(owner, provider)? {
            Some(token) => Ok(token),
            None => self.fallback_or(
                Some(owner),
                provider,
                format!(
                    "credential `{provider}` absente pour le propriétaire `{owner}` du run \
                     (la connecter via POST /credentials/{provider}/authorize)"
                ),
            ),
        }
    }
}

/// Stratégie de résolution des tokens de connecteurs
/// (`AUTOME_CONNECTOR_CREDENTIALS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorCredentialsMode {
    /// Défaut : token OAuth du propriétaire du run, sinon échec.
    PerUser,
    /// Opt-in opérateur **mono-tenant** : token du propriétaire, sinon token
    /// global `AUTOME_CONNECTOR_TOKEN_<PROVIDER>` partagé par tous les runs.
    SingleTenant,
}

impl ConnectorCredentialsMode {
    /// Lit le mode depuis `AUTOME_CONNECTOR_CREDENTIALS` (défaut : `PerUser`).
    pub fn from_env() -> CoreResult<Self> {
        Self::parse(
            std::env::var("AUTOME_CONNECTOR_CREDENTIALS")
                .ok()
                .as_deref(),
        )
    }

    /// Parse une valeur optionnelle (`None`/`""` ⇒ `PerUser`). Toute autre
    /// valeur est rejetée : pas de bascule silencieuse vers le token global.
    pub fn parse(value: Option<&str>) -> CoreResult<Self> {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" | "per_user" | "per-user" => Ok(Self::PerUser),
            "single_tenant" | "single-tenant" => Ok(Self::SingleTenant),
            other => Err(Error::Invalid(format!(
                "AUTOME_CONNECTOR_CREDENTIALS invalide : `{other}` \
                 (attendu : per_user | single_tenant)"
            ))),
        }
    }

    /// Construit le résolveur de ce mode sur le store de tokens, avec
    /// rafraîchissement des tokens expirés via `refresher`.
    pub fn resolver(
        self,
        store: Arc<dyn ConnectorTokenStore + Send + Sync>,
        refresher: Arc<dyn TokenRefresher + Send + Sync>,
    ) -> Arc<dyn CredentialResolver> {
        let per_user = OwnerCredentialResolver::new(store).with_refresher(refresher);
        Arc::new(match self {
            Self::PerUser => per_user,
            Self::SingleTenant => per_user.with_fallback(Arc::new(EnvCredentialResolver)),
        })
    }
}

/// `true` si le `action_type` est dispatché par ce module (connecteur connu).
/// `http_request` reste géré directement par le runner (pas de credentials).
pub fn is_connector_action(action_type: &str) -> bool {
    matches!(
        action_type.split('.').next().unwrap_or(""),
        "slack" | "gmail" | "notion" | "github"
    )
}

/// Exécute l'action de connecteur `action_type` avec `params`, en injectant le
/// token de `owner` (propriétaire du run) résolu par `resolver`. `http`
/// (clonable) est le transport partagé.
///
/// Erreurs (toutes en `String`, l'appelant décide du verdict) :
/// - provider/`action_type` inconnu → message explicite ;
/// - token non résolu → message du résolveur (aucune requête n'est envoyée) ;
/// - erreur d'exécution du connecteur → message du connecteur.
pub fn dispatch_action<C, R>(
    action_type: &str,
    params: &Value,
    http: &C,
    resolver: &R,
    owner: Option<&str>,
) -> Result<Value, String>
where
    C: HttpClient + Clone,
    R: CredentialResolver + ?Sized,
{
    let provider = action_type.split('.').next().unwrap_or("");
    if !is_connector_action(action_type) {
        return Err(unsupported(action_type));
    }
    // Token requis par tous les connecteurs supportés.
    let token = resolver.token(owner, provider)?;
    let run = |r: autome_core::Result<Value>| r.map_err(|e| e.to_string());

    match action_type {
        // ───── Slack ─────
        "slack.send_message" => {
            run(SlackSendMessage::new(SlackAuth::Bot(token), http.clone()).execute(params))
        }
        "slack.read_channel" => {
            run(SlackReadChannel::new(SlackAuth::Bot(token), http.clone()).execute(params))
        }
        // ───── Gmail ─────
        "gmail.send_email" => {
            run(GmailSendEmail::new(GmailAuth::OAuth2(token), http.clone()).execute(params))
        }
        "gmail.list_messages" => {
            run(GmailListMessages::new(GmailAuth::OAuth2(token), http.clone()).execute(params))
        }
        "gmail.read_message" => {
            run(GmailReadMessage::new(GmailAuth::OAuth2(token), http.clone()).execute(params))
        }
        "gmail.get_attachment" => {
            run(GmailGetAttachment::new(GmailAuth::OAuth2(token), http.clone()).execute(params))
        }
        // ───── Notion ─────
        "notion.create_page" => {
            run(NotionCreatePage::new(NotionAuth::Integration(token), http.clone()).execute(params))
        }
        "notion.read_page" => {
            run(NotionReadPage::new(NotionAuth::Integration(token), http.clone()).execute(params))
        }
        "notion.query_database" => run(NotionQueryDatabase::new(
            NotionAuth::Integration(token),
            http.clone(),
        )
        .execute(params)),
        // ───── GitHub ─────
        "github.create_issue" => {
            run(GithubCreateIssue::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        "github.comment_issue" => {
            run(GithubCommentIssue::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        "github.close_issue" => {
            run(GithubCloseIssue::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        "github.list_pulls" => {
            run(GithubListPulls::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        "github.create_pull" => {
            run(GithubCreatePull::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        "github.merge_pull" => {
            run(GithubMergePull::new(GithubAuth::Token(token), http.clone()).execute(params))
        }
        other => Err(unsupported(other)),
    }
}

fn unsupported(action_type: &str) -> String {
    format!(
        "action `{action_type}` non supportée par le worker (connecteurs : slack.*, gmail.*, notion.*, github.*, http_request)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_connectors::{HttpRequest, HttpResponse};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Mock `HttpClient` **clonable** (état partagé via `Arc<Mutex>`) : le clone
    /// consommé par l'action partage l'enregistrement des requêtes envoyées.
    #[derive(Clone, Default)]
    struct SharedMock {
        sent: Arc<Mutex<Vec<HttpRequest>>>,
        status: u16,
        body: Value,
    }

    impl SharedMock {
        fn ok(body: Value) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                status: 200,
                body,
            }
        }
        fn last(&self) -> HttpRequest {
            self.sent
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("une requête")
        }
    }

    impl HttpClient for SharedMock {
        fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.sent.lock().unwrap().push(req.clone());
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// Résolveur de test : token fixe pour tout provider (ou aucun).
    struct MockResolver(Option<String>);
    impl CredentialResolver for MockResolver {
        fn token(&self, _owner: Option<&str>, provider: &str) -> Result<String, String> {
            self.0
                .clone()
                .ok_or_else(|| format!("mock : pas de token `{provider}`"))
        }
    }

    fn resolver(tok: &str) -> MockResolver {
        MockResolver(Some(tok.into()))
    }

    #[test]
    fn slack_send_message_injects_bearer_and_maps_response() {
        let http = SharedMock::ok(json!({ "ok": true, "ts": "1.2" }));
        let out = dispatch_action(
            "slack.send_message",
            &json!({ "channel": "C1", "text": "hi" }),
            &http,
            &resolver("xoxb-secret"),
            Some("alice"),
        )
        .expect("dispatch ok");
        // Auth injectée dans la requête réellement envoyée.
        assert_eq!(
            http.last().headers.get("Authorization").map(String::as_str),
            Some("Bearer xoxb-secret")
        );
        // Réponse du connecteur remontée.
        assert_eq!(out["ok"], json!(true));
    }

    #[test]
    fn each_provider_injects_its_token() {
        for (action, body) in [
            ("gmail.send_email", json!({ "id": "m1" })),
            ("notion.create_page", json!({ "id": "p1" })),
            ("github.create_issue", json!({ "number": 1 })),
        ] {
            let http = SharedMock::ok(body);
            let params = match action {
                "gmail.send_email" => json!({ "to": "a@b.c", "subject": "s", "body": "b" }),
                "notion.create_page" => {
                    json!({ "parent": { "database_id": "db1" }, "properties": { "Name": "t" } })
                }
                _ => json!({ "owner": "o", "repo": "r", "title": "t" }),
            };
            let res = dispatch_action(action, &params, &http, &resolver("tok-123"), Some("alice"));
            assert!(res.is_ok(), "{action} devrait réussir : {res:?}");
            assert_eq!(
                http.last().headers.get("Authorization").map(String::as_str),
                Some("Bearer tok-123"),
                "{action} doit injecter le bearer"
            );
        }
    }

    #[test]
    fn unresolved_token_fails_before_any_request() {
        let http = SharedMock::ok(json!({}));
        let err = dispatch_action(
            "slack.send_message",
            &json!({ "channel": "C1", "text": "x" }),
            &http,
            &MockResolver(None),
            Some("alice"),
        )
        .unwrap_err();
        assert!(err.contains("pas de token `slack`"), "msg: {err}");
        assert!(
            http.sent.lock().unwrap().is_empty(),
            "aucune requête sans token"
        );
    }

    #[test]
    fn unknown_action_is_an_error() {
        let http = SharedMock::ok(json!({}));
        let err =
            dispatch_action("dropbox.upload", &json!({}), &http, &resolver("t"), None).unwrap_err();
        assert!(err.contains("non supportée"), "msg: {err}");
    }

    #[test]
    fn is_connector_action_recognizes_known_providers() {
        assert!(is_connector_action("slack.send_message"));
        assert!(is_connector_action("github.merge_pull"));
        assert!(!is_connector_action("http_request"));
        assert!(!is_connector_action("dropbox.upload"));
    }

    #[test]
    fn env_resolver_reads_provider_var() {
        // Variable dédiée et improbable pour éviter les collisions de test.
        let var = "AUTOME_CONNECTOR_TOKEN_SLACK";
        let prev = std::env::var(var).ok();
        std::env::set_var(var, "xoxb-from-env");
        let got = EnvCredentialResolver
            .token(Some("anyone"), "slack")
            .unwrap();
        assert_eq!(got, "xoxb-from-env");
        let err = EnvCredentialResolver
            .token(None, "notconfigured")
            .unwrap_err();
        assert!(
            err.contains("AUTOME_CONNECTOR_TOKEN_NOTCONFIGURED"),
            "{err}"
        );
        match prev {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    // ───── Résolution par propriétaire ─────

    use autome_connectors::oauth::{InMemoryConnectorTokenStore, TokenSet};
    use autome_core::{EnvelopeCipher, InMemoryKeyProvider};

    fn tokens(access: &str) -> TokenSet {
        TokenSet {
            access_token: access.into(),
            refresh_token: None,
            expires_at_ms: None,
            token_type: "Bearer".into(),
            scope: None,
        }
    }

    /// Store chiffré avec le token Slack d'alice uniquement.
    fn store_with_alice_slack() -> Arc<dyn ConnectorTokenStore + Send + Sync> {
        let store = InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
            InMemoryKeyProvider::with_key("v1", [2u8; 32]),
        ));
        store
            .store("alice", "slack", &tokens("xoxb-alice"))
            .unwrap();
        Arc::new(store)
    }

    /// Store qui échoue toujours (panne de base simulée).
    struct BrokenStore;
    impl ConnectorTokenStore for BrokenStore {
        fn store(&self, _: &str, _: &str, _: &TokenSet) -> CoreResult<()> {
            Err(Error::Invalid("storage: down".into()))
        }
        fn load(&self, _: &str, _: &str) -> CoreResult<Option<TokenSet>> {
            Err(Error::Invalid("storage: down".into()))
        }
        fn delete(&self, _: &str, _: &str) -> CoreResult<()> {
            Err(Error::Invalid("storage: down".into()))
        }
        fn list_by_user(&self, _: &str) -> CoreResult<Vec<String>> {
            Err(Error::Invalid("storage: down".into()))
        }
    }

    #[test]
    fn owner_resolver_returns_the_owner_token_only() {
        let r = OwnerCredentialResolver::new(store_with_alice_slack());
        assert_eq!(r.token(Some("alice"), "slack").unwrap(), "xoxb-alice");

        let bob = r.token(Some("bob"), "slack").unwrap_err();
        assert!(
            bob.contains("propriétaire `bob`") && bob.contains("/credentials/slack/authorize"),
            "{bob}"
        );
        let other_provider = r.token(Some("alice"), "github").unwrap_err();
        assert!(other_provider.contains("`github`"), "{other_provider}");
    }

    #[test]
    fn owner_resolver_rejects_runs_without_owner() {
        let r = OwnerCredentialResolver::new(store_with_alice_slack());
        for owner in [None, Some("")] {
            let err = r.token(owner, "slack").unwrap_err();
            assert!(err.contains("sans propriétaire"), "{err}");
        }
    }

    #[test]
    fn owner_resolver_falls_back_only_when_opted_in_and_token_absent() {
        let r = OwnerCredentialResolver::new(store_with_alice_slack())
            .with_fallback(Arc::new(resolver("global-token")));
        // Le token du propriétaire prime sur le global.
        assert_eq!(r.token(Some("alice"), "slack").unwrap(), "xoxb-alice");
        // Repli : propriétaire sans token, ou run sans propriétaire.
        assert_eq!(r.token(Some("bob"), "slack").unwrap(), "global-token");
        assert_eq!(r.token(None, "slack").unwrap(), "global-token");
    }

    #[test]
    fn store_error_never_falls_back_to_the_global_token() {
        let r = OwnerCredentialResolver::new(Arc::new(BrokenStore))
            .with_fallback(Arc::new(resolver("global-token")));
        let err = r.token(Some("alice"), "slack").unwrap_err();
        assert!(err.contains("impossible") && err.contains("down"), "{err}");
    }

    #[test]
    fn credentials_mode_defaults_to_per_user_and_rejects_unknown_values() {
        assert_eq!(
            ConnectorCredentialsMode::parse(None).unwrap(),
            ConnectorCredentialsMode::PerUser
        );
        assert_eq!(
            ConnectorCredentialsMode::parse(Some(" per_user ")).unwrap(),
            ConnectorCredentialsMode::PerUser
        );
        assert_eq!(
            ConnectorCredentialsMode::parse(Some("SINGLE_TENANT")).unwrap(),
            ConnectorCredentialsMode::SingleTenant
        );
        let err = ConnectorCredentialsMode::parse(Some("env")).unwrap_err();
        assert!(
            err.to_string().contains("AUTOME_CONNECTOR_CREDENTIALS"),
            "{err}"
        );
    }

    #[test]
    fn per_user_mode_never_reads_the_global_env_token() {
        // Même avec un token global défini, le mode par défaut l'ignore.
        let var = "AUTOME_CONNECTOR_TOKEN_NOTION";
        let prev = std::env::var(var).ok();
        std::env::set_var(var, "secret_global");
        let r = ConnectorCredentialsMode::PerUser.resolver(
            store_with_alice_slack(),
            Arc::new(MockRefresher::ok("unused")),
        );
        assert!(r.token(Some("alice"), "notion").is_err());
        let single = ConnectorCredentialsMode::SingleTenant.resolver(
            store_with_alice_slack(),
            Arc::new(MockRefresher::ok("unused")),
        );
        assert_eq!(
            single.token(Some("alice"), "notion").unwrap(),
            "secret_global"
        );
        match prev {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    // ───── Rafraîchissement des tokens expirés ─────

    const NOW: i64 = 1_700_000_000_000;

    /// Refresher de test : compte les appels, renvoie un résultat fixe après
    /// un délai optionnel (pour les courses).
    struct MockRefresher {
        calls: Mutex<Vec<(String, String)>>,
        result: Result<TokenSet, String>,
        delay_ms: u64,
    }

    impl MockRefresher {
        fn ok(access: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Ok(TokenSet {
                    access_token: access.into(),
                    refresh_token: None,
                    expires_at_ms: Some(NOW + 3_600_000),
                    token_type: "Bearer".into(),
                    scope: None,
                }),
                delay_ms: 0,
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                result: Err(msg.into()),
                ..Self::ok("unused")
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TokenRefresher for MockRefresher {
        fn refresh(&self, provider: &str, refresh_token: &str, _now: i64) -> CoreResult<TokenSet> {
            self.calls
                .lock()
                .unwrap()
                .push((provider.into(), refresh_token.into()));
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            self.result.clone().map_err(Error::Invalid)
        }
    }

    /// Store chiffré avec le token Gmail d'alice, expirant à `expires_at_ms`.
    fn store_with_alice_gmail(
        expires_at_ms: Option<i64>,
        refresh_token: Option<&str>,
    ) -> Arc<dyn ConnectorTokenStore + Send + Sync> {
        let store = InMemoryConnectorTokenStore::new(EnvelopeCipher::new(
            InMemoryKeyProvider::with_key("v1", [6u8; 32]),
        ));
        let tokens = TokenSet {
            access_token: "ya29-old".into(),
            refresh_token: refresh_token.map(str::to_string),
            expires_at_ms,
            token_type: "Bearer".into(),
            scope: Some("gmail.send".into()),
        };
        store.store("alice", "gmail", &tokens).unwrap();
        Arc::new(store)
    }

    fn refreshing(
        store: &Arc<dyn ConnectorTokenStore + Send + Sync>,
        refresher: &Arc<MockRefresher>,
    ) -> OwnerCredentialResolver {
        OwnerCredentialResolver::new(Arc::clone(store))
            .with_refresher(Arc::clone(refresher) as Arc<dyn TokenRefresher + Send + Sync>)
            .with_clock(Arc::new(|| NOW))
    }

    #[test]
    fn valid_token_is_used_without_refreshing() {
        let store = store_with_alice_gmail(Some(NOW + 3_600_000), Some("rt-alice"));
        let refresher = Arc::new(MockRefresher::ok("ya29-new"));
        let r = refreshing(&store, &refresher);
        assert_eq!(r.token(Some("alice"), "gmail").unwrap(), "ya29-old");
        assert!(refresher.calls().is_empty());
    }

    #[test]
    fn expired_token_is_refreshed_and_persisted() {
        let store = store_with_alice_gmail(Some(NOW - 1), Some("rt-alice"));
        let refresher = Arc::new(MockRefresher::ok("ya29-new"));
        let r = refreshing(&store, &refresher);

        assert_eq!(r.token(Some("alice"), "gmail").unwrap(), "ya29-new");
        assert_eq!(
            refresher.calls(),
            vec![("gmail".to_string(), "rt-alice".to_string())]
        );
        // Re-stocké : nouvel access token, refresh token et scope conservés.
        let stored = store.load("alice", "gmail").unwrap().unwrap();
        assert_eq!(stored.access_token, "ya29-new");
        assert_eq!(stored.expires_at_ms, Some(NOW + 3_600_000));
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-alice"));
        assert_eq!(stored.scope.as_deref(), Some("gmail.send"));
        // Appel suivant : token neuf, aucun nouveau rafraîchissement.
        assert_eq!(r.token(Some("alice"), "gmail").unwrap(), "ya29-new");
        assert_eq!(refresher.calls().len(), 1);
    }

    #[test]
    fn token_expiring_within_the_skew_is_refreshed() {
        let store = store_with_alice_gmail(Some(NOW + REFRESH_SKEW_MS / 2), Some("rt-alice"));
        let refresher = Arc::new(MockRefresher::ok("ya29-new"));
        assert_eq!(
            refreshing(&store, &refresher)
                .token(Some("alice"), "gmail")
                .unwrap(),
            "ya29-new"
        );
    }

    #[test]
    fn expired_token_without_refresh_token_asks_to_reconnect() {
        let store = store_with_alice_gmail(Some(NOW - 1), None);
        let refresher = Arc::new(MockRefresher::ok("ya29-new"));
        let err = refreshing(&store, &refresher)
            .token(Some("alice"), "gmail")
            .unwrap_err();
        assert!(
            err.contains("expirée sans refresh token")
                && err.contains("/credentials/gmail/authorize"),
            "{err}"
        );
        assert!(refresher.calls().is_empty());
    }

    #[test]
    fn expired_token_without_refresher_fails_explicitly() {
        let store = store_with_alice_gmail(Some(NOW - 1), Some("rt-alice"));
        let err = OwnerCredentialResolver::new(store)
            .with_clock(Arc::new(|| NOW))
            .token(Some("alice"), "gmail")
            .unwrap_err();
        assert!(err.contains("non configuré"), "{err}");
    }

    #[test]
    fn failed_refresh_never_falls_back_and_keeps_the_stored_token() {
        let store = store_with_alice_gmail(Some(NOW - 1), Some("rt-alice"));
        let refresher = Arc::new(MockRefresher::failing(
            "oauth refresh : http 400 — invalid_grant",
        ));
        let err = refreshing(&store, &refresher)
            .with_fallback(Arc::new(resolver("global-token")))
            .token(Some("alice"), "gmail")
            .unwrap_err();
        assert!(
            err.contains("invalid_grant") && err.contains("/credentials/gmail/authorize"),
            "{err}"
        );
        let stored = store.load("alice", "gmail").unwrap().unwrap();
        assert_eq!(stored.access_token, "ya29-old");
    }

    #[test]
    fn concurrent_runs_refresh_only_once() {
        let store = store_with_alice_gmail(Some(NOW - 1), Some("rt-alice"));
        let refresher = Arc::new(MockRefresher {
            delay_ms: 50,
            ..MockRefresher::ok("ya29-new")
        });
        let r = refreshing(&store, &refresher);
        let barrier = std::sync::Barrier::new(8);
        let tokens: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        r.token(Some("alice"), "gmail").unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(tokens.iter().all(|t| t == "ya29-new"), "{tokens:?}");
        assert_eq!(refresher.calls().len(), 1, "un seul appel au provider");
    }
}
