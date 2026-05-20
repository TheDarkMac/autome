//! SDK interne pour créer un connecteur (F4.5).
//!
//! Factorise les patterns qui s'étaient accumulés entre F4.1 → F4.4 :
//!
//! | Module             | Rôle                                                                  |
//! |--------------------|-----------------------------------------------------------------------|
//! | [`auth`]           | trait [`Credentials`](auth::Credentials) (en-têtes d'auth)            |
//! | [`http`]           | helpers URL (`percent_encode`, `to_query`)                            |
//! | [`api`]            | [`ResponseInterpreter`](api::ResponseInterpreter) + `interpret`       |
//! | [`retry`]          | [`RetryingClient`](retry::RetryingClient) + [`Clock`](retry::Clock)   |
//! | [`pagination`]     | [`paginate_all`](pagination::paginate_all) via closure « next »       |
//! | [`schema`]         | [`ActionSchema`](schema::ActionSchema), [`ParamSpec`](schema::ParamSpec) |
//! | [`connector`]      | trait [`Connector`](connector::Connector) (provider + actions)        |
//! | [`testing`]        | [`RecordingClient`](testing::RecordingClient) (transport factice)     |
//!
//! Voir [`crate::template`] pour un connecteur exemple complet qui n'utilise
//! que ce SDK (~50 lignes).

pub mod api;
pub mod auth;
pub mod connector;
pub mod http;
pub mod pagination;
pub mod retry;
pub mod schema;
pub mod testing;

pub use api::{interpret, ResponseInterpreter};
pub use auth::{headers_for, Credentials};
pub use connector::Connector;
pub use http::{percent_encode, to_query};
pub use pagination::paginate_all;
pub use retry::{Clock, RetryPolicy, RetryingClient, StdClock};
pub use schema::{ActionSchema, ParamKind, ParamSpec};
pub use testing::RecordingClient;
