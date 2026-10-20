//! Catalogue des actions de connecteur (#257).
//!
//! `GET /connectors/actions` sert les [`ActionSchema`] de
//! [`autome_connectors::catalog`] : la palette de nodes et le panneau de
//! configuration du frontend en sont tirés, au lieu d'une liste recopiée à
//! la main qui avait dérivé du worker (`slack.send` au lieu de
//! `slack.send_message`, paramètres absents ou renommés).
//!
//! Route **publique**, comme `/openapi.json` : la liste est statique, identique
//! pour tous, et ne dit rien d'un utilisateur ni de ses credentials.

use autome_connectors::ActionSchema;
use axum::{routing::get, Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

/// Réponse de `GET /connectors/actions`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConnectorActionsResponse {
    /// Actions des connecteurs livrés, connecteur par connecteur.
    pub actions: Vec<ActionSchema>,
}

/// `GET /connectors/actions` — actions de connecteur exécutables par le
/// worker, avec leurs paramètres.
#[utoipa::path(
    get,
    path = "/connectors/actions",
    tag = "connectors",
    responses(
        (status = 200, description = "Actions des connecteurs livrés et leurs paramètres", body = ConnectorActionsResponse)
    )
)]
pub async fn list_actions() -> Json<ConnectorActionsResponse> {
    Json(ConnectorActionsResponse {
        actions: autome_connectors::catalog::action_schemas(),
    })
}

/// Sous-router du catalogue, à merger au niveau racine (cf. `main.rs`).
pub fn connector_actions_router() -> Router {
    Router::new().route("/connectors/actions", get(list_actions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    async fn fetch() -> Value {
        let response = connector_actions_router()
            .oneshot(
                Request::builder()
                    .uri("/connectors/actions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).expect("JSON valide")
    }

    #[tokio::test]
    async fn serves_every_action_of_the_catalog() {
        let body = fetch().await;
        let actions = body["actions"].as_array().expect("actions");
        let kinds: Vec<&str> = actions.iter().filter_map(|a| a["kind"].as_str()).collect();
        let expected: Vec<&str> = autome_connectors::catalog::action_schemas()
            .iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(kinds, expected);
        assert!(kinds.contains(&"slack.send_message"));
        assert!(
            !kinds.contains(&"echo.say"),
            "le connecteur exemple n'est pas servi"
        );
    }

    #[tokio::test]
    async fn describes_each_param_with_its_type_and_requirement() {
        let body = fetch().await;
        let send = body["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["kind"] == "slack.send_message")
            .expect("slack.send_message");
        assert_eq!(send["provider"], "slack");
        let text = send["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "text")
            .expect("param text");
        assert_eq!(text["required"], true);
        assert_eq!(text["kind"]["type"], "text");
        let blocks = send["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "blocks")
            .expect("param blocks");
        assert_eq!(blocks["kind"]["type"], "array");
        assert_eq!(blocks["kind"]["inner"]["type"], "object");
    }
}
