//! Spec OpenAPI 3 générée à la compilation (F7.4.c).
//!
//! [`ApiDoc`] agrège tous les `#[utoipa::path]` posés dans les modules
//! `workflows`, `runs` et `credentials`. Le sous-router [`openapi_router`]
//! expose un seul endpoint :
//!
//! - `GET /openapi.json` : sérialise la spec via [`utoipa::OpenApi::openapi`].
//!
//! La spec déclare un scheme de sécurité `bearer` (Bearer JWT, format
//! `Authorization: Bearer <token>`) que chaque handler référence via
//! `security(("bearer" = []))`. Aucun endpoint d'`auth` du présent module
//! n'est exposé — seul le contenu fonctionnel (workflows, runs,
//! credentials) figure dans la spec.

use axum::{response::Json, routing::get, Router};
use utoipa::{
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
    Modify, OpenApi,
};

/// Document OpenAPI racine. Tous les handlers décorés `#[utoipa::path]`
/// sont enregistrés dans `paths(...)` ; les DTO sont collectés dans
/// `components(schemas(...))`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Autome API",
        version = "0.1.0",
        description = "Backend HTTP du moteur d'automatisation Autome.",
    ),
    paths(
        // workflows (F7.1)
        crate::workflows::list_workflows,
        crate::workflows::create_workflow,
        crate::workflows::get_workflow,
        crate::workflows::update_workflow,
        crate::workflows::delete_workflow,
        // runs (F7.2)
        crate::runs::create_run,
        crate::runs::get_run,
        crate::runs::list_runs,
        // credentials (F7.3)
        crate::credentials::list_providers,
        crate::credentials::start_authorize,
        crate::credentials::complete_authorize,
        crate::credentials::list_credentials,
        crate::credentials::delete_credential,
    ),
    components(schemas(
        crate::workflows::ListResponse,
        crate::runs::CreateRunBody,
        crate::runs::CreateRunResponse,
        crate::runs::ListRunsResponse,
        crate::credentials::ProviderDto,
        crate::credentials::ListProvidersResponse,
        crate::credentials::AuthorizeResponse,
        crate::credentials::CallbackBody,
        crate::credentials::CredentialDto,
        crate::credentials::ListCredentialsResponse,
    )),
    tags(
        (name = "workflows", description = "CRUD des workflows."),
        (name = "runs", description = "Déclenchement et suivi des runs."),
        (name = "credentials", description = "OAuth2 connecteurs tiers."),
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

/// Ajoute le scheme `bearer` (JWT) à la spec — utoipa ne le déduit pas
/// automatiquement, on le pose explicitement ici une fois pour tous les
/// endpoints qui le référencent.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some(
                        "Token JWT émis par `/auth/login` — header `Authorization: Bearer <token>`.",
                    ))
                    .build(),
            ),
        );
    }
}

/// Handler `GET /openapi.json`.
pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Construit le sous-router OpenAPI — une seule route, à merger au niveau
/// racine du serveur (cf. `main.rs`).
pub fn openapi_router() -> Router {
    Router::new().route("/openapi.json", get(openapi_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn fetch_spec() -> serde_json::Value {
        let app = openapi_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).expect("spec OpenAPI = JSON valide")
    }

    #[tokio::test]
    async fn openapi_json_returns_200_and_valid_openapi_3() {
        let spec = fetch_spec().await;
        // OpenAPI 3.x — exact version exposed by utoipa
        let version = spec["openapi"].as_str().unwrap_or("");
        assert!(
            version.starts_with("3."),
            "version OpenAPI attendue 3.x, trouvée {version}"
        );
        assert_eq!(spec["info"]["title"], "Autome API");
    }

    #[tokio::test]
    async fn openapi_lists_every_documented_endpoint() {
        let spec = fetch_spec().await;
        let paths = spec["paths"].as_object().expect("paths existe");
        // F7.1 — workflows
        assert!(paths.contains_key("/workflows"));
        assert!(paths.contains_key("/workflows/{id}"));
        // F7.2 — runs
        assert!(paths.contains_key("/workflows/{id}/runs"));
        assert!(paths.contains_key("/runs/{run_id}"));
        // F7.3 — credentials
        assert!(paths.contains_key("/credentials"));
        assert!(paths.contains_key("/credentials/providers"));
        assert!(paths.contains_key("/credentials/{provider}"));
        assert!(paths.contains_key("/credentials/{provider}/authorize"));
        assert!(paths.contains_key("/credentials/{provider}/callback"));
    }

    #[tokio::test]
    async fn openapi_declares_bearer_security_scheme() {
        let spec = fetch_spec().await;
        let scheme = &spec["components"]["securitySchemes"]["bearer"];
        assert_eq!(scheme["type"], "http");
        assert_eq!(scheme["scheme"], "bearer");
        assert_eq!(scheme["bearerFormat"], "JWT");
    }

    #[tokio::test]
    async fn openapi_handlers_reference_bearer_security() {
        let spec = fetch_spec().await;
        // Au moins un endpoint référence bearer dans son `security`.
        let post_workflows = &spec["paths"]["/workflows"]["post"]["security"];
        let arr = post_workflows.as_array().expect("security est une liste");
        assert!(
            arr.iter().any(|s| s["bearer"].is_array()),
            "POST /workflows doit exiger bearer : {arr:?}"
        );
    }

    #[tokio::test]
    async fn openapi_does_not_leak_any_secret_keyword() {
        // Sentinelle : on ne doit voir aucun nom de champ "secret".
        // (Les commentaires textuels qui mentionnent "secret" sont OK : on
        // cherche les clés JSON exactes "client_secret", "access_token",
        // "refresh_token" qui dénoteraient une fuite de schéma.)
        let spec = fetch_spec().await;
        let text = spec.to_string();
        for forbidden in ["\"client_secret\"", "\"access_token\"", "\"refresh_token\""] {
            assert!(
                !text.contains(forbidden),
                "la spec OpenAPI ne doit pas contenir le champ {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn openapi_tags_cover_all_domains() {
        let spec = fetch_spec().await;
        let tags = spec["tags"].as_array().expect("tags est une liste");
        let names: Vec<&str> = tags.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in ["workflows", "runs", "credentials"] {
            assert!(
                names.contains(&expected),
                "tag '{expected}' attendu dans {names:?}"
            );
        }
    }
}
