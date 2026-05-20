//! Pagination via closure « next request » (F4.5.b).
//!
//! Suit la pagination en alternant `client.send(req)` puis
//! `next(&body)` jusqu'à ce que `next` retourne `None`. Aggrège tous les
//! body 2xx dans un `Vec<Value>` : l'extraction métier (concaténer
//! `messages`, `results`, …) est laissée à l'appelant.
//!
//! Permet de couvrir les trois styles rencontrés en F4.* :
//! - body cursor (Notion, Slack) — `next_cursor`/`response_metadata.next_cursor` ;
//! - body token (Gmail) — `nextPageToken` injecté en query ;
//! - Link header (GitHub) — quand la prochaine étape exposera les headers
//!   sur `HttpResponse` (cf. domaine 7).

use autome_core::{Error, Result};
use serde_json::Value;

use crate::http::{HttpClient, HttpRequest};

/// Suit la pagination en boucle jusqu'à ce que `next` renvoie `None`.
///
/// Renvoie `Err` au premier statut non-`2xx` ou au premier échec transport.
pub fn paginate_all<C, F>(client: &C, initial: HttpRequest, next: F) -> Result<Vec<Value>>
where
    C: HttpClient,
    F: Fn(&Value) -> Option<HttpRequest>,
{
    let mut req = initial;
    let mut pages = Vec::new();
    loop {
        let resp = client
            .send(&req)
            .map_err(|e| Error::Invalid(format!("paginate: {e}")))?;
        if !(200..300).contains(&resp.status) {
            return Err(Error::Invalid(format!(
                "paginate: http {} — {}",
                resp.status, resp.body
            )));
        }
        let body = resp.body;
        let next_req = next(&body);
        pages.push(body);
        match next_req {
            Some(r) => req = r,
            None => break,
        }
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use crate::sdk::testing::RecordingClient;
    use serde_json::json;

    fn req(url: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: Default::default(),
            body: None,
        }
    }

    #[test]
    fn stops_when_next_returns_none() {
        let client = RecordingClient::queue(vec![Ok(HttpResponse {
            status: 200,
            body: json!({ "items": [1, 2], "has_more": false }),
        })]);
        let pages = paginate_all(&client, req("https://x/?p=1"), |_b| None).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["items"], json!([1, 2]));
        assert_eq!(client.sent_count(), 1);
    }

    #[test]
    fn follows_next_until_none() {
        let client = RecordingClient::queue(vec![
            Ok(HttpResponse {
                status: 200,
                body: json!({ "items": [1, 2], "cursor": "c2" }),
            }),
            Ok(HttpResponse {
                status: 200,
                body: json!({ "items": [3, 4], "cursor": "c3" }),
            }),
            Ok(HttpResponse {
                status: 200,
                body: json!({ "items": [5], "cursor": null }),
            }),
        ]);
        let pages = paginate_all(&client, req("https://x/?p=1"), |body| {
            body.get("cursor")
                .and_then(Value::as_str)
                .map(|c| req(&format!("https://x/?cursor={c}")))
        })
        .unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0]["items"], json!([1, 2]));
        assert_eq!(pages[2]["items"], json!([5]));
        // Vérifie l'URL des requêtes suivantes
        let urls: Vec<String> = client.all_sent().into_iter().map(|r| r.url).collect();
        assert_eq!(urls[0], "https://x/?p=1");
        assert_eq!(urls[1], "https://x/?cursor=c2");
        assert_eq!(urls[2], "https://x/?cursor=c3");
    }

    #[test]
    fn surfaces_http_errors() {
        let client = RecordingClient::with_status(500, json!({ "boom": true }));
        let err = paginate_all(&client, req("https://x/"), |_| None).unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("http 500"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_transport_errors() {
        let client = RecordingClient::failing("dns");
        let err = paginate_all(&client, req("https://x/"), |_| None).unwrap_err();
        match err {
            Error::Invalid(m) => assert!(m.contains("dns"), "msg={m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
