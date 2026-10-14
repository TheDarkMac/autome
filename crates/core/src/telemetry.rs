//! Logs structurés de l'API et du worker (#176).
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

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

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
/// `writer` (stdout en production, tampon en test).
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match format {
        LogFormat::Json => Box::new(
            builder
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .finish(),
        ),
        LogFormat::Text => Box::new(builder.finish()),
    }
}

/// Identifiant de corrélation d'une requête : 128 bits aléatoires en hex.
pub fn new_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Installe le subscriber global (stdout) selon l'environnement et renvoie
/// le format retenu. À appeler une fois, au démarrage du binaire.
pub fn init(profile: Profile) -> Result<LogFormat> {
    let format = LogFormat::from_env(profile)?;
    tracing::subscriber::set_global_default(subscriber(format, env_filter(), std::io::stdout))
        .map_err(|e| Error::Invalid(format!("initialisation des logs : {e}")))?;
    Ok(format)
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
        let sub = subscriber(LogFormat::Json, EnvFilter::new("info"), buffer.clone());
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
}
