//! CORS : origines du frontend autorisées à appeler l'API (#225).
//!
//! Le frontend (Vite `:5173` en dev, nginx `:8081` en compose, ou un domaine
//! à part en production) n'est pas servi par l'API : sans en-têtes CORS, le
//! navigateur bloque toutes ses requêtes (préflight `OPTIONS` en 405).
//!
//! La liste est **explicite** (`AUTOME_CORS_ORIGINS`, séparées par des
//! virgules) : pas de joker, les jetons étant envoyés en `Authorization`
//! (aucun cookie, donc pas d'`allow_credentials`). Défaut : les deux origines
//! locales en `dev`, aucune en `prod` (même origine seulement).

use std::time::Duration;

use autome_core::Profile;
use axum::http::{header, HeaderName, HeaderValue, Method};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::observability::REQUEST_ID_HEADER;

/// Variable d'environnement listant les origines autorisées.
pub const CORS_ORIGINS_ENV: &str = "AUTOME_CORS_ORIGINS";

/// Origines autorisées par défaut en `dev` : Vite et le frontend compose.
pub const DEV_ORIGINS: [&str; 2] = ["http://localhost:5173", "http://localhost:8081"];

/// Durée de mise en cache d'un préflight par le navigateur.
const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(600);

/// Origines autorisées lues depuis l'environnement ; invalide ⇒ refus de démarrer.
pub fn allowed_origins_from_env(profile: Profile) -> Result<Vec<HeaderValue>, String> {
    allowed_origins(profile, std::env::var(CORS_ORIGINS_ENV).ok().as_deref())
}

/// Comme [`allowed_origins_from_env`], valeur brute fournie (tests).
///
/// Variable présente mais vide ⇒ aucune origine (désactive aussi le défaut
/// de `dev`).
pub fn allowed_origins(profile: Profile, raw: Option<&str>) -> Result<Vec<HeaderValue>, String> {
    let Some(raw) = raw else {
        return Ok(match profile {
            Profile::Dev => DEV_ORIGINS
                .iter()
                .map(|o| HeaderValue::from_static(o))
                .collect(),
            Profile::Prod => Vec::new(),
        });
    };
    raw.split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(parse_origin)
        .collect()
}

/// Une origine = `http(s)://hôte[:port]`, sans chemin, query ni joker. Une
/// barre finale est tolérée (copiée d'une barre d'adresse) et retirée : le
/// navigateur n'en envoie jamais dans `Origin`.
fn parse_origin(raw: &str) -> Result<HeaderValue, String> {
    let invalid = |why: &str| format!("{CORS_ORIGINS_ENV} : origine « {raw} » invalide ({why})");
    let origin = raw.strip_suffix('/').unwrap_or(raw);
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| invalid("schéma http:// ou https:// attendu"))?;
    if rest.is_empty() {
        return Err(invalid("hôte manquant"));
    }
    if rest.contains('*') {
        return Err(invalid("joker interdit, listez les origines"));
    }
    if rest.contains(['/', '?', '#', '@']) || rest.chars().any(char::is_whitespace) {
        return Err(invalid("ni chemin, ni query, ni identifiants"));
    }
    HeaderValue::from_str(origin).map_err(|_| invalid("caractères non autorisés"))
}

/// Pose la couche CORS sur tout le routeur ; aucune origine ⇒ routeur inchangé.
pub fn with_cors(router: Router, origins: Vec<HeaderValue>) -> Router {
    if origins.is_empty() {
        return router;
    }
    router.layer(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
            .expose_headers([HeaderName::from_static(REQUEST_ID_HEADER)])
            .max_age(PREFLIGHT_MAX_AGE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn strs(v: &[HeaderValue]) -> Vec<&str> {
        v.iter().map(|h| h.to_str().unwrap()).collect()
    }

    #[test]
    fn defaults_allow_local_frontends_in_dev_and_nothing_in_prod() {
        assert_eq!(
            strs(&allowed_origins(Profile::Dev, None).unwrap()),
            DEV_ORIGINS
        );
        assert!(allowed_origins(Profile::Prod, None).unwrap().is_empty());
        // Variable vide : aucune origine, même en dev.
        assert!(allowed_origins(Profile::Dev, Some(" ")).unwrap().is_empty());
    }

    #[test]
    fn parses_an_explicit_list() {
        let got = allowed_origins(
            Profile::Prod,
            Some("https://app.example.com, http://localhost:3000/ ,"),
        )
        .unwrap();
        assert_eq!(
            strs(&got),
            ["https://app.example.com", "http://localhost:3000"]
        );
    }

    #[test]
    fn rejects_wildcards_paths_and_other_schemes() {
        for bad in [
            "*",
            "https://*.example.com",
            "app.example.com",
            "ftp://example.com",
            "https://",
            "https://example.com/app",
            "https://user@example.com",
            "https://example.com?x=1",
        ] {
            let err = allowed_origins(Profile::Prod, Some(bad)).unwrap_err();
            assert!(err.contains(CORS_ORIGINS_ENV), "{bad}: {err}");
        }
    }

    fn app(origins: &[&str]) -> Router {
        let origins = origins
            .iter()
            .map(|o| HeaderValue::from_str(o).unwrap())
            .collect();
        with_cors(
            Router::new().route("/workflows", get(|| async { "ok" }).post(|| async { "ok" })),
            origins,
        )
    }

    fn preflight(origin: &str) -> Request<Body> {
        Request::builder()
            .method(Method::OPTIONS)
            .uri("/workflows")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn preflight_from_an_allowed_origin_is_answered() {
        let res = app(&["http://localhost:5173"])
            .oneshot(preflight("http://localhost:5173"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(
            h[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:5173"
        );
        let methods = h[header::ACCESS_CONTROL_ALLOW_METHODS].to_str().unwrap();
        assert!(methods.contains("POST") && methods.contains("PUT"));
        let headers = h[header::ACCESS_CONTROL_ALLOW_HEADERS].to_str().unwrap();
        assert!(headers.contains("authorization") && headers.contains("content-type"));
        assert!(h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
    }

    #[tokio::test]
    async fn other_origins_get_no_cors_headers() {
        let res = app(&["http://localhost:5173"])
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert!(res
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn simple_requests_expose_the_request_id() {
        let res = app(&["http://localhost:5173"])
            .oneshot(
                Request::builder()
                    .uri("/workflows")
                    .header(header::ORIGIN, "http://localhost:5173")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://localhost:5173"
        );
        assert_eq!(
            res.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS],
            REQUEST_ID_HEADER
        );
    }

    #[tokio::test]
    async fn without_origins_the_router_is_unchanged() {
        let res = app(&[])
            .oneshot(preflight("http://localhost:5173"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
