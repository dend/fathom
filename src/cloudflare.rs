//! The Cloudflare API, for what the S3 API can't do: turning an API token into
//! S3 keys, finding the public domain of a bucket, and purging it from the cache.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::sigv4;

pub const API: &str = "https://api.cloudflare.com/client/v4";
/// URLs or prefixes per purge request. Every plan allows at least this many.
pub const PURGE_BATCH: usize = 100;
const ATTEMPTS: u32 = 4;

pub struct Api {
    agent: ureq::Agent,
    base: String,
    account_id: String,
    token: String,
}

/// Every Cloudflare API response wraps its result like this.
#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct TokenStatus {
    id: String,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize, Default)]
struct CustomDomains {
    #[serde(default)]
    domains: Vec<CustomDomain>,
}

#[derive(Deserialize)]
struct CustomDomain {
    domain: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    status: DomainStatus,
    #[serde(default, rename = "zoneId")]
    zone_id: String,
}

#[derive(Deserialize, Default)]
struct DomainStatus {
    #[serde(default)]
    ownership: String,
    #[serde(default)]
    ssl: String,
}

#[derive(Deserialize)]
struct Zone {
    id: String,
}

/// Why a request failed, and how long to wait before trying again, when
/// trying again may help.
struct Failure {
    message: String,
    status: u16,
    retry_after: Option<Duration>,
}

#[derive(Deserialize, Default)]
struct ManagedDomain {
    #[serde(default)]
    domain: String,
    #[serde(default)]
    enabled: bool,
}

impl Api {
    pub fn new(account_id: &str, token: &str) -> Self {
        // Points the app at a local stand-in for the API, such as the mock server
        // in examples/mock_s3.rs.
        let base = std::env::var("FATHOM_CLOUDFLARE_API").unwrap_or_else(|_| API.to_string());
        Self::with_base(&base, account_id, token)
    }

    pub fn with_base(base: &str, account_id: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(concat!("fathom/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Self {
            agent,
            base: base.trim_end_matches('/').to_string(),
            account_id: account_id.trim().to_string(),
            token: token.trim().to_string(),
        }
    }

    /// The S3 key pair for this token. Cloudflare derives it from the token: the
    /// access key ID is the token's ID and the secret is the SHA-256 of its value.
    /// Asking for the ID also checks that the token is valid.
    pub fn s3_keys(&self) -> Result<(String, String), String> {
        let id = self.token_id()?;
        Ok((id, sigv4::sha256_hex(self.token.as_bytes())))
    }

    fn token_id(&self) -> Result<String, String> {
        // Account tokens and user tokens are verified at different paths.
        let path = format!("/accounts/{}/tokens/verify", self.account_id);
        let status = match self.get::<TokenStatus>(&path, "") {
            Ok(status) => status,
            Err(_) => self
                .get::<TokenStatus>("/user/tokens/verify", "")
                .map_err(|e| format!("Cloudflare didn't accept this API token: {e}"))?,
        };
        if !status.status.is_empty() && status.status != "active" {
            return Err(format!("This API token is {}", status.status));
        }
        Ok(status.id)
    }

    /// The public base URL of a bucket, such as `https://cdn.example.com`: an
    /// active custom domain, or else its r2.dev address if public access is on.
    /// `None` when the bucket has neither.
    pub fn public_domain(
        &self,
        bucket: &str,
        jurisdiction: &str,
    ) -> Result<Option<String>, String> {
        let custom: CustomDomains = self.get(&self.domains_path(bucket, "custom"), jurisdiction)?;
        if let Some(domain) = pick_custom(&custom) {
            return Ok(Some(domain));
        }
        let managed: ManagedDomain =
            self.get(&self.domains_path(bucket, "managed"), jurisdiction)?;
        Ok(pick_managed(&managed))
    }

    /// Every custom domain that serves a bucket, as a base URL such as
    /// `https://cdn.example.com`, with the ID of its zone when Cloudflare says.
    pub fn custom_domains(
        &self,
        bucket: &str,
        jurisdiction: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let custom: CustomDomains = self.get(&self.domains_path(bucket, "custom"), jurisdiction)?;
        Ok(custom
            .domains
            .into_iter()
            .filter(serving)
            .map(|d| (format!("https://{}", d.domain), d.zone_id))
            .collect())
    }

    fn domains_path(&self, bucket: &str, kind: &str) -> String {
        format!(
            "/accounts/{}/r2/buckets/{}/domains/{kind}",
            self.account_id,
            sigv4::uri_encode(bucket, true)
        )
    }

    /// The ID of the zone that serves `host`, needed to purge its cache: the
    /// longest part of the hostname that is a zone in the account.
    pub fn zone_of(&self, host: &str) -> Result<String, String> {
        let labels: Vec<&str> = host.split('.').collect();
        for start in 0..labels.len().saturating_sub(1) {
            let name = labels[start..].join(".");
            let path = format!("/zones?name={}", sigv4::uri_encode(&name, true));
            let zones: Vec<Zone> = self.get(&path, "").map_err(|e| {
                format!(
                    "Could not find the Cloudflare zone of {host}: {e}. Attach {host} to the \
                     bucket as a custom domain, or give the API token the Zone > Zone > Read \
                     permission."
                )
            })?;
            if let Some(zone) = zones.into_iter().next() {
                return Ok(zone.id);
            }
        }
        Err(format!(
            "{host} isn't in a zone of this Cloudflare account, so its cache can't be purged."
        ))
    }

    /// Removes URLs such as `https://cdn.example.com/a.png` and prefixes such
    /// as `cdn.example.com/photos/` from the cache of a zone. `progress` gets
    /// how many of them are done.
    pub fn purge(
        &self,
        zone_id: &str,
        files: &[String],
        prefixes: &[String],
        progress: &dyn Fn(usize),
    ) -> Result<(), String> {
        let path = format!("/zones/{}/purge_cache", sigv4::uri_encode(zone_id, true));
        let mut done = 0;
        for (field, items) in [("files", files), ("prefixes", prefixes)] {
            for batch in items.chunks(PURGE_BATCH) {
                let body = serde_json::json!({ field: batch });
                self.request::<serde_json::Value>(&path, "", Some(&body))
                    .map_err(|failure| match failure.status {
                        401 | 403 => format!(
                            "{}. Purging needs an API token with the Zone > Cache Purge \
                             permission.",
                            failure.message
                        ),
                        _ => failure.message,
                    })?;
                done += batch.len();
                progress(done);
            }
        }
        Ok(())
    }

    fn get<T: DeserializeOwned>(&self, path: &str, jurisdiction: &str) -> Result<T, String> {
        self.request(path, jurisdiction, None)
            .map_err(|failure| failure.message)
    }

    /// Sends a GET, or a POST when there is a body, trying again while
    /// Cloudflare is rate limiting or briefly unavailable.
    fn request<T: DeserializeOwned>(
        &self,
        path: &str,
        jurisdiction: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Failure> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(path, jurisdiction, body) {
                Err(Failure {
                    retry_after: Some(wait),
                    ..
                }) if attempt < ATTEMPTS => std::thread::sleep(wait),
                other => return other,
            }
        }
    }

    fn send<T: DeserializeOwned>(
        &self,
        path: &str,
        jurisdiction: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, Failure> {
        let url = format!("{}{path}", self.base);
        let authorization = format!("Bearer {}", self.token);
        let result = match body {
            Some(body) => self
                .agent
                .post(&url)
                .header("authorization", &authorization)
                .header("content-type", "application/json")
                .send(body.to_string()),
            None => {
                let mut request = self.agent.get(&url).header("authorization", &authorization);
                if !jurisdiction.is_empty() {
                    request = request.header("cf-r2-jurisdiction", jurisdiction);
                }
                request.call()
            }
        };
        let fail = |message: String, status: u16, retry_after: Option<Duration>| Failure {
            message,
            status,
            retry_after,
        };
        let mut response = result.map_err(|e| fail(self.redact(&e.to_string()), 0, None))?;
        let status = response.status().as_u16();
        let retry_after = (status == 429 || status >= 500).then(|| {
            let seconds: u64 = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(2);
            Duration::from_secs(seconds.clamp(1, 30))
        });
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()
            .map_err(|e| fail(self.redact(&e.to_string()), status, retry_after))?;
        let envelope: Envelope<T> = serde_json::from_str(&body).map_err(|_| {
            let message = format!("Unexpected response from Cloudflare (HTTP {status})");
            fail(message, status, retry_after)
        })?;
        match envelope.result {
            Some(result) if envelope.success && status < 300 => Ok(result),
            _ => {
                let message = match envelope.errors.first() {
                    Some(error) => self.redact(&format!("{} (code {})", error.message, error.code)),
                    None => format!("Cloudflare returned HTTP {status}"),
                };
                Err(fail(message, status, retry_after))
            }
        }
    }

    /// Keeps the account ID and the token out of messages that reach the screen.
    fn redact(&self, text: &str) -> String {
        let mut text = text.to_string();
        for (secret, label) in [(&self.token, "[token]"), (&self.account_id, "[account]")] {
            if secret.len() >= 8 {
                text = text.replace(secret.as_str(), label);
            }
        }
        text
    }
}

/// Whether a custom domain is set up and answering requests.
fn serving(domain: &CustomDomain) -> bool {
    domain.enabled
        && domain.status.ownership == "active"
        && matches!(domain.status.ssl.as_str(), "active" | "")
}

fn pick_custom(custom: &CustomDomains) -> Option<String> {
    custom
        .domains
        .iter()
        .find(|d| serving(d))
        .map(|d| format!("https://{}", d.domain))
}

fn pick_managed(managed: &ManagedDomain) -> Option<String> {
    (managed.enabled && !managed.domain.is_empty()).then(|| format!("https://{}", managed.domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(json: &str) -> CustomDomains {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn picks_the_first_active_custom_domain() {
        let domains = custom(
            r#"{"domains": [
                {"domain": "old.example.com", "enabled": false, "status": {"ownership": "active", "ssl": "active"}},
                {"domain": "new.example.com", "enabled": true, "status": {"ownership": "pending", "ssl": "pending"}},
                {"domain": "cdn.example.com", "enabled": true, "status": {"ownership": "active", "ssl": "active"}, "minTLS": "1.2"}
            ]}"#,
        );
        assert_eq!(
            pick_custom(&domains).as_deref(),
            Some("https://cdn.example.com")
        );
        assert_eq!(pick_custom(&custom(r#"{"domains": []}"#)), None);
    }

    #[test]
    fn reads_the_zone_of_a_custom_domain_when_given() {
        let domains = custom(
            r#"{"domains": [
                {"domain": "cdn.example.com", "enabled": true, "status": {"ownership": "active", "ssl": "active"}, "zoneId": "023e105f4ecef8ad9ca31a8372d0c353", "zoneName": "example.com"},
                {"domain": "old.example.com", "enabled": true, "status": {"ownership": "active", "ssl": "active"}}
            ]}"#,
        );
        let zones: Vec<&str> = domains.domains.iter().map(|d| d.zone_id.as_str()).collect();
        assert_eq!(zones, ["023e105f4ecef8ad9ca31a8372d0c353", ""]);
    }

    #[test]
    fn uses_the_r2_dev_address_only_when_public() {
        let on: ManagedDomain = serde_json::from_str(
            r#"{"bucketId": "b1", "domain": "pub-abc.r2.dev", "enabled": true}"#,
        )
        .unwrap();
        let off: ManagedDomain = serde_json::from_str(
            r#"{"bucketId": "b1", "domain": "pub-abc.r2.dev", "enabled": false}"#,
        )
        .unwrap();
        assert_eq!(pick_managed(&on).as_deref(), Some("https://pub-abc.r2.dev"));
        assert_eq!(pick_managed(&off), None);
    }

    #[test]
    fn secret_is_the_sha256_of_the_token() {
        // The documented derivation: `echo -n "<token>" | shasum -a 256`.
        assert_eq!(
            sigv4::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn errors_never_show_the_token() {
        let api = Api::with_base(
            "http://127.0.0.1:1",
            "0123456789abcdef0123456789abcdef",
            "secret-token-value",
        );
        let text =
            api.redact("bad token secret-token-value for account 0123456789abcdef0123456789abcdef");
        assert_eq!(text, "bad token [token] for account [account]");
    }
}
