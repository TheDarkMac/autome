//! Logs structurés (#176) et export des traces OpenTelemetry (#249) de l'API
//! et du worker.
//!
//! Les crates émettent via la façade [`tracing`] (niveaux, champs
//! structurés, spans) ; chaque binaire installe le subscriber au démarrage
//! avec [`init`] :
//!
//! - **format** : `AUTOME_LOG_FORMAT` = `json` | `text`. Défaut : `json` en
//!   profil `prod` (une ligne JSON par événement, pour Loki/CloudWatch/ELK),
//!   `text` en `dev` (lisible en terminal) ;
//! - **niveau** : `RUST_LOG` (syntaxe `EnvFilter`, ex.
//!   `info,autome_worker=debug`), défaut `info`.
//!
//! En JSON, les champs de l'événement sont à plat (`run_id`, `status`…) et
//! ceux du span courant sous `span` (`request_id` côté API, `run_id` /
//! `workflow_id` côté worker) : chaque ligne se rattache à sa requête ou à
//! son run.
//!
//! **Traces** (#249). Quand un endpoint OTLP est configuré ([`TraceExport`]),
//! une couche `tracing-opentelemetry` s'ajoute à celle des logs : les spans
//! (`http_server`, `run_dispatch`, `run`, `node`, `http_client`) partent par OTLP
//! HTTP/protobuf vers un collecteur (Jaeger, Tempo, OpenTelemetry
//! Collector…), par lots, depuis un thread dédié. Le contexte passe d'un
//! processus à l'autre au format W3C `traceparent` ([`inject_context`],
//! [`set_parent`]) — l'API l'écrit dans les en-têtes du message de run, le
//! worker le relit — et [`record_trace_id`] ajoute `trace_id` aux logs du
//! span. Sans endpoint, rien n'est installé : ni thread, ni en-tête, ni
//! champ supplémentaire.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, Tracer};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::{Error, Profile, Result};

/// Format de sortie des logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Une ligne JSON par événement (production).
    Json,
    /// Texte lisible (développement).
    Text,
}

impl LogFormat {
    /// Lit `AUTOME_LOG_FORMAT` (défaut selon `profile`).
    pub fn from_env(profile: Profile) -> Result<Self> {
        Self::parse(std::env::var("AUTOME_LOG_FORMAT").ok().as_deref(), profile)
    }

    /// Parse une valeur optionnelle ; `None`/`""` ⇒ `Json` en `prod`, `Text`
    /// en `dev`. Toute autre valeur que `json`/`text` est rejetée.
    pub fn parse(value: Option<&str>, profile: Profile) -> Result<Self> {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" => Ok(match profile {
                Profile::Prod => Self::Json,
                Profile::Dev => Self::Text,
            }),
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            other => Err(Error::Invalid(format!(
                "AUTOME_LOG_FORMAT invalide : `{other}` (attendu : json | text)"
            ))),
        }
    }
}

/// Filtre de niveau : `RUST_LOG` s'il est valide, sinon `info`.
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Construit le subscriber `format` filtré par `filter`, écrivant dans
/// `writer` (stdout en production, tampon en test). Avec un `tracer`, les
/// spans sont aussi exportés en OpenTelemetry — sauf [`LOG_ONLY_SPANS`] ;
/// [`TRACE_ONLY_SPANS`] n'apparaissent pas dans les logs.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
    tracer: Option<Tracer>,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let traces = tracer.map(|tracer| {
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            // Fichier, ligne et thread n'aident pas à lire une trace et
            // alourdissent chaque span.
            .with_location(false)
            .with_threads(false)
            .with_filter(filter_fn(|meta| {
                !(meta.is_span() && LOG_ONLY_SPANS.contains(&meta.name()))
            }))
    });
    let registry = Registry::default().with(filter).with(traces);
    // Les spans de trace seule sont invisibles des logs : une ligne émise
    // dans un nœud garde le contexte du run (`run_id`) plutôt que celui du
    // nœud.
    let logged = filter_fn(|meta| !(meta.is_span() && TRACE_ONLY_SPANS.contains(&meta.name())));
    let logs = tracing_subscriber::fmt::layer().with_writer(writer);
    match format {
        LogFormat::Json => Box::new(
            registry.with(
                logs.json()
                    .flatten_event(true)
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_filter(logged),
            ),
        ),
        LogFormat::Text => Box::new(registry.with(logs.with_filter(logged))),
    }
}

/// Identifiant de corrélation d'une requête : 128 bits aléatoires en hex.
pub fn new_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Span d'une requête HTTP reçue par l'API (logs seuls) : enveloppe toutes
/// les requêtes, 404 compris, avant le routage — son nom ne peut donc pas
/// porter la route. La trace en reçoit un enfant [`HTTP_SERVER_SPAN`].
pub const REQUEST_SPAN: &str = "http_request";

/// Span serveur d'une requête routée (trace seule), nommé d'après la route
/// (`POST /workflows/:id/runs`) ; ni les sondes ni `/metrics` n'en ont.
pub const HTTP_SERVER_SPAN: &str = "http_server";

/// Span d'un nœud exécuté par le moteur (trace seule).
pub const NODE_SPAN: &str = "node";

/// Span d'un appel HTTP sortant des connecteurs (trace seule).
pub const HTTP_CLIENT_SPAN: &str = "http_client";

/// Span de la mise en file d'un run (trace seule) : parent du span `run` du
/// worker, par le `traceparent` du message.
pub const DISPATCH_SPAN: &str = "run_dispatch";

/// Spans journalisés mais jamais exportés en trace.
pub const LOG_ONLY_SPANS: [&str; 1] = [REQUEST_SPAN];

/// Spans exportés en trace mais absents des logs : imbriqués dans `run` ou
/// `http_request`, ils masqueraient en JSON le contexte de la ligne (seul le
/// span courant y figure).
pub const TRACE_ONLY_SPANS: [&str; 4] =
    [HTTP_SERVER_SPAN, NODE_SPAN, HTTP_CLIENT_SPAN, DISPATCH_SPAN];

/// Délai accordé au vidage des spans en attente à l'arrêt.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Télémétrie installée par [`init`] : format des logs et, si l'export est
/// actif, le fournisseur de traces. **À garder jusqu'à la fin de `main`** :
/// le relâcher (ou appeler [`Telemetry::shutdown`], indispensable avant un
/// `process::exit`) vide les spans en attente vers le collecteur.
#[must_use = "relâcher la télémétrie arrête l'export des traces"]
pub struct Telemetry {
    format: LogFormat,
    provider: Mutex<Option<SdkTracerProvider>>,
}

impl Telemetry {
    /// Format des logs retenu.
    pub fn log_format(&self) -> LogFormat {
        self.format
    }

    /// `true` si les traces sont exportées.
    pub fn exports_traces(&self) -> bool {
        self.provider.lock().map(|p| p.is_some()).unwrap_or(false)
    }

    /// Vide les spans en attente puis arrête l'export (≤ 5 s, bloquant).
    /// Idempotent ; sans effet si l'export est inactif.
    pub fn shutdown(&self) {
        let provider = self.provider.lock().ok().and_then(|mut p| p.take());
        if let Some(provider) = provider {
            match provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
                Ok(()) => tracing::debug!("traces en attente exportées"),
                Err(e) => tracing::warn!(error = %e, "traces en attente non exportées à l'arrêt"),
            }
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Installe le subscriber global (stdout) selon l'environnement : logs, et
/// export des traces OTLP si [`TraceExport::from_env`] l'active.
/// `service_name` est le `service.name` par défaut (`OTEL_SERVICE_NAME` ou
/// `OTEL_RESOURCE_ATTRIBUTES` le remplacent). À appeler une fois, au
/// démarrage du binaire.
pub fn init(profile: Profile, service_name: &str) -> Result<Telemetry> {
    let format = LogFormat::from_env(profile)?;
    let provider = match TraceExport::from_env()? {
        TraceExport::Disabled => None,
        TraceExport::Otlp(settings) => Some(tracer_provider(&settings, service_name)?),
    };
    let tracer = provider.as_ref().map(|p| p.tracer("autome"));
    tracing::subscriber::set_global_default(subscriber(
        format,
        env_filter(),
        std::io::stdout,
        tracer,
    ))
    .map_err(|e| Error::Invalid(format!("initialisation des logs : {e}")))?;
    Ok(Telemetry {
        format,
        provider: Mutex::new(provider),
    })
}

// ─────────────────────────── Configuration OTLP ───────────────────────────

/// Export des traces, lu des variables standard OpenTelemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceExport {
    /// Aucun export (défaut).
    Disabled,
    /// Export OTLP HTTP/protobuf.
    Otlp(OtlpSettings),
}

/// Destination de l'export OTLP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpSettings {
    /// URL complète de réception des traces (chemin `/v1/traces` compris).
    pub endpoint: String,
    /// Délai maximal d'un envoi.
    pub timeout: Duration,
}

/// Endpoint par défaut quand l'export est demandé sans adresse.
const DEFAULT_TRACES_ENDPOINT: &str = "http://localhost:4318/v1/traces";
/// Délai d'envoi par défaut (spécification OTLP).
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

impl TraceExport {
    /// Lit l'environnement du processus (cf. [`TraceExport::parse`]).
    pub fn from_env() -> Result<Self> {
        Self::parse(|name| std::env::var(name).ok())
    }

    /// Décide de l'export à partir de `var` (lecture d'une variable) :
    ///
    /// - `OTEL_SDK_DISABLED=true` ou `OTEL_TRACES_EXPORTER=none` ⇒ aucun
    ///   export ;
    /// - sinon, export si `OTEL_TRACES_EXPORTER=otlp` ou si un endpoint est
    ///   donné : `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (pris tel quel) ou
    ///   `OTEL_EXPORTER_OTLP_ENDPOINT` (base, `/v1/traces` ajouté) ; défaut
    ///   `http://localhost:4318/v1/traces` ;
    /// - protocole (`OTEL_EXPORTER_OTLP[_TRACES]_PROTOCOL`) : `http/protobuf`
    ///   seulement — `grpc` et `http/json` sont refusés plutôt qu'ignorés ;
    /// - délai (`OTEL_EXPORTER_OTLP[_TRACES]_TIMEOUT`, en ms), 10 s par défaut.
    ///
    /// Les en-têtes (`OTEL_EXPORTER_OTLP[_TRACES]_HEADERS`), la ressource
    /// (`OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`), l'échantillonnage
    /// (`OTEL_TRACES_SAMPLER[_ARG]`) et les lots (`OTEL_BSP_*`) sont lus par
    /// le SDK lui-même.
    pub fn parse(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |name: &str| {
            var(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if get("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return Ok(Self::Disabled);
        }
        let requested = match get("OTEL_TRACES_EXPORTER").map(|v| v.to_ascii_lowercase()) {
            None => false,
            Some(v) if v == "none" => return Ok(Self::Disabled),
            Some(v) if v == "otlp" => true,
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "OTEL_TRACES_EXPORTER invalide : `{other}` (attendu : otlp | none)"
                )))
            }
        };
        let endpoint = match (
            get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"),
            get("OTEL_EXPORTER_OTLP_ENDPOINT"),
        ) {
            (Some(traces), _) => traces,
            (None, Some(base)) => format!("{}/v1/traces", base.trim_end_matches('/')),
            (None, None) if requested => DEFAULT_TRACES_ENDPOINT.to_string(),
            (None, None) => return Ok(Self::Disabled),
        };
        match reqwest::Url::parse(&endpoint) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => {}
            _ => {
                return Err(Error::Invalid(format!(
                    "endpoint OTLP invalide : `{endpoint}` (URL http:// ou https:// attendue)"
                )))
            }
        }
        let protocol = get("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .or_else(|| get("OTEL_EXPORTER_OTLP_PROTOCOL"))
            .map(|v| v.to_ascii_lowercase());
        match protocol.as_deref() {
            None | Some("http/protobuf") => {}
            Some(other) => {
                return Err(Error::Invalid(format!(
                    "protocole OTLP non pris en charge : `{other}` (seul http/protobuf l'est ; \
                     le port OTLP/HTTP d'un collecteur est en général 4318)"
                )))
            }
        }
        let timeout = match get("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT")
            .or_else(|| get("OTEL_EXPORTER_OTLP_TIMEOUT"))
        {
            None => DEFAULT_EXPORT_TIMEOUT,
            Some(ms) => match ms.parse::<u64>() {
                Ok(ms) if ms > 0 => Duration::from_millis(ms),
                _ => {
                    return Err(Error::Invalid(format!(
                        "OTEL_EXPORTER_OTLP_TIMEOUT invalide : `{ms}` (millisecondes > 0)"
                    )))
                }
            },
        };
        Ok(Self::Otlp(OtlpSettings { endpoint, timeout }))
    }
}

/// Fournisseur de traces : exporteur OTLP HTTP/protobuf par lots, ressource
/// lue de l'environnement, `service.name` par défaut `service_name`.
pub fn tracer_provider(settings: &OtlpSettings, service_name: &str) -> Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(OtlpHttpClient::new(settings.timeout)?)
        .with_endpoint(settings.endpoint.clone())
        .with_timeout(settings.timeout)
        .build()
        .map_err(|e| Error::Invalid(format!("exporteur OTLP : {e}")))?;
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource(service_name))
        .build())
}

/// Ressource des traces : détecteurs du SDK (`OTEL_SERVICE_NAME`,
/// `OTEL_RESOURCE_ATTRIBUTES`), complétés par `service_name` et la version
/// du binaire quand l'opérateur ne les donne pas.
fn resource(service_name: &str) -> Resource {
    let detected = Resource::builder().build();
    let mut builder = Resource::builder();
    let name = opentelemetry::Key::from_static_str("service.name");
    if detected
        .get(&name)
        .is_none_or(|v| v.as_str().starts_with("unknown_service"))
    {
        builder = builder.with_service_name(service_name.to_string());
    }
    let version = opentelemetry::Key::from_static_str("service.version");
    if detected.get(&version).is_none() {
        builder = builder.with_attribute(KeyValue::new(version, env!("CARGO_PKG_VERSION")));
    }
    builder.build()
}

/// Taille maximale lue d'une réponse du collecteur.
const MAX_COLLECTOR_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Transport de l'exporteur : le `reqwest` bloquant du workspace (celui de
/// l'exporteur tirerait une seconde version de reqwest). Appelé depuis le
/// thread du processeur par lots, hors runtime tokio. Un échec d'envoi est
/// journalisé une fois, puis son rétablissement.
#[derive(Debug)]
struct OtlpHttpClient {
    inner: reqwest::blocking::Client,
    failing: AtomicBool,
}

impl OtlpHttpClient {
    fn new(timeout: Duration) -> Result<Self> {
        // Le client bloquant démarre son propre runtime : le construire dans
        // celui du binaire paniquerait.
        let inner = std::thread::spawn(move || {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
        })
        .join()
        .map_err(|_| Error::Invalid("client HTTP de l'exporteur OTLP : panique".into()))?
        .map_err(|e| Error::Invalid(format!("client HTTP de l'exporteur OTLP : {e}")))?;
        Ok(Self {
            inner,
            failing: AtomicBool::new(false),
        })
    }

    fn send(
        &self,
        request: opentelemetry_http::Request<opentelemetry_http::Bytes>,
    ) -> std::result::Result<
        opentelemetry_http::Response<opentelemetry_http::Bytes>,
        opentelemetry_http::HttpError,
    > {
        use std::io::Read;
        let (parts, body) = request.into_parts();
        let mut response = self
            .inner
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body.to_vec())
            .send()?;
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let mut bytes = Vec::new();
        response
            .take(MAX_COLLECTOR_RESPONSE_BYTES)
            .read_to_end(&mut bytes)?;
        let mut out = opentelemetry_http::Response::builder()
            .status(status)
            .body(opentelemetry_http::Bytes::from(bytes))?;
        *out.headers_mut() = headers;
        Ok(out)
    }

    /// Journalise le passage en échec ou le retour à la normale.
    fn note(&self, failure: Option<String>) {
        match failure {
            Some(error) if !self.failing.swap(true, Ordering::Relaxed) => {
                tracing::warn!(
                    error,
                    "export des traces impossible : spans perdus jusqu'au rétablissement"
                )
            }
            None if self.failing.swap(false, Ordering::Relaxed) => {
                tracing::info!("export des traces rétabli")
            }
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for OtlpHttpClient {
    async fn send_bytes(
        &self,
        request: opentelemetry_http::Request<opentelemetry_http::Bytes>,
    ) -> std::result::Result<
        opentelemetry_http::Response<opentelemetry_http::Bytes>,
        opentelemetry_http::HttpError,
    > {
        let result = self.send(request);
        self.note(match &result {
            Ok(r) if r.status().is_success() => None,
            Ok(r) => Some(format!("collecteur : HTTP {}", r.status().as_u16())),
            Err(e) => Some(e.to_string()),
        });
        result
    }
}

// ─────────────────────────── Propagation W3C ───────────────────────────

/// En-têtes de contexte W3C (`traceparent`, `tracestate`).
pub const TRACE_CONTEXT_HEADERS: [&str; 2] = ["traceparent", "tracestate"];

struct MapInjector<'a>(&'a mut BTreeMap<String, String>);

impl Injector for MapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

struct MapExtractor<'a>(&'a BTreeMap<String, String>);

impl Extractor for MapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

/// Écrit le contexte du span courant (`traceparent`, `tracestate`) dans
/// `headers`. Sans export actif, le contexte est vide et rien n'est écrit.
pub fn inject_context(headers: &mut BTreeMap<String, String>) {
    let cx = tracing::Span::current().context();
    if cx.span().span_context().is_valid() {
        TraceContextPropagator::new().inject_context(&cx, &mut MapInjector(headers));
    }
}

/// Rattache `span` au contexte W3C lu dans `headers` (span distant parent).
/// À appeler juste après la création du span, avant d'y entrer. Sans
/// contexte valide (absent, illisible) ou sans export actif, ne fait rien.
pub fn set_parent(span: &tracing::Span, headers: &BTreeMap<String, String>) {
    let cx = TraceContextPropagator::new().extract(&MapExtractor(headers));
    if cx.span().span_context().is_valid() {
        // Échoue seulement sans couche OpenTelemetry : rien à rattacher.
        let _ = span.set_parent(cx);
    }
}

/// Identifiant (hex) de la trace de `span`, `None` sans export actif. À
/// poser dans un champ `trace_id` (déclaré `tracing::field::Empty`) d'un
/// span journalisé : ses logs renvoient alors à la trace.
pub fn trace_id(span: &tracing::Span) -> Option<String> {
    let cx = span.context();
    let span_context = cx.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// Renseigne le champ `trace_id` de `span` avec l'id de sa propre trace
/// (cf. [`trace_id`]). Sans export actif, le champ reste vide, donc absent
/// des logs.
pub fn record_trace_id(span: &tracing::Span) {
    if let Some(id) = trace_id(span) {
        span.record("trace_id", id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn request_ids_are_random_128_bit_hex() {
        let (a, b) = (new_request_id(), new_request_id());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn default_format_depends_on_profile() {
        assert_eq!(
            LogFormat::parse(None, Profile::Prod).unwrap(),
            LogFormat::Json
        );
        assert_eq!(
            LogFormat::parse(Some(""), Profile::Dev).unwrap(),
            LogFormat::Text
        );
    }

    #[test]
    fn explicit_format_overrides_profile_and_unknown_is_rejected() {
        assert_eq!(
            LogFormat::parse(Some(" JSON "), Profile::Dev).unwrap(),
            LogFormat::Json
        );
        assert_eq!(
            LogFormat::parse(Some("text"), Profile::Prod).unwrap(),
            LogFormat::Text
        );
        let err = LogFormat::parse(Some("logfmt"), Profile::Prod).unwrap_err();
        assert!(err.to_string().contains("AUTOME_LOG_FORMAT"), "{err}");
    }

    /// Tampon partagé servant de sortie de log en test.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn lines(buffer: &Buffer) -> Vec<serde_json::Value> {
        String::from_utf8(buffer.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("une ligne JSON par événement"))
            .collect()
    }

    #[test]
    fn json_lines_carry_level_fields_and_span_context() {
        let buffer = Buffer::default();
        let sub = subscriber(
            LogFormat::Json,
            EnvFilter::new("info"),
            buffer.clone(),
            None,
        );
        tracing::subscriber::with_default(sub, || {
            let span = tracing::info_span!("run", run_id = "run-42", workflow_id = "wf-1");
            let _guard = span.enter();
            tracing::warn!(status = "failed", "run en échec");
            tracing::debug!("filtré par le niveau info");
        });

        let lines = lines(&buffer);
        assert_eq!(lines.len(), 1, "debug filtré : {lines:?}");
        let line = &lines[0];
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["message"], "run en échec");
        assert_eq!(line["status"], "failed", "champ à plat");
        assert_eq!(line["span"]["run_id"], "run-42");
        assert_eq!(line["span"]["workflow_id"], "wf-1");
        assert!(line["timestamp"].is_string());
    }

    // ─────────────────────────── Traces (#249) ───────────────────────────

    use opentelemetry_sdk::trace::InMemorySpanExporter;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn otlp(pairs: &[(&str, &str)]) -> OtlpSettings {
        match TraceExport::parse(env(pairs)).unwrap() {
            TraceExport::Otlp(settings) => settings,
            TraceExport::Disabled => panic!("export attendu pour {pairs:?}"),
        }
    }

    #[test]
    fn traces_are_not_exported_without_endpoint_or_when_disabled() {
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        for pairs in [
            vec![],
            vec![("OTEL_EXPORTER_OTLP_ENDPOINT", "  ")],
            vec![endpoint, ("OTEL_SDK_DISABLED", "TRUE")],
            vec![endpoint, ("OTEL_TRACES_EXPORTER", "none")],
        ] {
            assert_eq!(
                TraceExport::parse(env(&pairs)).unwrap(),
                TraceExport::Disabled,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn base_endpoint_gets_the_traces_path_and_signal_endpoint_is_verbatim() {
        assert_eq!(
            otlp(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/")]).endpoint,
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            otlp(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://ignored:4318"),
                (
                    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                    "https://otlp.example/custom"
                ),
            ])
            .endpoint,
            "https://otlp.example/custom"
        );
        let requested = otlp(&[("OTEL_TRACES_EXPORTER", "otlp")]);
        assert_eq!(requested.endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(requested.timeout, Duration::from_secs(10));
    }

    #[test]
    fn timeout_is_read_in_milliseconds_signal_first() {
        let settings = otlp(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "5000"),
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "1500"),
        ]);
        assert_eq!(settings.timeout, Duration::from_millis(1500));
    }

    #[test]
    fn unsupported_or_invalid_settings_are_rejected_at_startup() {
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        for (pairs, expected) in [
            (
                vec![endpoint, ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")],
                "grpc",
            ),
            (
                vec![
                    endpoint,
                    ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json"),
                ],
                "http/json",
            ),
            (
                vec![("OTEL_TRACES_EXPORTER", "jaeger")],
                "OTEL_TRACES_EXPORTER",
            ),
            (
                vec![("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4318")],
                "endpoint OTLP invalide",
            ),
            (
                vec![endpoint, ("OTEL_EXPORTER_OTLP_TIMEOUT", "10s")],
                "TIMEOUT",
            ),
            (
                vec![endpoint, ("OTEL_EXPORTER_OTLP_TIMEOUT", "0")],
                "TIMEOUT",
            ),
        ] {
            let err = TraceExport::parse(env(&pairs)).unwrap_err().to_string();
            assert!(err.contains(expected), "{pairs:?} : {err}");
        }
    }

    /// Subscriber JSON exportant ses spans dans un exporteur en mémoire. Le
    /// fournisseur renvoyé doit vivre jusqu'à la lecture des spans : le
    /// tracer ne le retient pas.
    fn traced(
        buffer: &Buffer,
    ) -> (
        Box<dyn tracing::Subscriber + Send + Sync>,
        InMemorySpanExporter,
        SdkTracerProvider,
    ) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let sub = subscriber(
            LogFormat::Json,
            EnvFilter::new("info"),
            buffer.clone(),
            Some(provider.tracer("test")),
        );
        (sub, exporter, provider)
    }

    fn exported(exporter: &InMemorySpanExporter, name: &str) -> opentelemetry_sdk::trace::SpanData {
        exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("span `{name}` non exporté"))
    }

    #[test]
    fn context_travels_through_message_headers_and_tags_the_logs() {
        let buffer = Buffer::default();
        let (sub, exporter, _provider) = traced(&buffer);
        let mut headers = BTreeMap::new();
        tracing::subscriber::with_default(sub, || {
            // Producteur : l'API met le run en file.
            let dispatch = tracing::info_span!(DISPATCH_SPAN, otel.name = "run dispatch");
            dispatch.in_scope(|| inject_context(&mut headers));
            drop(dispatch);

            // Consommateur : le worker reprend le contexte du message.
            let run =
                tracing::info_span!("run", run_id = "run-1", trace_id = tracing::field::Empty);
            set_parent(&run, &headers);
            record_trace_id(&run);
            run.in_scope(|| tracing::info!("run exécuté"));
        });

        let traceparent = headers.get("traceparent").expect("traceparent injecté");
        let dispatch = exported(&exporter, "run dispatch");
        let run = exported(&exporter, "run");
        let trace_id = dispatch.span_context.trace_id().to_string();
        assert!(traceparent.contains(&trace_id), "{traceparent}");
        assert_eq!(
            run.span_context.trace_id(),
            dispatch.span_context.trace_id()
        );
        assert_eq!(run.parent_span_id, dispatch.span_context.span_id());

        let lines = lines(&buffer);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["span"]["run_id"], "run-1");
        assert_eq!(lines[0]["span"]["trace_id"], trace_id.as_str());
    }

    #[test]
    fn log_only_and_trace_only_spans_are_kept_apart() {
        let buffer = Buffer::default();
        let (sub, exporter, _provider) = traced(&buffer);
        tracing::subscriber::with_default(sub, || {
            let request = tracing::info_span!(REQUEST_SPAN, request_id = "req-1");
            let _request = request.enter();
            let run = tracing::info_span!("run", run_id = "run-1");
            let _run = run.enter();
            let node = tracing::info_span!(NODE_SPAN, node_id = "n1");
            node.in_scope(|| tracing::info!("dans le nœud"));
        });

        // Logs : la ligne émise dans le nœud garde le contexte du run.
        let lines = lines(&buffer);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["span"]["run_id"], "run-1");
        assert!(lines[0]["span"]["node_id"].is_null(), "{:?}", lines[0]);

        // Trace : le nœud est l'enfant du run ; la requête n'est pas exportée.
        let names: Vec<String> = exporter
            .get_finished_spans()
            .unwrap()
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert!(!names.contains(&REQUEST_SPAN.to_string()), "{names:?}");
        let (node, run) = (exported(&exporter, NODE_SPAN), exported(&exporter, "run"));
        assert_eq!(node.parent_span_id, run.span_context.span_id());
        assert_eq!(node.events.len(), 1, "l'événement est attaché au nœud");
    }

    #[test]
    fn nothing_is_propagated_without_export() {
        let buffer = Buffer::default();
        let sub = subscriber(
            LogFormat::Json,
            EnvFilter::new("info"),
            buffer.clone(),
            None,
        );
        let mut headers = BTreeMap::new();
        tracing::subscriber::with_default(sub, || {
            let span = tracing::info_span!("run", trace_id = tracing::field::Empty);
            span.in_scope(|| inject_context(&mut headers));
            assert_eq!(trace_id(&span), None);
            record_trace_id(&span);
            span.in_scope(|| tracing::info!("sans export"));
        });
        assert!(headers.is_empty(), "{headers:?}");
        assert!(lines(&buffer)[0]["span"]["trace_id"].is_null());
    }

    #[test]
    fn unreadable_incoming_context_starts_a_new_trace() {
        let buffer = Buffer::default();
        let (sub, exporter, _provider) = traced(&buffer);
        let headers =
            BTreeMap::from([("traceparent".to_string(), "not-a-traceparent".to_string())]);
        tracing::subscriber::with_default(sub, || {
            let run = tracing::info_span!("run");
            set_parent(&run, &headers);
            drop(run.entered());
        });
        let run = exported(&exporter, "run");
        assert!(run.span_context.is_valid());
        assert_eq!(run.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    }

    #[test]
    fn export_failures_are_logged_once_then_recovery() {
        let buffer = Buffer::default();
        let sub = subscriber(
            LogFormat::Json,
            EnvFilter::new("info"),
            buffer.clone(),
            None,
        );
        let client = OtlpHttpClient::new(Duration::from_secs(1)).unwrap();
        tracing::subscriber::with_default(sub, || {
            client.note(None);
            client.note(Some("connexion refusée".into()));
            client.note(Some("connexion refusée".into()));
            client.note(None);
            client.note(None);
        });
        let lines = lines(&buffer);
        let levels: Vec<&str> = lines.iter().map(|l| l["level"].as_str().unwrap()).collect();
        assert_eq!(levels, ["WARN", "INFO"], "{lines:?}");
        assert_eq!(lines[0]["error"], "connexion refusée");
    }

    /// Collecteur OTLP minimal : répond 200 et renvoie la première ligne et
    /// les en-têtes de chaque requête reçue.
    fn collector() -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 8192];
                // En-têtes, puis le corps annoncé par `content-length`.
                let head_len = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break raw.len();
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&raw[..head_len]).to_lowercase();
                let body_len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while raw.len() < head_len + body_len {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                }
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
                let _ = tx.send(head);
            }
        });
        (port, rx)
    }

    #[test]
    fn spans_are_posted_as_otlp_protobuf_to_the_collector() {
        let (port, received) = collector();
        let provider = tracer_provider(
            &OtlpSettings {
                endpoint: format!("http://127.0.0.1:{port}/v1/traces"),
                timeout: Duration::from_secs(5),
            },
            "autome-test",
        )
        .unwrap();
        let sub = subscriber(
            LogFormat::Json,
            EnvFilter::new("info"),
            Buffer::default(),
            Some(provider.tracer("test")),
        );
        tracing::subscriber::with_default(sub, || {
            drop(tracing::info_span!("run", run_id = "run-1").entered());
        });
        provider.force_flush().unwrap();
        let head = received
            .recv_timeout(Duration::from_secs(5))
            .expect("lot reçu par le collecteur");
        assert!(head.starts_with("post /v1/traces "), "{head}");
        assert!(
            head.contains("content-type: application/x-protobuf"),
            "{head}"
        );
        provider.shutdown().unwrap();
    }

    #[test]
    fn resource_names_the_service_unless_the_operator_does() {
        if std::env::var_os("OTEL_SERVICE_NAME").is_some()
            || std::env::var_os("OTEL_RESOURCE_ATTRIBUTES").is_some()
        {
            return; // environnement de l'opérateur : le défaut ne s'applique pas
        }
        let resource = resource("autome-test");
        let get = |key: &'static str| {
            resource
                .get(&opentelemetry::Key::from_static_str(key))
                .map(|v| v.as_str().to_string())
        };
        assert_eq!(get("service.name").as_deref(), Some("autome-test"));
        assert_eq!(
            get("service.version").as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }
}
