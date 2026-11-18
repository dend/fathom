//! The Cloudflare API, for what the S3 API can't do: turning an API token into
//! S3 keys, and finding the public domain of a bucket.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::sigv4;

const API: &str = "https://api.cloudflare.com/client/v4";

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
}

#[derive(Deserialize, Default)]
struct DomainStatus {
    #[serde(default)]
    ownership: String,
    #[serde(default)]
    ssl: String,
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
        let path = format!(
            "/accounts/{}/r2/buckets/{}/domains",
            self.account_id,
            sigv4::uri_encode(bucket, true)
        );
        let custom: CustomDomains = self.get(&format!("{path}/custom"), jurisdiction)?;
        if let Some(domain) = pick_custom(&custom) {
            return Ok(Some(domain));
        }
        let managed: ManagedDomain = self.get(&format!("{path}/managed"), jurisdiction)?;
        Ok(pick_managed(&managed))
    }

    fn get<T: DeserializeOwned>(&self, path: &str, jurisdiction: &str) -> Result<T, String> {
        let mut request = self
            .agent
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token));
        if !jurisdiction.is_empty() {
            request = request.header("cf-r2-jurisdiction", jurisdiction);
        }
        let mut response = request.call().map_err(|e| self.redact(&e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()
            .map_err(|e| self.redact(&e.to_string()))?;
        let envelope: Envelope<T> = serde_json::from_str(&body)
            .map_err(|_| format!("Unexpected response from Cloudflare (HTTP {status})"))?;
        match envelope.result {
            Some(result) if envelope.success && status < 300 => Ok(result),
            _ => Err(match envelope.errors.first() {
                Some(error) => self.redact(&format!("{} (code {})", error.message, error.code)),
                None => format!("Cloudflare returned HTTP {status}"),
            }),
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

fn pick_custom(custom: &CustomDomains) -> Option<String> {
    custom
        .domains
        .iter()
        .find(|d| {
            d.enabled
                && d.status.ownership == "active"
                && matches!(d.status.ssl.as_str(), "active" | "")
        })
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
