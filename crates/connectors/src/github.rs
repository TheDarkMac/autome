//! Connecteur GitHub (F4.4) — actions issues / PR + vérification de
//! signature webhook.
//!
//! Architecture identique à [`crate::slack`] / [`crate::gmail`] /
//! [`crate::notion`] : aucune dépendance réseau — le transport est injecté
//! via [`crate::HttpClient`]. L'authentification se limite ici au **token
//! statique** (`GithubAuth::Token`) : PAT (`ghp_…`) ou installation token
//! GitHub App. Le JWT App → installation token relève de F6.2.
//!
//! GitHub impose `User-Agent` sur tous les appels REST (sinon 403) ; on
//! envoie systématiquement aussi `Accept: application/vnd.github+json` et
//! `X-GitHub-Api-Version: 2022-11-28`.
//!
//! Schéma des `params` (résumé) :
//! ```json
//! // create_issue (F4.4.b)
//! { "owner": "ando", "repo": "autome", "title": "Bug X",
//!   "body": "détails…", "assignees": ["alice"], "labels": ["bug"] }
//! // comment_issue (F4.4.b)
//! { "owner": "ando", "repo": "autome", "issue_number": 42, "body": "yo" }
//! // close_issue (F4.4.b)
//! { "owner": "ando", "repo": "autome", "issue_number": 42,
//!   "state_reason": "completed" }
//! // list_pulls (F4.4.c)
//! { "owner": "ando", "repo": "autome", "state": "open",
//!   "head": "ando:feature", "base": "main", "per_page": 30, "page": 1 }
//! // create_pull (F4.4.c)
//! { "owner": "ando", "repo": "autome", "title": "T",
//!   "head": "feature/x", "base": "main", "body": "…", "draft": false }
//! // merge_pull (F4.4.c)
//! { "owner": "ando", "repo": "autome", "pull_number": 7,
//!   "merge_method": "squash", "commit_title": "…" }
//! ```
//!
//! Signature webhook (F4.4.d) : [`verify_github_signature`] valide un
//! header `X-Hub-Signature-256: sha256=<hex>` contre le HMAC-SHA256 du
//! body brut. Comparaison constant-time via `hmac::Mac::verify_slice`.
//!
//! NB : `percent_encode` et `interpret` sont volontairement dupliqués
//! depuis les autres connecteurs — facto dans le SDK connecteur (F4.5).

use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::http::{HttpClient, HttpRequest, HttpResponse};

type HmacSha256 = Hmac<Sha256>;

/// Racine de l'API GitHub REST.
const GITHUB_API: &str = "https://api.github.com";
/// Version d'API GitHub ciblée (`X-GitHub-Api-Version`).
pub const GITHUB_API_VERSION: &str = "2022-11-28";
/// `User-Agent` envoyé à GitHub (obligatoire — sinon 403).
pub const USER_AGENT: &str = "autome-connectors";

/// Stratégie d'authentification GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GithubAuth {
    /// PAT (`ghp_…`) ou installation token GitHub App.
    Token(String),
}

impl GithubAuth {
    fn bearer(&self) -> String {
        match self {
            GithubAuth::Token(t) => format!("Bearer {t}"),
        }
    }
}

fn common_headers(auth: &GithubAuth) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert("Authorization".into(), auth.bearer());
    h.insert("Accept".into(), "application/vnd.github+json".into());
    h.insert("X-GitHub-Api-Version".into(), GITHUB_API_VERSION.into());
    h.insert("User-Agent".into(), USER_AGENT.into());
    h
}

fn write_headers(auth: &GithubAuth, with_body: bool) -> BTreeMap<String, String> {
    let mut h = common_headers(auth);
    if with_body {
        h.insert(
            "Content-Type".into(),
            "application/json; charset=utf-8".into(),
        );
    }
    h
}

/// Traduit la réponse GitHub en `Result<Value>`.
fn interpret(resp: HttpResponse) -> Result<Value> {
    if (200..300).contains(&resp.status) {
        return Ok(resp.body);
    }
    let msg = resp
        .body
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| resp.body.to_string());
    Err(Error::Invalid(format!(
        "github: http {} — {}",
        resp.status, msg
    )))
}

/// Encodage URL minimal (RFC 3986 « unreserved »).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn to_query(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn require_str<'a>(params: &'a Value, key: &str, ctx: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid(format!("github {ctx} : `{key}` requis (string)")))
}

fn require_uint(params: &Value, key: &str, ctx: &str) -> Result<u64> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Invalid(format!("github {ctx} : `{key}` requis (entier ≥ 0)")))
}

fn repo_path(owner: &str, repo: &str) -> String {
    format!(
        "{GITHUB_API}/repos/{}/{}",
        percent_encode(owner),
        percent_encode(repo)
    )
}

// ──────────────────── F4.4.d — Vérif signature webhook ────────────────────

/// Vérifie un header `X-Hub-Signature-256` (`sha256=<hex>`) contre le HMAC
/// SHA-256 du corps brut.
///
/// La comparaison est faite en temps constant par `hmac::Mac::verify_slice`.
///
/// Retourne `false` (sans paniquer) si :
/// - le header n'a pas le préfixe `sha256=` ;
/// - la partie hex est mal formée ;
/// - la signature décodée ne fait pas 32 octets ;
/// - le secret est vide (cas où l'app n'est pas configurée) — bloque toute
///   réception qui aurait été signée avec un secret partagé valide.
pub fn verify_github_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_part) = header.strip_prefix("sha256=") else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let Ok(sig) = hex::decode(hex_part) else {
        return false;
    };
    if sig.len() != 32 {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

// ───────────────────────── Actions ─────────────────────────

/// Action `POST /repos/{owner}/{repo}/issues` (F4.4.b).
pub struct GithubCreateIssue<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubCreateIssue<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "create_issue")?;
        let repo = require_str(params, "repo", "create_issue")?;
        let title = require_str(params, "title", "create_issue")?;
        let mut body = serde_json::Map::new();
        body.insert("title".into(), Value::String(title.into()));
        for key in ["body", "assignees", "labels", "milestone"] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{}/issues", repo_path(owner, repo)),
            headers: write_headers(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for GithubCreateIssue<C> {
    fn kind(&self) -> &'static str {
        "github.create_issue"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github create_issue : {e}")))?;
        interpret(resp)
    }
}

/// Action `POST /repos/{owner}/{repo}/issues/{n}/comments` (F4.4.b).
pub struct GithubCommentIssue<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubCommentIssue<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "comment_issue")?;
        let repo = require_str(params, "repo", "comment_issue")?;
        let issue = require_uint(params, "issue_number", "comment_issue")?;
        let body_text = require_str(params, "body", "comment_issue")?;
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{}/issues/{}/comments", repo_path(owner, repo), issue),
            headers: write_headers(&self.auth, true),
            body: Some(json!({ "body": body_text })),
        })
    }
}

impl<C: HttpClient> Action for GithubCommentIssue<C> {
    fn kind(&self) -> &'static str {
        "github.comment_issue"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github comment_issue : {e}")))?;
        interpret(resp)
    }
}

/// Action `PATCH /repos/{owner}/{repo}/issues/{n}` avec `{state:"closed"}`
/// (F4.4.b).
pub struct GithubCloseIssue<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubCloseIssue<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "close_issue")?;
        let repo = require_str(params, "repo", "close_issue")?;
        let issue = require_uint(params, "issue_number", "close_issue")?;
        let mut body = serde_json::Map::new();
        body.insert("state".into(), Value::String("closed".into()));
        if let Some(reason) = params.get("state_reason").and_then(Value::as_str) {
            body.insert("state_reason".into(), Value::String(reason.into()));
        }
        Ok(HttpRequest {
            method: "PATCH".into(),
            url: format!("{}/issues/{}", repo_path(owner, repo), issue),
            headers: write_headers(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for GithubCloseIssue<C> {
    fn kind(&self) -> &'static str {
        "github.close_issue"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github close_issue : {e}")))?;
        interpret(resp)
    }
}

/// Action `GET /repos/{owner}/{repo}/pulls` (F4.4.c).
pub struct GithubListPulls<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubListPulls<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "list_pulls")?;
        let repo = require_str(params, "repo", "list_pulls")?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (param_key, api_key) in [
            ("state", "state"),
            ("head", "head"),
            ("base", "base"),
            ("sort", "sort"),
            ("direction", "direction"),
            ("per_page", "per_page"),
            ("page", "page"),
        ] {
            if let Some(v) = params.get(param_key) {
                let s = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                pairs.push((api_key.into(), s));
            }
        }
        let mut url = format!("{}/pulls", repo_path(owner, repo));
        if !pairs.is_empty() {
            url.push('?');
            url.push_str(&to_query(&pairs));
        }
        Ok(HttpRequest {
            method: "GET".into(),
            url,
            headers: write_headers(&self.auth, false),
            body: None,
        })
    }
}

impl<C: HttpClient> Action for GithubListPulls<C> {
    fn kind(&self) -> &'static str {
        "github.list_pulls"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github list_pulls : {e}")))?;
        interpret(resp)
    }
}

/// Action `POST /repos/{owner}/{repo}/pulls` (F4.4.c).
pub struct GithubCreatePull<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubCreatePull<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "create_pull")?;
        let repo = require_str(params, "repo", "create_pull")?;
        let title = require_str(params, "title", "create_pull")?;
        let head = require_str(params, "head", "create_pull")?;
        let base = require_str(params, "base", "create_pull")?;
        let mut body = serde_json::Map::new();
        body.insert("title".into(), Value::String(title.into()));
        body.insert("head".into(), Value::String(head.into()));
        body.insert("base".into(), Value::String(base.into()));
        for key in ["body", "draft", "maintainer_can_modify", "issue"] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{}/pulls", repo_path(owner, repo)),
            headers: write_headers(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for GithubCreatePull<C> {
    fn kind(&self) -> &'static str {
        "github.create_pull"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github create_pull : {e}")))?;
        interpret(resp)
    }
}

/// Action `PUT /repos/{owner}/{repo}/pulls/{n}/merge` (F4.4.c).
pub struct GithubMergePull<C: HttpClient> {
    auth: GithubAuth,
    client: C,
}

impl<C: HttpClient> GithubMergePull<C> {
    /// Construit l'action avec l'auth et le transport injecté.
    pub fn new(auth: GithubAuth, client: C) -> Self {
        Self { auth, client }
    }

    fn build_request(&self, params: &Value) -> Result<HttpRequest> {
        let owner = require_str(params, "owner", "merge_pull")?;
        let repo = require_str(params, "repo", "merge_pull")?;
        let pr = require_uint(params, "pull_number", "merge_pull")?;
        let mut body = serde_json::Map::new();
        for key in ["commit_title", "commit_message", "sha", "merge_method"] {
            if let Some(v) = params.get(key) {
                body.insert(key.into(), v.clone());
            }
        }
        Ok(HttpRequest {
            method: "PUT".into(),
            url: format!("{}/pulls/{}/merge", repo_path(owner, repo), pr),
            headers: write_headers(&self.auth, true),
            body: Some(Value::Object(body)),
        })
    }
}

impl<C: HttpClient> Action for GithubMergePull<C> {
    fn kind(&self) -> &'static str {
        "github.merge_pull"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let req = self.build_request(params)?;
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("github merge_pull : {e}")))?;
        interpret(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct MockClient {
        last: RefCell<Option<HttpRequest>>,
        resp: HttpResponse,
        fail: Option<String>,
    }

    impl MockClient {
        fn ok(body: Value) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse { status: 200, body },
                fail: None,
            }
        }
        fn with_status(status: u16, body: Value) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse { status, body },
                fail: None,
            }
        }
        fn failing(msg: &str) -> Self {
            Self {
                last: RefCell::new(None),
                resp: HttpResponse {
                    status: 200,
                    body: json!({}),
                },
                fail: Some(msg.into()),
            }
        }
    }

    impl HttpClient for MockClient {
        fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String> {
            *self.last.borrow_mut() = Some(req.clone());
            if let Some(m) = &self.fail {
                Err(m.clone())
            } else {
                Ok(self.resp.clone())
            }
        }
    }

    fn check_common_headers(headers: &BTreeMap<String, String>, with_body: bool) {
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer ghp_test")
        );
        assert_eq!(
            headers.get("Accept").map(String::as_str),
            Some("application/vnd.github+json")
        );
        assert_eq!(
            headers.get("X-GitHub-Api-Version").map(String::as_str),
            Some("2022-11-28")
        );
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("autome-connectors")
        );
        if with_body {
            assert_eq!(
                headers.get("Content-Type").map(String::as_str),
                Some("application/json; charset=utf-8")
            );
        } else {
            assert!(!headers.contains_key("Content-Type"));
        }
    }

    // ───── F4.4.d — verify_github_signature ─────

    /// HMAC-SHA256 attendu pour `secret=It's a Secret to Everybody`,
    /// `body=Hello, World!` — vecteur tiré de la doc GitHub Webhooks.
    const GITHUB_VEC_SECRET: &[u8] = b"It's a Secret to Everybody";
    const GITHUB_VEC_BODY: &[u8] = b"Hello, World!";
    const GITHUB_VEC_HMAC_HEX: &str =
        "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn signature_verifies_with_correct_hmac() {
        let header = format!("sha256={GITHUB_VEC_HMAC_HEX}");
        assert!(verify_github_signature(
            GITHUB_VEC_SECRET,
            GITHUB_VEC_BODY,
            &header
        ));
    }

    #[test]
    fn signature_rejects_wrong_hmac() {
        let mut bad = String::from(GITHUB_VEC_HMAC_HEX);
        // Flip un caractère hex au milieu
        bad.replace_range(10..11, "0");
        let header = format!("sha256={bad}");
        assert!(!verify_github_signature(
            GITHUB_VEC_SECRET,
            GITHUB_VEC_BODY,
            &header
        ));
    }

    #[test]
    fn signature_rejects_missing_prefix() {
        assert!(!verify_github_signature(
            GITHUB_VEC_SECRET,
            GITHUB_VEC_BODY,
            GITHUB_VEC_HMAC_HEX,
        ));
    }

    #[test]
    fn signature_rejects_wrong_length() {
        // 30 octets hex au lieu de 32
        let short = "0".repeat(60);
        assert!(!verify_github_signature(
            GITHUB_VEC_SECRET,
            GITHUB_VEC_BODY,
            &format!("sha256={short}"),
        ));
    }

    #[test]
    fn signature_rejects_bad_hex() {
        assert!(!verify_github_signature(
            GITHUB_VEC_SECRET,
            GITHUB_VEC_BODY,
            "sha256=ZZ",
        ));
    }

    #[test]
    fn signature_rejects_empty_secret() {
        let header = format!("sha256={GITHUB_VEC_HMAC_HEX}");
        assert!(!verify_github_signature(b"", GITHUB_VEC_BODY, &header,));
    }

    #[test]
    fn signature_rejects_when_body_tampered() {
        let header = format!("sha256={GITHUB_VEC_HMAC_HEX}");
        assert!(!verify_github_signature(
            GITHUB_VEC_SECRET,
            b"Hello, world!", // minuscule
            &header,
        ));
    }

    // ───── F4.4.a — auth + UA + Accept + API-Version ─────

    #[test]
    fn all_actions_inject_required_headers() {
        let auth = GithubAuth::Token("ghp_test".into());

        let c = GithubCreateIssue::new(auth.clone(), MockClient::ok(json!({ "number": 1 })));
        c.execute(&json!({ "owner": "o", "repo": "r", "title": "T" }))
            .unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, true);

        let c = GithubCommentIssue::new(auth.clone(), MockClient::ok(json!({ "id": 9 })));
        c.execute(&json!({ "owner": "o", "repo": "r", "issue_number": 1, "body": "yo" }))
            .unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, true);

        let c = GithubCloseIssue::new(auth.clone(), MockClient::ok(json!({ "number": 1 })));
        c.execute(&json!({ "owner": "o", "repo": "r", "issue_number": 1 }))
            .unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, true);

        let c = GithubListPulls::new(auth.clone(), MockClient::ok(json!([])));
        c.execute(&json!({ "owner": "o", "repo": "r" })).unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, false);

        let c = GithubCreatePull::new(auth.clone(), MockClient::ok(json!({ "number": 1 })));
        c.execute(&json!({
            "owner": "o", "repo": "r", "title": "T", "head": "h", "base": "main"
        }))
        .unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, true);

        let c = GithubMergePull::new(auth, MockClient::ok(json!({ "merged": true })));
        c.execute(&json!({ "owner": "o", "repo": "r", "pull_number": 1 }))
            .unwrap();
        check_common_headers(&c.client.last.borrow().as_ref().unwrap().headers, true);
    }

    // ───── F4.4.b — issues ─────

    #[test]
    fn create_issue_builds_post_with_owner_repo_title() {
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "number": 7, "html_url": "https://github.com/o/r/issues/7" })),
        );
        let out = act
            .execute(&json!({
                "owner": "o", "repo": "r", "title": "Bug",
                "body": "details", "labels": ["bug", "ci"], "assignees": ["alice"]
            }))
            .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://api.github.com/repos/o/r/issues");
        let body = sent.body.unwrap();
        assert_eq!(body["title"], json!("Bug"));
        assert_eq!(body["body"], json!("details"));
        assert_eq!(body["labels"], json!(["bug", "ci"]));
        assert_eq!(body["assignees"], json!(["alice"]));
        assert_eq!(out["number"], json!(7));
    }

    #[test]
    fn create_issue_requires_owner_repo_title() {
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({})),
        );
        assert!(matches!(
            act.execute(&json!({ "repo": "r", "title": "T" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`owner`")
        ));
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({})),
        );
        assert!(matches!(
            act.execute(&json!({ "owner": "o", "title": "T" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`repo`")
        ));
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({})),
        );
        assert!(matches!(
            act.execute(&json!({ "owner": "o", "repo": "r" })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`title`")
        ));
    }

    #[test]
    fn comment_issue_builds_post_to_comments_path() {
        let act = GithubCommentIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "id": 11 })),
        );
        act.execute(&json!({
            "owner": "o", "repo": "r", "issue_number": 42, "body": "hi"
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(
            sent.url,
            "https://api.github.com/repos/o/r/issues/42/comments"
        );
        assert_eq!(sent.body.unwrap(), json!({ "body": "hi" }));
    }

    #[test]
    fn close_issue_patches_with_state_closed_and_reason() {
        let act = GithubCloseIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "number": 42, "state": "closed" })),
        );
        act.execute(&json!({
            "owner": "o", "repo": "r", "issue_number": 42,
            "state_reason": "completed"
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "PATCH");
        assert_eq!(sent.url, "https://api.github.com/repos/o/r/issues/42");
        let body = sent.body.unwrap();
        assert_eq!(body["state"], json!("closed"));
        assert_eq!(body["state_reason"], json!("completed"));
    }

    // ───── F4.4.c — PR ─────

    #[test]
    fn list_pulls_builds_get_with_query() {
        let act = GithubListPulls::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!([{ "number": 1 }])),
        );
        act.execute(&json!({
            "owner": "o", "repo": "r",
            "state": "open", "head": "o:feature/x", "base": "main",
            "sort": "updated", "direction": "desc",
            "per_page": 30, "page": 2
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "GET");
        assert!(
            sent.url
                .starts_with("https://api.github.com/repos/o/r/pulls?"),
            "url={}",
            sent.url
        );
        assert!(sent.url.contains("state=open"));
        // `:` et `/` doivent être pourcent-encodés dans `head`
        assert!(
            sent.url.contains("head=o%3Afeature%2Fx"),
            "url={}",
            sent.url
        );
        assert!(sent.url.contains("base=main"));
        assert!(sent.url.contains("sort=updated"));
        assert!(sent.url.contains("direction=desc"));
        assert!(sent.url.contains("per_page=30"));
        assert!(sent.url.contains("page=2"));
        assert!(sent.body.is_none());
    }

    #[test]
    fn list_pulls_works_without_query() {
        let act = GithubListPulls::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!([])),
        );
        act.execute(&json!({ "owner": "o", "repo": "r" })).unwrap();
        let url = act.client.last.borrow().clone().unwrap().url;
        assert_eq!(url, "https://api.github.com/repos/o/r/pulls");
    }

    #[test]
    fn create_pull_posts_with_required_and_optional_fields() {
        let act = GithubCreatePull::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "number": 99, "html_url": "https://github.com/o/r/pull/99" })),
        );
        act.execute(&json!({
            "owner": "o", "repo": "r",
            "title": "T", "head": "feature/x", "base": "main",
            "body": "details", "draft": true
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.url, "https://api.github.com/repos/o/r/pulls");
        let body = sent.body.unwrap();
        assert_eq!(body["title"], json!("T"));
        assert_eq!(body["head"], json!("feature/x"));
        assert_eq!(body["base"], json!("main"));
        assert_eq!(body["body"], json!("details"));
        assert_eq!(body["draft"], json!(true));
    }

    #[test]
    fn create_pull_requires_title_head_base() {
        let act = GithubCreatePull::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({})),
        );
        assert!(matches!(
            act.execute(&json!({
                "owner": "o", "repo": "r", "head": "h", "base": "main"
            })).unwrap_err(),
            Error::Invalid(ref m) if m.contains("`title`")
        ));
    }

    #[test]
    fn merge_pull_puts_with_optional_body() {
        let act = GithubMergePull::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "merged": true, "message": "Pull Request merged" })),
        );
        act.execute(&json!({
            "owner": "o", "repo": "r", "pull_number": 5,
            "merge_method": "squash", "commit_title": "squashed"
        }))
        .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "PUT");
        assert_eq!(sent.url, "https://api.github.com/repos/o/r/pulls/5/merge");
        let body = sent.body.unwrap();
        assert_eq!(body["merge_method"], json!("squash"));
        assert_eq!(body["commit_title"], json!("squashed"));
    }

    #[test]
    fn merge_pull_works_without_optional_body() {
        let act = GithubMergePull::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({ "merged": true })),
        );
        act.execute(&json!({ "owner": "o", "repo": "r", "pull_number": 5 }))
            .unwrap();
        let sent = act.client.last.borrow().clone().unwrap();
        assert_eq!(sent.method, "PUT");
        assert_eq!(sent.body.unwrap(), json!({}));
    }

    // ───── owner / repo pourcent-encodés ─────

    #[test]
    fn owner_repo_are_percent_encoded() {
        // Cas peu probable mais : un owner contenant un `/` ne doit pas
        // permettre d'injection de path (« octocat/../evil » → « octocat%2F..%2Fevil »).
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::ok(json!({})),
        );
        act.execute(&json!({ "owner": "a/b", "repo": "r", "title": "T" }))
            .unwrap();
        let url = act.client.last.borrow().clone().unwrap().url;
        assert!(url.starts_with("https://api.github.com/repos/a%2Fb/r/issues"));
    }

    // ───── mapping erreurs ─────

    #[test]
    fn http_non_2xx_extracts_message() {
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::with_status(
                404,
                json!({ "message": "Not Found", "documentation_url": "https://…" }),
            ),
        );
        let err = act
            .execute(&json!({ "owner": "o", "repo": "r", "title": "T" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => {
                assert!(m.contains("http 404"), "msg={m}");
                assert!(m.contains("Not Found"), "msg={m}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_is_propagated() {
        let act = GithubCreateIssue::new(
            GithubAuth::Token("ghp_test".into()),
            MockClient::failing("timeout"),
        );
        let err = act
            .execute(&json!({ "owner": "o", "repo": "r", "title": "T" }))
            .unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("timeout"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn kinds_are_stable() {
        let t = || GithubAuth::Token("ghp_test".into());
        let m = || MockClient::ok(json!({}));
        assert_eq!(
            GithubCreateIssue::new(t(), m()).kind(),
            "github.create_issue"
        );
        assert_eq!(
            GithubCommentIssue::new(t(), m()).kind(),
            "github.comment_issue"
        );
        assert_eq!(GithubCloseIssue::new(t(), m()).kind(), "github.close_issue");
        assert_eq!(GithubListPulls::new(t(), m()).kind(), "github.list_pulls");
        assert_eq!(GithubCreatePull::new(t(), m()).kind(), "github.create_pull");
        assert_eq!(GithubMergePull::new(t(), m()).kind(), "github.merge_pull");
    }
}
