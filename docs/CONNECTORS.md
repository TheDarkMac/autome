# 🔌 Guide — créer un connecteur

> Comment ajouter une intégration tierce (Slack, Gmail, …) à Autome avec le
> **SDK connecteur** (`crates/connectors/src/sdk`, feature F4.5). Le squelette
> complet le plus court vit dans [`crate::template`](../crates/connectors/src/template.rs)
> (connecteur `echo`, ~50 lignes) — c'est le point de départ à copier.

Public visé : développeur Rust. Approche **TDD** : un connecteur se teste
entièrement **sans réseau** grâce au transport factice du SDK.

---

## 1. Le SDK en bref

Un connecteur ne parle jamais au réseau directement : il construit une
`HttpRequest`, la confie à un `HttpClient` **injecté**, puis interprète la
`HttpResponse`. Ce découplage rend chaque action testable en isolation.

| Module (`crates/connectors/src/sdk/`) | Rôle |
|---|---|
| `auth` | trait `Credentials` (pose les en-têtes d'auth) + `headers_for` |
| `http` *(crate-level `http.rs`)* | `HttpClient`, `HttpRequest`, `HttpResponse` |
| `api` | `ResponseInterpreter` + `interpret` (mappe la réponse → `Result<Value>`) |
| `schema` | `ActionSchema`, `ParamSpec`, `ParamKind` (description des actions pour l'UI/API) |
| `connector` | trait `Connector` (registre : provider + liste des actions) |
| `retry` | `RetryingClient`, `RetryPolicy`, `Clock` (réessais sur erreurs transitoires) |
| `pagination` | `paginate_all` (agrège les pages via une closure « next ») |
| `testing` | `RecordingClient` (transport factice : enregistre les requêtes, rejoue des réponses) |

Tout est ré-exporté : `use autome_connectors::sdk::{...};`.

---

## 2. Le patron en 4 étapes

Tout connecteur suit la même structure (voir `template.rs`) :

1. **Auth** — un enum `*Auth` qui implémente `Credentials` : il insère les
   en-têtes (`Authorization` Bearer et/ou extras propres au provider).
2. **Interpreter** — un struct `*Interpreter` qui implémente
   `ResponseInterpreter` : il extrait le message d'erreur métier d'une réponse
   non-2xx (et peut transformer une réponse 2xx).
3. **Action(s)** — un struct `Action<C: HttpClient>` (trait `Action` de `core`)
   qui valide les `params`, construit une `HttpRequest` via `headers_for`, puis
   délègue à `interpret`.
4. **Connector** — un struct `*Connector` qui implémente `Connector` : il publie
   le `provider()` et la liste des `ActionSchema` (consommée par la palette du
   frontend et l'API).

---

## 3. Exemple complet (`echo`)

Le connecteur ci-dessous est le squelette minimal. Copiez-le dans
`crates/connectors/src/<provider>.rs`, déclarez-le dans `lib.rs`, puis adaptez.

```rust
use std::collections::BTreeMap;

use autome_core::{Action, Error, Result};
use serde_json::{json, Value};

use crate::http::{HttpClient, HttpRequest};
use crate::sdk::{
    headers_for, interpret, ActionSchema, Connector, Credentials, ParamSpec, ResponseInterpreter,
};

const ECHO_API: &str = "https://echo.example.invalid/v1";

// ── 1. Auth : pose les en-têtes ───────────────────────────────────────────
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EchoAuth {
    ApiKey(String),
}

impl Credentials for EchoAuth {
    fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            EchoAuth::ApiKey(k) => {
                headers.insert("X-Api-Key".into(), k.clone());
            }
        }
    }
}

// ── 2. Interpreter : extrait l'erreur métier d'une réponse non-2xx ─────────
pub struct EchoInterpreter;

impl ResponseInterpreter for EchoInterpreter {
    fn provider(&self) -> &'static str {
        "echo"
    }
    fn extract_error(&self, body: &Value) -> Option<String> {
        body.get("error").and_then(Value::as_str).map(str::to_string)
    }
}

// ── 3. Action : valide les params, construit la requête, interprète ────────
pub struct EchoAction<C: HttpClient> {
    auth: EchoAuth,
    client: C,
}

impl<C: HttpClient> EchoAction<C> {
    pub fn new(auth: EchoAuth, client: C) -> Self {
        Self { auth, client }
    }
}

impl<C: HttpClient> Action for EchoAction<C> {
    fn kind(&self) -> &'static str {
        "echo.say"
    }
    fn execute(&self, params: &Value) -> Result<Value> {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("echo say : `message` requis".into()))?;
        let req = HttpRequest {
            method: "POST".into(),
            url: format!("{ECHO_API}/echo"),
            headers: headers_for(&self.auth, true), // true = il y a un body
            body: Some(json!({ "message": message })),
        };
        let resp = self
            .client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("echo say : {e}")))?;
        interpret(resp, &EchoInterpreter)
    }
}

// ── 4. Connector : registre des actions exposées ──────────────────────────
pub struct EchoConnector;

impl Connector for EchoConnector {
    fn provider(&self) -> &'static str {
        "echo"
    }
    fn action_schemas(&self) -> Vec<ActionSchema> {
        vec![ActionSchema {
            provider: "echo",
            kind: "echo.say",
            label: "Echo — Renvoyer un message",
            description: "Envoie un payload qui est renvoyé tel quel.",
            params: vec![ParamSpec::req_str("message", "Message à renvoyer")],
        }]
    }
}
```

---

## 4. Référence des types clés

### `HttpClient` / `HttpRequest` / `HttpResponse` (`http.rs`)

```rust
pub trait HttpClient {
    fn send(&self, req: &HttpRequest) -> std::result::Result<HttpResponse, String>;
}

pub struct HttpRequest {
    pub method: String,                    // "GET", "POST", …
    pub url: String,
    pub headers: BTreeMap<String, String>, // ordonnés → déterministe
    pub body: Option<Value>,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: Value,                       // corps décodé en JSON
}
```

### `Credentials` / `headers_for` (`sdk::auth`)

```rust
pub trait Credentials {
    fn apply(&self, headers: &mut BTreeMap<String, String>);
}

// Construit les en-têtes : applique l'auth, puis ajoute
// `Content-Type: application/json; charset=utf-8` si `has_body`.
pub fn headers_for<C: Credentials>(creds: &C, has_body: bool) -> BTreeMap<String, String>;
```

### `ResponseInterpreter` / `interpret` (`sdk::api`)

`interpret` applique la règle commune : sur **2xx** → `on_success(body)` (par
défaut, renvoie le body) ; sinon → erreur `Invalid` enrichie de
`extract_error` (ex. `"<provider>: http 400 — bad request"`).

```rust
pub trait ResponseInterpreter {
    fn provider(&self) -> &'static str;
    fn on_success(&self, body: Value) -> Result<Value> { Ok(body) } // override optionnel
    fn extract_error(&self, body: &Value) -> Option<String> { None } // override conseillé
}

pub fn interpret<I: ResponseInterpreter>(resp: HttpResponse, interpreter: &I) -> Result<Value>;
```

### Schéma d'action (`sdk::schema`)

```rust
pub struct ActionSchema {
    pub provider: &'static str,        // "echo"
    pub kind: &'static str,            // "echo.say"  (== Action::kind)
    pub label: &'static str,
    pub description: &'static str,
    pub params: Vec<ParamSpec>,
}

pub enum ParamKind { String, Integer, Boolean, Object, Array { inner: Box<ParamKind> }, Enum { variants: Vec<&'static str> } }
```

Helpers `const` pour les cas courants : `ParamSpec::req_str`, `opt_str`,
`opt_int`, `opt_obj`. Pour les autres types (`Boolean`, `Array`, `Enum`),
construisez `ParamSpec { key, label, kind, required, description }` directement.

> ⚠️ Le `kind` de l'`ActionSchema` **doit** être identique à celui renvoyé par
> `Action::kind()` : c'est la clé qui relie le schéma (UI/API) à l'exécution.

---

## 5. Tester sans réseau (`RecordingClient`)

Le SDK fournit un `HttpClient` factice qui enregistre ce qu'on lui envoie et
rejoue des réponses scriptées :

| Constructeur | Comportement |
|---|---|
| `RecordingClient::ok(body)` | renvoie `200` + `body` (collant : répété) |
| `RecordingClient::with_status(status, body)` | renvoie ce statut/body |
| `RecordingClient::failing(msg)` | renvoie une erreur transport à chaque appel |
| `RecordingClient::queue(vec![...])` | rejoue des réponses en FIFO |

Inspection : `last()` (dernière requête), `all_sent()`, `sent_count()`.

```rust
#[test]
fn echo_sends_post_with_message_and_api_key() {
    let act = EchoAction::new(
        EchoAuth::ApiKey("k-1".into()),
        RecordingClient::ok(json!({ "echoed": "hi" })),
    );
    let out = act.execute(&json!({ "message": "hi" })).unwrap();

    let sent = act.client.last().unwrap();
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.url, "https://echo.example.invalid/v1/echo");
    assert_eq!(sent.headers.get("X-Api-Key").map(String::as_str), Some("k-1"));
    assert_eq!(out["echoed"], json!("hi"));
}

#[test]
fn echo_maps_error_response() {
    let act = EchoAction::new(
        EchoAuth::ApiKey("k".into()),
        RecordingClient::with_status(400, json!({ "error": "bad request" })),
    );
    let err = act.execute(&json!({ "message": "hi" })).unwrap_err();
    assert!(matches!(err, Error::Invalid(m) if m.contains("bad request")));
}
```

Lancer la suite du crate :

```bash
cargo test -p autome-connectors
```

---

## 6. Sujets avancés

- **Pagination** — `sdk::paginate_all` agrège toutes les pages à partir d'une
  closure qui fournit le « curseur suivant ». Exemple réel : `gmail.rs`.
- **Réessais** — enrober un `HttpClient` dans `RetryingClient` (avec une
  `RetryPolicy` et une `Clock` injectable pour des tests déterministes) pour
  retenter automatiquement les erreurs transitoires.
- **OAuth2** — le module `sdk`-voisin `oauth` fournit Authorization Code + PKCE,
  l'échange/rafraîchissement de tokens et un `ConnectorTokenStore` chiffré
  (cf. F6.2/F6.3). Les credentials d'un provider OAuth se déclarent dans
  l'`OAuthRegistry` et sont listés via `OAuthCredentialService`.

Connecteurs réels à étudier, par ordre de complexité :

| Fichier | Particularité |
|---|---|
| `slack.rs` | le plus compact (Bearer + interpreter) |
| `notion.rs` | helpers de blocs (`text_block`, `heading_block`, `extract_plain_text`) |
| `gmail.rs` | MIME + pagination |
| `github.rs` | vérification de signature des webhooks (`verify_github_signature`) |

---

## 7. Enregistrer le connecteur

1. Créer `crates/connectors/src/<provider>.rs` (patron §3).
2. L'ajouter dans `crates/connectors/src/lib.rs` :
   ```rust
   pub mod <provider>;
   pub use <provider>::{<Provider>Auth, <Provider>Connector, /* actions… */};
   ```
3. Écrire les tests (TDD, §5) et garder `cargo fmt` + `cargo clippy -D warnings`
   verts (DoD de la CI).

> ℹ️ **État actuel** : le moteur d'exécution et l'API ne consomment pas encore
> automatiquement le registre de connecteurs (les `action_schemas` ne sont pas
> branchés sur la palette live de l'API/engine — intégration en cours, cf.
> [dette #107](https://github.com/TheDarkMac/autome/issues/107)). Le contrat du
> connecteur (auth → requête → interprétation, + schémas + tests) est lui
> complet et stable : c'est ce que ce guide couvre.

---

## ✅ Checklist

- [ ] Enum `*Auth` + `impl Credentials`.
- [ ] `*Interpreter` + `impl ResponseInterpreter` (avec `extract_error`).
- [ ] Une ou plusieurs `Action<C: HttpClient>` (`kind` == schéma).
- [ ] `*Connector` publiant les `ActionSchema`.
- [ ] Tests `RecordingClient` : succès, paramètre manquant, erreur HTTP.
- [ ] Module déclaré + ré-exporté dans `lib.rs`.
- [ ] `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test -p autome-connectors` verts.

---

## Voir aussi

- [README](../README.md) · [ARCHITECTURE.md](ARCHITECTURE.md) (§3, §9) · [ROADMAP.md](../ROADMAP.md)
- Code : [`crates/connectors/src/sdk/`](../crates/connectors/src/sdk/) · [`template.rs`](../crates/connectors/src/template.rs)
