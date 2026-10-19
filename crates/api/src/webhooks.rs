//! Déclencheurs webhook (#202) : ingress HTTP public et gestion des URL.
//!
//! | Méthode | Path                                          | Auth / permission      | Description                         |
//! |---------|-----------------------------------------------|------------------------|-------------------------------------|
//! | *       | `/hooks/{token}`                              | aucune (URL secrète)   | Déclenche un run (202 + `run_id`)   |
//! | GET     | `/workflows/{id}/webhooks`                    | Bearer `runs.execute`  | URL + secret de chaque nœud webhook |
//! | POST    | `/workflows/{id}/webhooks/{node_id}/rotate`   | Bearer `runs.execute`  | Nouvelle URL (l'ancienne meurt)     |
//!
//! ## URL secrète
//!
//! Chaque nœud `trigger_type: "webhook"` reçoit un token de 256 bits (64
//! caractères hexadécimaux), HMAC d'une clé dérivée d'`ENCRYPTION_KEY` et d'un
//! sel aléatoire propre à l'endpoint. La base ne garde que le sel et le
//! SHA-256 du token ([`autome_core::WebhookEndpointStore`]) : sans la clé, elle
//! ne livre aucune URL. Connaître l'URL suffit à déclencher le workflow ;
//! d'où la permission `runs.execute` pour la lire, et la rotation.
//!
//! ## Réception
//!
//! Token inconnu ou mal formé, workflow supprimé, nœud retiré ou qui n'est
//! plus un webhook : même **404**. Puis, dans l'ordre :
//!
//! 1. quota par endpoint ([`DEFAULT_MAX_PER_WINDOW`] requêtes par fenêtre de
//!    15 min, compteur partagé entre répliques) ⇒ 429 ;
//! 2. méthode différente du param `method` du nœud (défaut `POST`) ⇒ 405 ;
//! 3. param `signature: "github"` : `X-Hub-Signature-256` vérifié avec le
//!    secret de signature de l'endpoint ⇒ 401 sinon (toute autre valeur que
//!    `none` est refusée : pas de signature ignorée par erreur) ;
//! 4. param `event` : un `X-GitHub-Event` différent (ex. `ping`) est acquitté
//!    **204** sans run ;
//! 5. run dispatché dans l'espace du propriétaire (`trigger_type: "webhook"`)
//!    ⇒ **202** `{ run_id, status }`.
//!
//! Le nœud trigger renvoie au workflow le payload `{ node_id, method, query,
//! headers, body }` (corps JSON parsé, sinon texte UTF-8, `null` si vide ;
//! ≤ [`MAX_BODY_BYTES`]). Les en-têtes d'authentification, cookies,
//! signatures et en-têtes d'adresse IP en sont retirés : rien de cela n'est
//! transmis au run ni stocké.
//!
//! Le param `path` du nœud (éditeur) n'intervient pas dans l'URL : c'est un
//! libellé.

use std::collections::BTreeMap;
use std::sync::Arc;

use autome_connectors::verify_github_signature;
use autome_core::{
    AuditEvent, AuditOutcome, Config, DispatchRequest, EndpointSecret, LoginAttempt,
    LoginThrottleStore, Node, NodeKind, RunDispatcher, WebhookEndpointStore, WorkflowGraph,
    WorkflowId, WorkflowRepository, PERM_RUNS_EXECUTE,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, FromRef, Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use crate::auth::{AuthError, AuthState, AuthenticatedUser};
use crate::workflows::NamespaceQuery;

/// Taille maximale du corps d'une requête entrante (1 Mio).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Requêtes acceptées par endpoint et par fenêtre de 15 min, par défaut
/// (`AUTOME_WEBHOOK_MAX_PER_WINDOW`).
pub const DEFAULT_MAX_PER_WINDOW: u32 = 1_000;
/// Préfixe des URL de webhook.
pub const HOOKS_PREFIX: &str = "/hooks/";

/// En-têtes jamais transmis au run : authentification, cookies, signatures,
/// adresse IP du client (aucune IP n'est stockée).
const DROPPED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-hub-signature",
    "x-hub-signature-256",
    "forwarded",
    "x-forwarded-for",
    "x-real-ip",
    "true-client-ip",
    "cf-connecting-ip",
    "x-client-ip",
];

/// Clé serveur des webhooks : dérive tokens et secrets de signature.
#[derive(Clone)]
pub struct WebhookKeys {
    key: [u8; 32],
}

impl std::fmt::Debug for WebhookKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookKeys(<redacted>)")
    }
}

impl WebhookKeys {
    /// Clé dérivée de `secret` (séparation de domaine : elle diffère de toute
    /// autre clé tirée du même secret).
    pub fn new(secret: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"autome/webhook/v1\0");
        hasher.update(secret);
        Self {
            key: hasher.finalize().into(),
        }
    }

    /// Clé dérivée d'`ENCRYPTION_KEY` : identique sur toutes les répliques.
    /// Changer `ENCRYPTION_KEY` change toutes les URL de webhook.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.encryption_key.expose().as_bytes())
    }

    fn derive(&self, label: &[u8], salt: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepte toute longueur");
        mac.update(label);
        mac.update(salt);
        hex(&mac.finalize().into_bytes())
    }

    /// Token de l'URL `/hooks/{token}` de l'endpoint de sel `salt`.
    pub fn token(&self, salt: &[u8]) -> String {
        self.derive(b"token\0", salt)
    }

    /// Secret de signature (`X-Hub-Signature-256`) de l'endpoint.
    pub fn signing_secret(&self, salt: &[u8]) -> String {
        self.derive(b"signing\0", salt)
    }

    /// Nouveau secret d'endpoint : sel aléatoire et hash du token dérivé.
    pub fn fresh(&self) -> EndpointSecret {
        let mut salt = vec![0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        let token_hash = token_hash(&self.token(&salt));
        EndpointSecret { salt, token_hash }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Clé de résolution d'un token : son SHA-256.
pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Un token est exactement 64 caractères hexadécimaux minuscules.
fn is_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Remplace le token d'un chemin `/hooks/…` (journaux : l'URL est un secret).
pub fn redact_path(path: &str) -> &str {
    if path.starts_with(HOOKS_PREFIX) {
        "/hooks/{token}"
    } else {
        path
    }
}

/// Le nœud `node_id` de `workflow` s'il s'agit d'un trigger webhook.
fn webhook_node<'a>(workflow: &'a WorkflowGraph, node_id: &str) -> Option<&'a Node> {
    workflow
        .nodes
        .iter()
        .find(|n| n.id.0 == node_id && is_webhook(n))
}

fn is_webhook(node: &Node) -> bool {
    matches!(&node.kind, NodeKind::Trigger { trigger_type } if trigger_type == "webhook")
}

/// Identifiants des nœuds webhook de `workflow`.
pub fn webhook_node_ids(workflow: &WorkflowGraph) -> Vec<String> {
    workflow
        .nodes
        .iter()
        .filter(|n| is_webhook(n))
        .map(|n| n.id.0.clone())
        .collect()
}

fn param<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Méthode HTTP attendue par le nœud (param `method`, défaut `POST`).
fn expected_method(node: &Node) -> String {
    param(node, "method").unwrap_or("POST").to_ascii_uppercase()
}

/// État des handlers webhook.
#[derive(Clone)]
pub struct WebhooksState {
    /// Authentification des routes de gestion, audit, horloge.
    pub auth: AuthState,
    /// Workflows (lecture du nœud visé).
    pub workflows: Arc<dyn WorkflowRepository + Send + Sync>,
    /// Endpoints (sel + hash du token).
    pub endpoints: Arc<dyn WebhookEndpointStore>,
    /// Dispatcher de runs.
    pub dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
    /// Compteurs du quota par endpoint (partagés en PostgreSQL).
    pub throttle: Arc<dyn LoginThrottleStore>,
    /// Clé de dérivation des tokens.
    pub keys: WebhookKeys,
    /// Requêtes acceptées par endpoint et par fenêtre.
    pub max_per_window: u32,
}

impl WebhooksState {
    /// État avec le quota par défaut ([`DEFAULT_MAX_PER_WINDOW`]).
    pub fn new(
        auth: AuthState,
        workflows: Arc<dyn WorkflowRepository + Send + Sync>,
        endpoints: Arc<dyn WebhookEndpointStore>,
        dispatcher: Arc<dyn RunDispatcher + Send + Sync>,
        throttle: Arc<dyn LoginThrottleStore>,
        keys: WebhookKeys,
    ) -> Self {
        Self {
            auth,
            workflows,
            endpoints,
            dispatcher,
            throttle,
            keys,
            max_per_window: DEFAULT_MAX_PER_WINDOW,
        }
    }

    /// Builder : quota par endpoint et par fenêtre.
    pub fn with_max_per_window(mut self, max: u32) -> Self {
        self.max_per_window = max;
        self
    }

    fn now(&self) -> i64 {
        self.auth.clock.now_unix_seconds()
    }

    async fn audit(&self, actor: &str, action: &str, resource: &str, outcome: AuditOutcome) {
        let now_ms = self.now().saturating_mul(1_000);
        let event = AuditEvent::new(now_ms, actor, action, outcome).with_resource(resource);
        if let Err(e) = self.auth.audit.record(event).await {
            tracing::error!(action, error = %e, "événement d'audit non enregistré");
        }
    }
}

impl FromRef<WebhooksState> for AuthState {
    fn from_ref(input: &WebhooksState) -> Self {
        input.auth.clone()
    }
}

/// Quota par endpoint depuis l'environnement
/// (`AUTOME_WEBHOOK_MAX_PER_WINDOW`, défaut [`DEFAULT_MAX_PER_WINDOW`]).
pub fn max_per_window_from_env() -> Result<u32, String> {
    max_per_window_from_source(|name| std::env::var(name).ok())
}

/// Comme [`max_per_window_from_env`], variable lue via `get` (tests).
pub fn max_per_window_from_source(get: impl Fn(&str) -> Option<String>) -> Result<u32, String> {
    const NAME: &str = "AUTOME_WEBHOOK_MAX_PER_WINDOW";
    match get(NAME) {
        None => Ok(DEFAULT_MAX_PER_WINDOW),
        Some(v) if v.trim().is_empty() => Ok(DEFAULT_MAX_PER_WINDOW),
        Some(v) => v
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{NAME} : entier ≥ 1 attendu, obtenu `{v}`")),
    }
}

/// Erreurs des handlers webhook.
#[derive(Debug)]
pub enum WebhookError {
    /// Authentification / RBAC (routes de gestion).
    Auth(AuthError),
    /// URL, workflow ou nœud inconnu.
    NotFound,
    /// Méthode différente de celle du nœud.
    MethodNotAllowed(String),
    /// Signature absente ou invalide.
    BadSignature,
    /// Quota de l'endpoint épuisé.
    TooManyRequests(i64),
    /// Corps illisible (JSON invalide, texte non UTF-8).
    BadRequest(String),
    /// Erreur backend.
    Internal(String),
}

impl From<AuthError> for WebhookError {
    fn from(e: AuthError) -> Self {
        Self::Auth(e)
    }
}

fn internal(e: impl std::fmt::Display) -> WebhookError {
    WebhookError::Internal(e.to_string())
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> Response {
        match self {
            Self::Auth(e) => e.into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "webhook inconnu").into_response(),
            Self::MethodNotAllowed(allowed) => (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, allowed)],
                "méthode non acceptée par ce webhook",
            )
                .into_response(),
            Self::BadSignature => {
                (StatusCode::UNAUTHORIZED, "signature absente ou invalide").into_response()
            }
            Self::TooManyRequests(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                "quota du webhook atteint",
            )
                .into_response(),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            Self::Internal(msg) => {
                tracing::error!(error = %msg, "webhook : erreur interne");
                (StatusCode::INTERNAL_SERVER_ERROR, "erreur interne").into_response()
            }
        }
    }
}

/// Réponse d'un webhook accepté.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookAccepted {
    /// Identifiant du run créé.
    pub run_id: String,
    /// État initial (`"queued"`).
    pub status: String,
}

/// Corps de la requête entrante, tel que transmis au run.
fn parse_body(headers: &HeaderMap, body: &Bytes) -> Result<Value, WebhookError> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let mime = ct
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        })
        .unwrap_or(false);
    if is_json {
        serde_json::from_slice(body)
            .map_err(|e| WebhookError::BadRequest(format!("corps JSON invalide : {e}")))
    } else {
        std::str::from_utf8(body)
            .map(|text| Value::String(text.to_string()))
            .map_err(|_| WebhookError::BadRequest("corps non UTF-8".into()))
    }
}

/// En-têtes transmis au run (noms en minuscules, valeurs UTF-8 seulement).
fn forwarded_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        let name = name.as_str();
        if DROPPED_HEADERS.contains(&name) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            out.entry(name.to_string())
                .and_modify(|v: &mut String| {
                    v.push_str(", ");
                    v.push_str(value);
                })
                .or_insert_with(|| value.to_string());
        }
    }
    out
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `* /hooks/{token}` — déclenche le workflow du nœud webhook visé.
#[utoipa::path(
    post,
    path = "/hooks/{token}",
    tag = "webhooks",
    params(("token" = String, Path, description = "Token secret de l'URL (64 caractères hexadécimaux)")),
    request_body(content = serde_json::Value, description = "Corps libre (JSON parsé, sinon texte), ≤ 1 Mio"),
    responses(
        (status = 202, description = "Run accepté en file", body = WebhookAccepted),
        (status = 204, description = "Événement filtré par le param `event` du nœud : aucun run"),
        (status = 400, description = "Corps JSON invalide ou non UTF-8"),
        (status = 401, description = "Signature absente ou invalide (nœud signé)"),
        (status = 404, description = "URL inconnue, workflow ou nœud supprimé"),
        (status = 405, description = "Méthode différente du param `method` du nœud"),
        (status = 413, description = "Corps supérieur à 1 Mio"),
        (status = 429, description = "Quota de l'endpoint atteint (`Retry-After`)")
    )
)]
pub async fn receive(
    State(state): State<WebhooksState>,
    Path(token): Path<String>,
    method: Method,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, WebhookError> {
    if !is_token(&token) {
        return Err(WebhookError::NotFound);
    }
    let hash = token_hash(&token);
    let target = state
        .endpoints
        .resolve(&hash)
        .await
        .map_err(internal)?
        .ok_or(WebhookError::NotFound)?;

    // Quota avant tout travail : lecture du workflow, vérification HMAC.
    let throttle_key = format!("webhook:{}", hex(&hash));
    if let LoginAttempt::Blocked { retry_after_secs } = state
        .throttle
        .begin_attempt(&throttle_key, state.max_per_window, state.now())
        .await
        .map_err(internal)?
    {
        return Err(WebhookError::TooManyRequests(retry_after_secs));
    }

    let workflow = state
        .workflows
        .get(&target.owner_id, &WorkflowId::new(&target.workflow_id))
        .await
        .map_err(internal)?
        .ok_or(WebhookError::NotFound)?;
    let node = webhook_node(&workflow, &target.node_id).ok_or(WebhookError::NotFound)?;

    let expected = expected_method(node);
    if method.as_str() != expected {
        return Err(WebhookError::MethodNotAllowed(expected));
    }

    match param(node, "signature") {
        None | Some("none") => {}
        Some("github") => {
            let secret = state.keys.signing_secret(&target.salt);
            let valid = header_str(&headers, "x-hub-signature-256")
                .is_some_and(|sig| verify_github_signature(secret.as_bytes(), &body, sig));
            if !valid {
                state
                    .audit(
                        "webhook",
                        "webhooks.receive",
                        &target.workflow_id,
                        AuditOutcome::Denied,
                    )
                    .await;
                return Err(WebhookError::BadSignature);
            }
        }
        Some(other) => {
            tracing::warn!(
                workflow_id = %target.workflow_id,
                node_id = %target.node_id,
                signature = other,
                "webhook : type de signature non pris en charge, requête refusée"
            );
            return Err(WebhookError::BadSignature);
        }
    }

    if let Some(event) = param(node, "event") {
        if header_str(&headers, "x-github-event") != Some(event) {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
    }

    let payload = json!({
        "node_id": target.node_id,
        "method": method.as_str(),
        "query": query,
        "headers": forwarded_headers(&headers),
        "body": parse_body(&headers, &body)?,
    });
    let outcome = state
        .dispatcher
        .dispatch(DispatchRequest {
            workflow_id: target.workflow_id.clone(),
            version: 1,
            trigger_type: "webhook".into(),
            payload,
            now_ms: state.now().saturating_mul(1_000),
            owner_id: Some(target.owner_id.clone()),
        })
        .await
        .map_err(internal)?;
    state
        .audit(
            "webhook",
            "webhooks.receive",
            &outcome.run_id,
            AuditOutcome::Success,
        )
        .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookAccepted {
            run_id: outcome.run_id,
            status: "queued".into(),
        }),
    )
        .into_response())
}

/// URL d'un nœud webhook.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookInfo {
    /// Nœud trigger webhook.
    pub node_id: String,
    /// Libellé du nœud.
    pub name: String,
    /// Méthode acceptée.
    pub method: String,
    /// Chemin secret à appeler (`/hooks/{token}`), relatif à l'URL de l'API.
    pub path: String,
    /// Vérification de signature configurée (`github`), absente sinon.
    pub signature: Option<String>,
    /// Secret de signature de l'endpoint (à saisir chez l'émetteur, ex.
    /// « Secret » d'un webhook GitHub).
    pub signing_secret: String,
}

/// Réponse de [`list_webhooks`].
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookList {
    /// Une entrée par nœud webhook du workflow.
    pub items: Vec<WebhookInfo>,
}

fn info(keys: &WebhookKeys, node: &Node, salt: &[u8]) -> WebhookInfo {
    WebhookInfo {
        node_id: node.id.0.clone(),
        name: node.name.clone(),
        method: expected_method(node),
        path: format!("{HOOKS_PREFIX}{}", keys.token(salt)),
        signature: param(node, "signature")
            .filter(|s| *s != "none")
            .map(str::to_string),
        signing_secret: keys.signing_secret(salt),
    }
}

async fn load_workflow(
    state: &WebhooksState,
    ns: &str,
    id: &str,
) -> Result<WorkflowGraph, WebhookError> {
    state
        .workflows
        .get(ns, &WorkflowId::new(id))
        .await
        .map_err(internal)?
        .ok_or(WebhookError::NotFound)
}

/// `GET /workflows/{id}/webhooks` — URL de chaque nœud webhook (créées à la
/// première lecture).
#[utoipa::path(
    get,
    path = "/workflows/{id}/webhooks",
    tag = "webhooks",
    params(("id" = String, Path, description = "Identifiant du workflow"), NamespaceQuery),
    responses(
        (status = 200, description = "URL et secrets des nœuds webhook", body = WebhookList),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `runs.execute`, ou `owner` d'autrui sans `*`"),
        (status = 404, description = "Workflow inconnu dans cet espace")
    ),
    security(("bearer" = []))
)]
pub async fn list_webhooks(
    user: AuthenticatedUser,
    State(state): State<WebhooksState>,
    Path(id): Path<String>,
    Query(q): Query<NamespaceQuery>,
) -> Result<Json<WebhookList>, WebhookError> {
    state
        .auth
        .require_permission(&user, PERM_RUNS_EXECUTE)
        .await?;
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let workflow = load_workflow(&state, &ns, &id).await?;
    let mut items = Vec::new();
    for node in workflow.nodes.iter().filter(|n| is_webhook(n)) {
        let salt = state
            .endpoints
            .ensure(&ns, &id, &node.id.0, state.keys.fresh())
            .await
            .map_err(internal)?;
        items.push(info(&state.keys, node, &salt));
    }
    state
        .audit(user.user_id(), "webhooks.list", &id, AuditOutcome::Success)
        .await;
    Ok(Json(WebhookList { items }))
}

/// `POST /workflows/{id}/webhooks/{node_id}/rotate` — nouvelle URL et nouveau
/// secret de signature ; les anciens cessent aussitôt de fonctionner.
#[utoipa::path(
    post,
    path = "/workflows/{id}/webhooks/{node_id}/rotate",
    tag = "webhooks",
    params(
        ("id" = String, Path, description = "Identifiant du workflow"),
        ("node_id" = String, Path, description = "Nœud trigger webhook"),
        NamespaceQuery
    ),
    responses(
        (status = 200, description = "Nouvelle URL", body = WebhookInfo),
        (status = 401, description = "Token absent ou invalide"),
        (status = 403, description = "Manque `runs.execute`, ou `owner` d'autrui sans `*`"),
        (status = 404, description = "Workflow ou nœud webhook inconnu")
    ),
    security(("bearer" = []))
)]
pub async fn rotate_webhook(
    user: AuthenticatedUser,
    State(state): State<WebhooksState>,
    Path((id, node_id)): Path<(String, String)>,
    Query(q): Query<NamespaceQuery>,
) -> Result<Json<WebhookInfo>, WebhookError> {
    state
        .auth
        .require_permission(&user, PERM_RUNS_EXECUTE)
        .await?;
    let ns = state.auth.namespace(&user, q.owner.as_deref()).await?;
    let workflow = load_workflow(&state, &ns, &id).await?;
    let node = webhook_node(&workflow, &node_id).ok_or(WebhookError::NotFound)?;
    let fresh = state.keys.fresh();
    let salt = fresh.salt.clone();
    state
        .endpoints
        .rotate(&ns, &id, &node_id, fresh)
        .await
        .map_err(internal)?;
    state
        .audit(
            user.user_id(),
            "webhooks.rotate",
            &format!("{id}/{node_id}"),
            AuditOutcome::Success,
        )
        .await;
    Ok(Json(info(&state.keys, node, &salt)))
}

/// Sous-router webhook : réception publique (corps ≤ [`MAX_BODY_BYTES`]) et
/// gestion des URL.
pub fn webhooks_router(state: WebhooksState) -> Router {
    Router::new()
        .route(
            "/hooks/:token",
            any(receive).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .route("/workflows/:id/webhooks", get(list_webhooks))
        .route(
            "/workflows/:id/webhooks/:node_id/rotate",
            post(rotate_webhook),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autome_core::{
        AuditSink, InMemoryAuditSink, InMemoryLoginThrottle, InMemoryPermissionStore,
        InMemoryQueue, InMemoryRevocationStore, InMemoryRunStateStore,
        InMemoryWebhookEndpointStore, InMemoryWorkflowRepository, NodeId, PermissionStore, Queue,
        QueueRunDispatcher, RevocationStore, Role, RunStateStore, TokenService,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const SECRET: &[u8] = b"test-secret-must-be-long-enough-32+";
    const FIXED_NOW: i64 = 1_700_000_000;

    struct FixedClock;
    impl crate::auth::Clock for FixedClock {
        fn now_unix_seconds(&self) -> i64 {
            FIXED_NOW
        }
    }

    struct Harness {
        app: Router,
        svc: TokenService,
        perms: Arc<InMemoryPermissionStore>,
        audit: Arc<InMemoryAuditSink>,
        workflows: Arc<InMemoryWorkflowRepository>,
        runs: Arc<InMemoryRunStateStore>,
        queue: Arc<InMemoryQueue>,
    }

    fn harness_with_quota(max: u32) -> Harness {
        let revocation: Arc<dyn RevocationStore + Send + Sync> =
            Arc::new(InMemoryRevocationStore::new());
        let perms = Arc::new(InMemoryPermissionStore::new());
        let audit = Arc::new(InMemoryAuditSink::new());
        let workflows = Arc::new(InMemoryWorkflowRepository::new());
        let runs = Arc::new(InMemoryRunStateStore::new());
        let queue = Arc::new(InMemoryQueue::new());
        let dispatcher = Arc::new(
            QueueRunDispatcher::new(
                Arc::clone(&queue) as Arc<dyn Queue + Send + Sync>,
                Arc::clone(&runs) as Arc<dyn RunStateStore + Send + Sync>,
            )
            // `hook-run`, puis `hook-run-2`… : un id ne sert qu'une fois.
            .with_id_generator({
                let n = std::sync::atomic::AtomicUsize::new(0);
                move || match n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => "hook-run".to_string(),
                    i => format!("hook-run-{}", i + 1),
                }
            }),
        );
        let auth = AuthState::new(
            TokenService::new(SECRET),
            revocation,
            Arc::clone(&perms) as Arc<dyn PermissionStore + Send + Sync>,
            Arc::clone(&audit) as Arc<dyn AuditSink + Send + Sync>,
        )
        .with_clock(Arc::new(FixedClock));
        let keys = WebhookKeys::new(b"encryption-key");
        let state = WebhooksState::new(
            auth,
            Arc::clone(&workflows) as Arc<dyn WorkflowRepository + Send + Sync>,
            Arc::new(InMemoryWebhookEndpointStore::new()),
            dispatcher,
            Arc::new(InMemoryLoginThrottle::new()),
            keys,
        )
        .with_max_per_window(max);
        Harness {
            app: webhooks_router(state),
            svc: TokenService::new(SECRET),
            perms,
            audit,
            workflows,
            runs,
            queue,
        }
    }

    fn harness() -> Harness {
        harness_with_quota(DEFAULT_MAX_PER_WINDOW)
    }

    fn workflow(id: &str, params: Value) -> WorkflowGraph {
        WorkflowGraph {
            id: WorkflowId::new(id),
            name: id.into(),
            nodes: vec![
                Node {
                    position: None,
                    id: NodeId::new("hook"),
                    name: "Webhook GitHub".into(),
                    kind: NodeKind::Trigger {
                        trigger_type: "webhook".into(),
                    },
                    params,
                },
                Node {
                    position: None,
                    id: NodeId::new("manual"),
                    name: "manuel".into(),
                    kind: NodeKind::Trigger {
                        trigger_type: "manual".into(),
                    },
                    params: Value::Null,
                },
            ],
            edges: vec![],
        }
    }

    async fn body_json(response: Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn bearer(h: &Harness, user: &str) -> String {
        format!(
            "Bearer {}",
            h.svc.issue_access(user, 900, FIXED_NOW).unwrap()
        )
    }

    /// Crée `wf` chez alice (éditrice) et renvoie la liste de ses webhooks.
    async fn setup(h: &Harness, params: Value) -> Value {
        h.perms.assign_role("alice", &Role::editor()).unwrap();
        h.workflows
            .create("alice", workflow("wf", params))
            .await
            .unwrap();
        let r = h
            .app
            .clone()
            .oneshot(
                Request::get("/workflows/wf/webhooks")
                    .header("Authorization", bearer(h, "alice"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        body_json(r).await
    }

    async fn call(h: &Harness, req: Request<Body>) -> Response {
        h.app.clone().oneshot(req).await.unwrap()
    }

    fn post_json(path: &str, body: &str) -> Request<Body> {
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn github_signature(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex(&mac.finalize().into_bytes()))
    }

    #[tokio::test]
    async fn listing_exposes_one_url_per_webhook_node_stable_across_reads() {
        let h = harness();
        let list = setup(&h, json!({ "path": "/hooks/x" })).await;
        let items = list["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "le trigger manuel n'a pas d'URL");
        assert_eq!(items[0]["node_id"], "hook");
        assert_eq!(items[0]["method"], "POST");
        assert!(items[0]["signature"].is_null());
        let path = items[0]["path"].as_str().unwrap();
        assert!(is_token(path.strip_prefix(HOOKS_PREFIX).unwrap()));
        assert_ne!(
            items[0]["signing_secret"],
            path.strip_prefix(HOOKS_PREFIX).unwrap()
        );
        // Seconde lecture : même URL.
        let again = call(
            &h,
            Request::get("/workflows/wf/webhooks")
                .header("Authorization", bearer(&h, "alice"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(body_json(again).await["items"][0]["path"], path);
    }

    #[tokio::test]
    async fn receiving_dispatches_a_webhook_run_with_the_request_payload() {
        let h = harness();
        let list = setup(&h, json!({})).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        let r = call(
            &h,
            Request::post(format!("{path}?source=ci"))
                .header("Content-Type", "application/json")
                .header("X-Custom", "abc")
                .header("Authorization", "Bearer secret")
                .header("Cookie", "session=1")
                .header("X-Forwarded-For", "203.0.113.9")
                .body(Body::from(r#"{"action":"opened","n":3}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert_eq!(
            body_json(r).await,
            json!({ "run_id": "hook-run", "status": "queued" })
        );
        let run = h.runs.load("hook-run").await.unwrap().unwrap();
        assert_eq!(run.trigger_type, "webhook");
        assert_eq!(run.workflow_id, "wf");
        assert_eq!(run.owner_id.as_deref(), Some("alice"));

        let delivery = h.queue.poll("runs").await.unwrap().unwrap();
        let payload = &delivery.message.body["payload"];
        assert_eq!(payload["node_id"], "hook");
        assert_eq!(payload["method"], "POST");
        assert_eq!(payload["query"], json!({ "source": "ci" }));
        assert_eq!(payload["body"], json!({ "action": "opened", "n": 3 }));
        assert_eq!(payload["headers"]["x-custom"], "abc");
        let headers = payload["headers"].as_object().unwrap();
        for dropped in ["authorization", "cookie", "x-forwarded-for"] {
            assert!(!headers.contains_key(dropped), "{dropped} transmis au run");
        }
        let events = h.audit.list().await.unwrap();
        assert!(events.iter().any(|e| e.action == "webhooks.receive"
            && e.outcome == AuditOutcome::Success
            && e.resource.as_deref() == Some("hook-run")));
    }

    #[tokio::test]
    async fn non_json_bodies_are_passed_as_text_and_empty_as_null() {
        let h = harness();
        let list = setup(&h, json!({})).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        let r = call(
            &h,
            Request::post(&path)
                .header("Content-Type", "text/plain")
                .body(Body::from("bonjour"))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let delivery = h.queue.poll("runs").await.unwrap().unwrap();
        assert_eq!(delivery.message.body["payload"]["body"], "bonjour");

        let r = call(&h, Request::post(&path).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let delivery = h.queue.poll("runs").await.unwrap().unwrap();
        assert!(delivery.message.body["payload"]["body"].is_null());

        let r = call(&h, post_json(&path, "{pas du json")).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_or_malformed_tokens_are_404() {
        let h = harness();
        setup(&h, json!({})).await;
        let unknown = format!("{HOOKS_PREFIX}{}", "a".repeat(64));
        for path in [unknown.as_str(), "/hooks/court", "/hooks/../../etc"] {
            let r = call(&h, post_json(path, "{}")).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
        }
        assert!(h.runs.load("hook-run").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn removed_node_or_deleted_workflow_kills_the_url() {
        let h = harness();
        let list = setup(&h, json!({})).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        // Le nœud n'est plus un webhook.
        let mut wf = workflow("wf", json!({}));
        wf.nodes.remove(0);
        h.workflows.update("alice", wf).await.unwrap();
        assert_eq!(
            call(&h, post_json(&path, "{}")).await.status(),
            StatusCode::NOT_FOUND
        );
        // Workflow supprimé.
        h.workflows
            .delete("alice", &WorkflowId::new("wf"))
            .await
            .unwrap();
        assert_eq!(
            call(&h, post_json(&path, "{}")).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn method_must_match_the_node_param() {
        let h = harness();
        let list = setup(&h, json!({ "method": "put" })).await;
        assert_eq!(list["items"][0]["method"], "PUT");
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        let r = call(&h, post_json(&path, "{}")).await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(r.headers()[header::ALLOW], "PUT");
        let r = call(
            &h,
            Request::put(&path)
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn github_signature_is_required_when_configured() {
        let h = harness();
        let list = setup(&h, json!({ "signature": "github" })).await;
        let item = &list["items"][0];
        assert_eq!(item["signature"], "github");
        let path = item["path"].as_str().unwrap().to_string();
        let secret = item["signing_secret"].as_str().unwrap().to_string();
        let body = br#"{"action":"opened"}"#;
        let signed = |sig: &str| {
            Request::post(&path)
                .header("Content-Type", "application/json")
                .header("X-Hub-Signature-256", sig)
                .body(Body::from(body.to_vec()))
                .unwrap()
        };
        // Sans signature, mauvaise clé, corps altéré : 401.
        assert_eq!(
            call(&h, post_json(&path, r#"{"action":"opened"}"#))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let wrong = github_signature("autre-secret", body);
        assert_eq!(
            call(&h, signed(&wrong)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let events = h.audit.list().await.unwrap();
        assert!(events
            .iter()
            .any(|e| e.action == "webhooks.receive" && e.outcome == AuditOutcome::Denied));
        // Signature valide : 202, et l'en-tête de signature n'atteint pas le run.
        let good = github_signature(&secret, body);
        assert_eq!(call(&h, signed(&good)).await.status(), StatusCode::ACCEPTED);
        let delivery = h.queue.poll("runs").await.unwrap().unwrap();
        assert!(delivery.message.body["payload"]["headers"]
            .get("x-hub-signature-256")
            .is_none());
    }

    #[tokio::test]
    async fn unsupported_signature_kinds_fail_closed() {
        let h = harness();
        let list = setup(&h, json!({ "signature": "stripe" })).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        assert_eq!(
            call(&h, post_json(&path, "{}")).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn other_github_events_are_acknowledged_without_a_run() {
        let h = harness();
        let list = setup(&h, json!({ "event": "issues" })).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        let with_event = |event: &str| {
            Request::post(&path)
                .header("Content-Type", "application/json")
                .header("X-GitHub-Event", event)
                .body(Body::from("{}"))
                .unwrap()
        };
        assert_eq!(
            call(&h, with_event("ping")).await.status(),
            StatusCode::NO_CONTENT
        );
        assert!(h.runs.load("hook-run").await.unwrap().is_none());
        assert_eq!(
            call(&h, with_event("issues")).await.status(),
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn quota_per_endpoint_returns_429_with_retry_after() {
        let h = harness_with_quota(2);
        let list = setup(&h, json!({})).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        for _ in 0..2 {
            assert_eq!(
                call(&h, post_json(&path, "{}")).await.status(),
                StatusCode::ACCEPTED
            );
        }
        let r = call(&h, post_json(&path, "{}")).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn oversized_bodies_are_rejected() {
        let h = harness();
        let list = setup(&h, json!({})).await;
        let path = list["items"][0]["path"].as_str().unwrap().to_string();
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        let r = call(
            &h,
            Request::post(&path)
                .header("Content-Type", "text/plain")
                .body(Body::from(big))
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn rotation_replaces_url_and_signing_secret() {
        let h = harness();
        let list = setup(&h, json!({})).await;
        let old = &list["items"][0];
        let old_path = old["path"].as_str().unwrap().to_string();
        let r = call(
            &h,
            Request::post("/workflows/wf/webhooks/hook/rotate")
                .header("Authorization", bearer(&h, "alice"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let new = body_json(r).await;
        let new_path = new["path"].as_str().unwrap().to_string();
        assert_ne!(new_path, old_path);
        assert_ne!(new["signing_secret"], old["signing_secret"]);
        assert_eq!(
            call(&h, post_json(&old_path, "{}")).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&h, post_json(&new_path, "{}")).await.status(),
            StatusCode::ACCEPTED
        );
        // Nœud inconnu ou non webhook : 404.
        for node in ["ghost", "manual"] {
            let r = call(
                &h,
                Request::post(format!("/workflows/wf/webhooks/{node}/rotate"))
                    .header("Authorization", bearer(&h, "alice"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{node}");
        }
    }

    #[tokio::test]
    async fn management_requires_runs_execute_and_stays_in_the_callers_namespace() {
        let h = harness();
        setup(&h, json!({})).await;
        let get = |user: &str, uri: &str| {
            Request::get(uri)
                .header("Authorization", bearer(&h, user))
                .body(Body::empty())
                .unwrap()
        };
        // Sans jeton : 401 ; lecteur (pas `runs.execute`) : 403.
        let r = call(
            &h,
            Request::get("/workflows/wf/webhooks")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        h.perms.assign_role("viewer", &Role::viewer()).unwrap();
        assert_eq!(
            call(&h, get("viewer", "/workflows/wf/webhooks"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        // Bob ne voit pas le workflow d'alice, ni par `?owner=`.
        h.perms.assign_role("bob", &Role::editor()).unwrap();
        assert_eq!(
            call(&h, get("bob", "/workflows/wf/webhooks"))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&h, get("bob", "/workflows/wf/webhooks?owner=alice"))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn keys_are_domain_separated_and_deterministic() {
        let keys = WebhookKeys::new(b"k");
        let salt = [7u8; 32];
        assert_eq!(keys.token(&salt), keys.token(&salt));
        assert_ne!(keys.token(&salt), keys.signing_secret(&salt));
        assert_ne!(keys.token(&salt), WebhookKeys::new(b"k2").token(&salt));
        assert!(is_token(&keys.token(&salt)));
        let fresh = keys.fresh();
        assert_eq!(fresh.token_hash, token_hash(&keys.token(&fresh.salt)));
        assert_ne!(keys.fresh().salt, fresh.salt);
        assert_eq!(format!("{keys:?}"), "WebhookKeys(<redacted>)");
    }

    #[test]
    fn hooks_paths_are_redacted_in_logs() {
        assert_eq!(redact_path("/hooks/abcdef"), "/hooks/{token}");
        assert_eq!(redact_path("/workflows/wf"), "/workflows/wf");
    }

    #[test]
    fn quota_env_is_validated() {
        let get = |v: &'static str| move |_: &str| Some(v.to_string());
        assert_eq!(
            max_per_window_from_source(|_| None),
            Ok(DEFAULT_MAX_PER_WINDOW)
        );
        assert_eq!(max_per_window_from_source(get("30")), Ok(30));
        assert!(max_per_window_from_source(get("0")).is_err());
        assert!(max_per_window_from_source(get("beaucoup")).is_err());
    }
}
