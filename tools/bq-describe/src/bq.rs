//! Shared BigQuery access layer for the `bq-*` tools.
//!
//! This file is copied verbatim into every `bq-*` crate. Each tool is its own
//! standalone cargo package (an empty `[workspace]`, no path dependencies are
//! allowed), so there is no crate to factor it into. Keeping it a single file
//! with no tool-specific code means drift between copies is a plain `diff`.
//!
//! Three things live here, and they are the three things every tool needs:
//!
//! 1. **Auth** — resolving a bearer token from whatever the operator supplied.
//! 2. **REST** — one place that makes an HTTP call to bigquery.googleapis.com.
//! 3. **Decoding** — turning BigQuery's `{"f":[{"v":...}]}` wire format into
//!    ordinary JSON, and rendering it compactly enough to fit an answer.
//!
//! ## Why decoding is the interesting part
//!
//! BigQuery does not return rows as JSON objects. It returns positional
//! `f`/`v` pairs that only mean something against the accompanying schema, with
//! every scalar as a *string* — including integers, floats and booleans. A
//! caller handed that raw has to do the join itself, and an agent asked to draw
//! a conclusion from it will spend its context re-deriving which column is
//! which. So [`decode_rows`] does the join once and coerces to real JSON types,
//! and [`render_rows`] prints the result as a table, because a table costs a
//! fraction of the tokens that `[{"col":1},{"col":2}]` does.

#![allow(dead_code)]

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::thetis::grip::sys;
use crate::thetis::grip::types::LogLevel;

pub const API_BASE: &str = "https://bigquery.googleapis.com/bigquery/v2";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const JWT_AUDIENCE: &str = "https://bigquery.googleapis.com/";

/// Bytes in a tebibyte, the unit BigQuery prices on-demand queries in.
pub const BYTES_PER_TIB: f64 = 1_099_511_627_776.0;
/// BigQuery bills a minimum of 10 MB per on-demand query.
pub const MIN_BILLED_BYTES: u64 = 10 * 1024 * 1024;
/// US on-demand list price per TiB scanned, in USD. Override with
/// `price_per_tib` in `[tools.bq]` for a different region or a negotiated rate.
pub const DEFAULT_PRICE_PER_TIB: f64 = 6.25;

const JWT_LIFETIME_SECS: u64 = 3600;
const JWT_BACKDATE_SECS: u64 = 30;
/// Refresh a cached access token this far before it actually expires, so a
/// long tool chain does not race the boundary.
const TOKEN_REFRESH_MARGIN_MS: u64 = 120_000;

// ---------------------------------------------------------------------------
// Configuration and auth
// ---------------------------------------------------------------------------

/// How we authenticate. Resolved once from config plus per-call arguments.
#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    /// A bearer token handed to us directly — from the `access_token`
    /// argument, or `token` in config. Cheapest path: no signing, no exchange.
    Token(String),
    /// A service-account key. Signed locally into a self-signed JWT, which
    /// BigQuery accepts as a bearer token with no call to Google's auth
    /// server at all. See docs/json-web-tokens.
    ServiceAccount {
        client_email: String,
        private_key: String,
    },
    /// A `gcloud auth application-default login` credential. This is a refresh
    /// token, not a signing key, so it *must* be exchanged over the network for
    /// an access token. Supported because it is what is actually on a
    /// developer's machine, whatever the docs recommend.
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        quota_project: Option<String>,
    },
    None,
}

/// Everything a `bq-*` tool needs to talk to BigQuery.
pub struct Bq {
    pub credential: Credential,
    pub project: Option<String>,
    pub location: Option<String>,
    pub price_per_tib: f64,
    pub max_bytes_billed: Option<u64>,
    pub timeout: std::time::Duration,
    /// Set when a `*_path` was configured but the host could not read it. Kept
    /// so the failure can be explained in terms of the path, not reported as
    /// "no credentials".
    unreadable: Option<String>,
}

impl Bq {
    /// Builds the client from this tool's `[tools.bq*]` block and the call's
    /// own arguments, with arguments winning.
    ///
    /// Group inheritance means `[tools.bq]` is merged before
    /// `[tools.bq-query]`, so one credential block serves every tool in the
    /// family.
    pub fn new(config: &Value, args: &Value) -> Self {
        let mut unreadable = None;

        // A per-call token beats configuration: it is how someone pipes in a
        // fresh `gcloud auth print-access-token` without storing a secret.
        let credential = if let Some(token) = str_arg(args, "access_token") {
            Credential::Token(token)
        } else {
            match service_account_json(config) {
                Ok(Some(cred)) => cred,
                Err(reason) => {
                    unreadable = Some(reason);
                    Credential::None
                }
                Ok(None) => match string_field(config, &["token", "access_token"]) {
                    Some(token) => Credential::Token(token),
                    None => Credential::None,
                },
            }
        };

        Bq {
            credential,
            project: str_arg(args, "project").or_else(|| {
                string_field(config, &["project", "project_id", "default_project"])
            }),
            location: str_arg(args, "location")
                .or_else(|| string_field(config, &["location", "default_location"])),
            price_per_tib: config
                .get("price_per_tib")
                .and_then(as_f64_loose)
                .unwrap_or(DEFAULT_PRICE_PER_TIB),
            max_bytes_billed: u64_arg(args, "maximum_bytes_billed")
                .or_else(|| config.get("max_bytes_billed").and_then(as_u64_loose)),
            timeout: std::time::Duration::from_secs(
                config
                    .get("http_timeout_secs")
                    .and_then(as_u64_loose)
                    .unwrap_or(60),
            ),
            unreadable,
        }
    }

    /// The project to bill and address. Required by every endpoint, so the
    /// error names all three ways to supply it rather than just failing.
    pub fn project(&self) -> Result<String, String> {
        self.project.clone().ok_or_else(|| {
            "no GCP project. Pass `project`, or set `project` in [tools.bq] in your local \
             config. `gcloud config get-value project` prints the one your CLI uses."
                .to_string()
        })
    }

    /// A bearer token for the Authorization header.
    pub fn token(&self) -> Result<String, String> {
        match &self.credential {
            Credential::Token(token) => Ok(token.clone()),
            Credential::ServiceAccount {
                client_email,
                private_key,
            } => self.self_signed_jwt(client_email, private_key),
            Credential::AuthorizedUser { .. } => self.exchange_refresh_token(),
            Credential::None => Err(self.no_credential_message()),
        }
    }

    fn no_credential_message(&self) -> String {
        if let Some(reason) = &self.unreadable {
            return format!(
                "could not read the configured BigQuery credentials: {reason}\n\n\
                 The path is resolved against the project root and must stay inside it."
            );
        }
        "no BigQuery credentials. Either:\n\
           * pass `access_token` — `gcloud auth print-access-token` prints one; or\n\
           * set `credentials_path` in [tools.bq] to a service-account JSON key, or to \
           ~/.config/gcloud/application_default_credentials.json after \
           `gcloud auth application-default login`."
            .to_string()
    }

    /// Signs a JWT that BigQuery accepts directly as a bearer token.
    ///
    /// The usual service-account dance signs an assertion and trades it at
    /// Google's token endpoint for an access token. That round trip is
    /// unnecessary: BigQuery accepts the self-signed JWT, so we skip it. One
    /// less network call, one less failure mode, and no token to cache.
    fn self_signed_jwt(&self, client_email: &str, private_key_pem: &str) -> Result<String, String> {
        let now = sys::now_ms() / 1000;
        let iat = now.saturating_sub(JWT_BACKDATE_SECS);
        let exp = iat + JWT_LIFETIME_SECS;

        let header = json!({ "alg": "RS256", "typ": "JWT" });
        // `aud` and `scope` are mutually exclusive here; `aud` is what the
        // BigQuery docs specify.
        let claims = json!({
            "iss": client_email,
            "sub": client_email,
            "aud": JWT_AUDIENCE,
            "iat": iat,
            "exp": exp,
        });

        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );

        let key = parse_private_key(private_key_pem)?;
        // PKCS#1 v1.5 is deterministic, so no RNG is needed — which matters,
        // because entropy in a wasm guest is not a given.
        let signature = SigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());

        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    /// Trades a `gcloud` ADC refresh token for an access token, cached in KV.
    ///
    /// The exchange costs a round trip and the result lasts an hour, so a
    /// six-call tool chain should pay for it once. The cache key is keyed by a
    /// digest of the refresh token, never the token itself.
    fn exchange_refresh_token(&self) -> Result<String, String> {
        let Credential::AuthorizedUser {
            client_id,
            client_secret,
            refresh_token,
            ..
        } = &self.credential
        else {
            return Err("not a refresh-token credential".to_string());
        };

        let mut hasher = Sha256::new();
        hasher.update(refresh_token.as_bytes());
        let digest = hasher.finalize();
        let cache_key = format!("bq/access-token/{}", hex16(&digest));

        if let Some(cached) = sys::kv_get("global", &cache_key) {
            if let Ok(entry) = serde_json::from_str::<Value>(&cached) {
                let expires = entry.get("expires_at_ms").and_then(as_u64_loose).unwrap_or(0);
                let token = entry.get("token").and_then(Value::as_str).unwrap_or("");
                if !token.is_empty() && expires > sys::now_ms() + TOKEN_REFRESH_MARGIN_MS {
                    return Ok(token.to_string());
                }
            }
        }

        let body = format!(
            "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
            urlencode(client_id),
            urlencode(client_secret),
            urlencode(refresh_token)
        );

        let response = waki::Client::new()
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.into_bytes())
            .connect_timeout(self.timeout)
            .send()
            .map_err(|e| format!("could not reach oauth2.googleapis.com: {e}"))?;

        let status = response.status_code();
        let bytes = response
            .body()
            .map_err(|e| format!("could not read the token response: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();

        if !(200..300).contains(&status) {
            // `invalid_grant` is the common one and the message Google returns
            // for it is unhelpful, so name the fix.
            if text.contains("invalid_grant") {
                return Err(format!(
                    "the stored gcloud credential was rejected ({status}). Run \
                     `gcloud auth application-default login` to refresh it, or pass a \
                     fresh `access_token`.\n\n{}",
                    clip(&text, 300)
                ));
            }
            return Err(format!(
                "token exchange failed ({status}): {}",
                clip(&text, 400)
            ));
        }

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| format!("the token response was not JSON: {e}"))?;
        let token = parsed
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| "the token response had no access_token".to_string())?
            .to_string();
        let lifetime = parsed
            .get("expires_in")
            .and_then(as_u64_loose)
            .unwrap_or(3600);

        sys::kv_put(
            "global",
            &cache_key,
            &json!({
                "token": token,
                "expires_at_ms": sys::now_ms() + lifetime * 1000,
            })
            .to_string(),
        );

        Ok(token)
    }

    /// An `x-goog-user-project` header, needed when a user credential has no
    /// project of its own to bill the API call to.
    fn quota_project(&self) -> Option<String> {
        match &self.credential {
            Credential::AuthorizedUser { quota_project, .. } => {
                quota_project.clone().or_else(|| self.project.clone())
            }
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    pub fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value, String> {
        self.request("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.request("POST", path, &[], Some(body))
    }

    /// The one place an HTTP request is actually made.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let token = self.token()?;
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{API_BASE}{path}")
        };
        sys::log(LogLevel::Debug, &format!("bigquery: {method} {path}"));

        let client = waki::Client::new();
        let mut request = match method {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "DELETE" => client.delete(&url),
            other => return Err(format!("unsupported HTTP method {other:?}")),
        };

        request = request
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .connect_timeout(self.timeout);

        if let Some(project) = self.quota_project() {
            request = request.header("x-goog-user-project", &project);
        }
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.body(body.to_string().into_bytes());
        }

        let response = request
            .send()
            .map_err(|e| format!("could not reach bigquery.googleapis.com: {e}"))?;

        let status = response.status_code();
        let bytes = response
            .body()
            .map_err(|e| format!("could not read BigQuery's response: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();

        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(json!({}));
            }
            return serde_json::from_str(&text)
                .map_err(|e| format!("BigQuery's response was not JSON: {e}: {}", clip(&text, 300)));
        }

        Err(explain_error(status, &text))
    }
}

/// Turns a BigQuery error payload into something worth reading.
///
/// The API nests the useful message two levels down and pairs it with a
/// `reason` code that determines what the caller should actually do, so both
/// are surfaced along with advice keyed off the code.
pub fn explain_error(status: u16, text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let error = parsed.get("error").cloned().unwrap_or(Value::Null);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text.trim())
        .to_string();
    let reason = error
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
        .and_then(|e| e.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let advice = match (status, reason.as_str()) {
        (_, "invalidQuery") => {
            "\n\nThe SQL was rejected before running, so nothing was billed. Note BigQuery \
             needs GoogleSQL here, not legacy SQL, and INFORMATION_SCHEMA views must be \
             qualified by dataset (`ds.INFORMATION_SCHEMA.TABLES`) or region \
             (`` `region-us`.INFORMATION_SCHEMA.JOBS ``)."
        }
        (_, "notFound") => {
            "\n\nCheck the project, dataset and table names. `bq-list` shows what exists, \
             and a table reference is `project.dataset.table`."
        }
        (_, "bytesBilledLimitExceeded") => {
            "\n\nThe query would scan more than the configured `maximum_bytes_billed` cap, \
             so it was refused and nothing was billed. Run `bq-query-cost` to see the real \
             figure, then raise the cap deliberately if it is worth it."
        }
        (_, "accessDenied") | (403, _) => {
            "\n\nThe credential authenticated but lacks permission. A read needs \
             `roles/bigquery.dataViewer` on the data plus `roles/bigquery.jobUser` on the \
             project to run queries at all."
        }
        (401, _) => {
            "\n\nThe credential was rejected. An access token lasts about an hour — \
             `gcloud auth print-access-token` mints a fresh one."
        }
        (_, "rateLimitExceeded") | (429, _) => {
            "\n\nRate limited. Wait and retry; consider fewer, larger queries."
        }
        (_, "quotaExceeded") => "\n\nA project quota is exhausted. Check quotas in the console.",
        _ => "",
    };

    let location = error
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
        .and_then(|e| e.get("location"))
        .and_then(Value::as_str)
        .map(|l| format!(" (at {l})"))
        .unwrap_or_default();

    format!("BigQuery returned {status}: {message}{location}{advice}")
}

// ---------------------------------------------------------------------------
// Credential parsing
// ---------------------------------------------------------------------------

/// Reads a credential out of config, accepting a service-account key, a gcloud
/// ADC file, or neither.
///
/// `Ok(None)` means nothing was configured. `Err` means something *was*
/// configured but could not be used — a distinction worth keeping, because the
/// two need different advice.
fn service_account_json(config: &Value) -> Result<Option<Credential>, String> {
    let raw = if let Some(inline) = string_field(
        config,
        &["credentials", "credentials_json", "service_account_json", "key_json"],
    ) {
        inline
    } else if let Some(contents) = string_field(
        config,
        &["credentials_contents", "key_contents", "service_account_contents"],
    ) {
        // A tool has no filesystem import, so the host reads a `*_path` secret
        // for us and inlines it as `*_contents`.
        contents
    } else if let Some(path) = string_field(
        config,
        &["credentials_path", "key_path", "service_account_path"],
    ) {
        let reason = string_field(
            config,
            &[
                "credentials_contents_error",
                "key_contents_error",
                "service_account_contents_error",
            ],
        )
        .unwrap_or_else(|| "the file could not be read".to_string());
        return Err(format!("{path}: {reason}"));
    } else {
        return Ok(None);
    };

    let parsed: Value = serde_json::from_str(raw.trim()).map_err(|e| {
        format!("the configured credentials are not valid JSON: {e}. Expected a downloaded \
                 service-account key or an application_default_credentials.json file.")
    })?;

    let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "service_account" => {
            let client_email = parsed
                .get("client_email")
                .and_then(Value::as_str)
                .ok_or_else(|| "the service-account key has no client_email".to_string())?
                .to_string();
            let private_key = parsed
                .get("private_key")
                .and_then(Value::as_str)
                .ok_or_else(|| "the service-account key has no private_key".to_string())?
                .replace("\\n", "\n");
            Ok(Some(Credential::ServiceAccount {
                client_email,
                private_key,
            }))
        }
        "authorized_user" => Ok(Some(Credential::AuthorizedUser {
            client_id: field(&parsed, "client_id")?,
            client_secret: field(&parsed, "client_secret")?,
            refresh_token: field(&parsed, "refresh_token")?,
            quota_project: parsed
                .get("quota_project_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })),
        other => Err(format!(
            "unsupported credential type {other:?}. Expected \"service_account\" (a key \
             downloaded from the console) or \"authorized_user\" (written by \
             `gcloud auth application-default login`)."
        )),
    }
}

fn field(value: &Value, name: &str) -> Result<String, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("the credential file has no {name}"))
}

/// Parses a PEM private key in either form a key might arrive in.
fn parse_private_key(pem: &str) -> Result<RsaPrivateKey, String> {
    let pem = pem.trim();
    if !pem.contains("PRIVATE KEY") {
        return Err("the service-account private_key is not PEM; it should begin with \
                    `-----BEGIN PRIVATE KEY-----`"
            .to_string());
    }
    // Google issues PKCS#8; accept PKCS#1 too rather than making the
    // distinction the operator's problem.
    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|e| format!("could not parse the service-account private key: {e}"))
}

// ---------------------------------------------------------------------------
// Row decoding — BigQuery's f/v format into ordinary JSON
// ---------------------------------------------------------------------------

/// A schema field, flattened enough to drive decoding and display.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: String,
    pub mode: String,
    pub fields: Vec<Field>,
    pub description: Option<String>,
}

impl Field {
    pub fn is_repeated(&self) -> bool {
        self.mode.eq_ignore_ascii_case("REPEATED")
    }
    pub fn is_record(&self) -> bool {
        matches!(self.ty.as_str(), "RECORD" | "STRUCT")
    }
    /// `type` as written in DDL, so a nested field reads the way a user would
    /// type it.
    pub fn display_type(&self) -> String {
        let base = if self.is_record() {
            format!(
                "STRUCT<{}>",
                self.fields
                    .iter()
                    .map(|f| format!("{} {}", f.name, f.display_type()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            self.ty.clone()
        };
        if self.is_repeated() {
            format!("ARRAY<{base}>")
        } else {
            base
        }
    }
}

/// Parses a `TableSchema` into [`Field`]s.
pub fn parse_schema(schema: &Value) -> Vec<Field> {
    schema
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().map(parse_field).collect())
        .unwrap_or_default()
}

fn parse_field(value: &Value) -> Field {
    Field {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        ty: value
            .get("type")
            .or_else(|| value.get("data_type"))
            .and_then(Value::as_str)
            .unwrap_or("STRING")
            .to_uppercase(),
        mode: value
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("NULLABLE")
            .to_string(),
        fields: value
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| fields.iter().map(parse_field).collect())
            .unwrap_or_default(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_string),
    }
}

/// Turns BigQuery's positional rows into JSON objects keyed by column name.
///
/// This is the join the API declines to do for you. Without it every consumer
/// has to track which index means which column, and the numbers all arrive as
/// strings.
pub fn decode_rows(fields: &[Field], rows: &[Value]) -> Vec<Value> {
    rows.iter().map(|row| decode_row(fields, row)).collect()
}

fn decode_row(fields: &[Field], row: &Value) -> Value {
    let cells = row.get("f").and_then(Value::as_array);
    let mut object = Map::new();
    for (index, field) in fields.iter().enumerate() {
        let cell = cells
            .and_then(|cells| cells.get(index))
            .and_then(|cell| cell.get("v"))
            .unwrap_or(&Value::Null);
        object.insert(field.name.clone(), decode_cell(field, cell));
    }
    Value::Object(object)
}

fn decode_cell(field: &Field, cell: &Value) -> Value {
    if cell.is_null() {
        return Value::Null;
    }
    // REPEATED wraps each element in its own {"v": ...}, so unwrap one layer
    // and decode each element as if the field were singular.
    if field.is_repeated() {
        let scalar = Field {
            mode: "NULLABLE".to_string(),
            ..field.clone()
        };
        return match cell.as_array() {
            Some(items) => Value::Array(
                items
                    .iter()
                    .map(|item| decode_cell(&scalar, item.get("v").unwrap_or(&Value::Null)))
                    .collect(),
            ),
            None => Value::Array(vec![]),
        };
    }
    if field.is_record() {
        return decode_row(&field.fields, cell);
    }
    coerce_scalar(&field.ty, cell)
}

/// Coerces a stringly-typed scalar to the JSON type it represents.
///
/// Every BigQuery scalar arrives as a string. Leaving an INT64 as `"42"` means
/// anything downstream doing arithmetic or comparison silently misbehaves, so
/// convert — but fall back to the string rather than lose data, since NUMERIC
/// and BIGNUMERIC can exceed f64 precision and a timestamp is worth keeping
/// legible.
fn coerce_scalar(ty: &str, cell: &Value) -> Value {
    let Some(text) = cell.as_str() else {
        return cell.clone();
    };
    match ty {
        "INTEGER" | "INT64" => text
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(text)),
        "FLOAT" | "FLOAT64" => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::from(text)),
        "BOOLEAN" | "BOOL" => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => Value::from(other),
        },
        // NUMERIC/BIGNUMERIC are exact decimals that f64 would corrupt, and a
        // TIMESTAMP is epoch-seconds-with-fraction that reads better as given.
        _ => Value::from(text),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders decoded rows as an aligned text table.
///
/// Deliberately not JSON: a table states each column name once instead of once
/// per row, which for a 50-row result is several times fewer tokens for the
/// same information. Nested values fall back to compact JSON in their cell.
pub fn render_rows(fields: &[Field], rows: &[Value], max_cell: usize) -> String {
    if rows.is_empty() {
        return "(no rows)".to_string();
    }
    let headers: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
    let mut table: Vec<Vec<String>> = vec![headers.clone()];

    for row in rows {
        table.push(
            fields
                .iter()
                .map(|field| {
                    let cell = row.get(&field.name).unwrap_or(&Value::Null);
                    clip(&render_cell(cell), max_cell)
                })
                .collect(),
        );
    }

    let columns = headers.len();
    let mut widths = vec![0usize; columns];
    for row in &table {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    for (row_index, row) in table.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                // No trailing padding on the last column: it is invisible and
                // it is not free.
                if index + 1 == columns {
                    cell.clone()
                } else {
                    format!("{cell:<width$}", width = widths[index])
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
        if row_index == 0 {
            let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            out.push_str(rule.join("  ").trim_end());
            out.push('\n');
        }
    }
    out
}

fn render_cell(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// A human-readable byte count. Bytes are the unit BigQuery bills in, so these
/// numbers are read constantly and `1.4 TiB` lands better than 12 digits.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Estimated on-demand cost, rounding the scan up to the 10 MB minimum charge.
///
/// Zero bytes is exempt from that rounding, and the distinction is real rather
/// than pedantic: DDL and metadata-only statements scan nothing and are genuinely
/// free, so charging them the 10 MB minimum invented a cost that BigQuery does
/// not bill. A cost estimate that overstates is as misleading as one that
/// understates.
pub fn estimate_cost(bytes: u64, price_per_tib: f64) -> f64 {
    if bytes == 0 {
        return 0.0;
    }
    let billed = bytes.max(MIN_BILLED_BYTES) as f64;
    billed / BYTES_PER_TIB * price_per_tib
}

/// Formats a cost, keeping small figures legible rather than rounding them to
/// `$0.00` and implying the query is free.
pub fn render_cost(usd: f64) -> String {
    if usd <= 0.0 {
        "free".to_string()
    } else if usd < 0.01 {
        format!("<$0.01 (${usd:.6})")
    } else {
        format!("${usd:.2}")
    }
}

/// A large number with thousands separators.
pub fn commas(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// SQL safety
// ---------------------------------------------------------------------------

/// The leading keyword of a statement, with comments and whitespace stripped.
///
/// Used to keep the read-only tools read-only. This is a guard against
/// accident, not an adversary: IAM is the real boundary, and the description of
/// every tool says so.
pub fn statement_kind(sql: &str) -> String {
    let mut cleaned = String::with_capacity(sql.len());
    let bytes: Vec<char> = sql.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        let next = bytes.get(index + 1).copied().unwrap_or('\0');
        if ch == '-' && next == '-' || ch == '#' {
            while index < bytes.len() && bytes[index] != '\n' {
                index += 1;
            }
        } else if ch == '/' && next == '*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == '*' && bytes[index + 1] == '/') {
                index += 1;
            }
            index += 2;
        } else {
            cleaned.push(ch);
            index += 1;
        }
    }

    cleaned
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('(')
        .to_uppercase()
}

/// Statements that only read. `WITH` and `(` both begin a SELECT.
const READ_ONLY_STATEMENTS: [&str; 4] = ["SELECT", "WITH", "", "("];

/// Refuses anything that is not plainly a read.
pub fn require_read_only(sql: &str) -> Result<(), String> {
    let kind = statement_kind(sql);
    if READ_ONLY_STATEMENTS.contains(&kind.as_str()) {
        return Ok(());
    }
    Err(format!(
        "this tool only runs read queries, and that statement starts with {kind}. \
         Use `bq-execute` for DML and DDL — it is a separate tool precisely so that \
         changing data is a deliberate act."
    ))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// First present, non-empty string among several config aliases.
pub fn string_field(config: &Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(text) = config.get(*name).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn str_arg(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub fn u64_arg(args: &Value, name: &str) -> Option<u64> {
    args.get(name).and_then(as_u64_loose)
}

pub fn bool_arg(args: &Value, name: &str) -> Option<bool> {
    match args.get(name) {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(Value::String(text)) => match text.trim() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A number that might be a JSON number or a stringified one — BigQuery
/// returns int64 fields as strings throughout, so both must work everywhere.
pub fn as_u64_loose(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
        .or_else(|| value.as_f64().map(|f| f as u64))
}

pub fn as_f64_loose(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
}

/// Truncates to `limit` characters on a char boundary, marking the cut.
pub fn clip(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn hex16(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Percent-encodes a form value. Only needed for the token exchange, where a
/// refresh token routinely contains `/` and `+`.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A qualified table reference, validated.
///
/// Identifiers are interpolated into generated SQL by `bq-profile`, so this is
/// the chokepoint that keeps a table name from carrying SQL with it.
pub fn qualify_table(project: &str, dataset: &str, table: &str) -> Result<String, String> {
    for (label, part) in [("dataset", dataset), ("table", table)] {
        if part.is_empty() {
            return Err(format!("{label} must not be empty"));
        }
        if !part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '*' || ch == '$')
        {
            return Err(format!(
                "{label} {part:?} contains characters that are not valid in a BigQuery \
                 identifier"
            ));
        }
    }
    Ok(format!("`{project}.{dataset}.{table}`"))
}
