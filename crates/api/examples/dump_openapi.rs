//! Dump de la spec OpenAPI vers stdout — génération reproductible (F8.7.a).
//!
//! La spec est générée à la compilation par utoipa (cf. `openapi::ApiDoc`).
//! Ce petit binaire la sérialise en JSON indenté pour alimenter le client
//! typé du frontend, **sans** démarrer le serveur ni la base de données.
//!
//! Usage :
//!
//! ```sh
//! cargo run -p autome-api --example dump_openapi > frontend/openapi.json
//! ```

use autome_api::openapi::ApiDoc;
use utoipa::OpenApi;

fn main() {
    let doc = ApiDoc::openapi();
    let json = doc
        .to_pretty_json()
        .expect("sérialisation de la spec OpenAPI en JSON");
    println!("{json}");
}
