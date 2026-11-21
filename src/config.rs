//! Connection profiles and where they are kept.
//!
//! Account ID, API token, access key ID, secret access key and custom endpoint
//! are stored together as one entry in the OS credential store (Windows
//! Credential Manager, macOS Keychain, Secret Service on Linux). The config file
//! only holds what is needed to draw the UI: names, bucket lists, public domains.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::s3::S3Client;

const KEYRING_SERVICE: &str = "dev.den.fathom";

/// Everything that identifies or grants access to an account. Lives only in
/// the credential store and in memory; never serialized anywhere else.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Credentials {
    pub account_id: String,
    /// The Cloudflare API token, when the connection was set up with one. The S3
    /// keys below are then derived from it.
    pub api_token: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Overrides the endpoint derived from the account ID when set.
    pub endpoint: String,
}

impl Credentials {
    pub fn endpoint_url(&self, jurisdiction: &str) -> String {
        if !self.endpoint.is_empty() {
            return self.endpoint.clone();
        }
        let jurisdiction = match jurisdiction {
            "" | "default" => String::new(),
            other => format!("{other}."),
        };
        format!(
            "https://{}.{jurisdiction}r2.cloudflarestorage.com",
            self.account_id
        )
    }

    /// Trims pasted values, and accepts a pasted S3 endpoint URL in place of
    /// the bare account ID.
    pub fn normalize(&mut self) {
        self.api_token = self.api_token.trim().to_string();
        self.access_key_id = self.access_key_id.trim().to_string();
        self.secret_access_key = self.secret_access_key.trim().to_string();
        self.endpoint = self.endpoint.trim().trim_end_matches('/').to_string();

        let account = self.account_id.trim();
        let account = account
            .strip_prefix("https://")
            .or_else(|| account.strip_prefix("http://"))
            .unwrap_or(account);
        self.account_id = account
            .split(['.', '/'])
            .next()
            .unwrap_or_default()
            .to_string();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Connection {
    pub id: String,
    pub name: String,
    /// `""` (default), `"eu"` or `"fedramp"`.
    pub jurisdiction: String,
    /// Whether a custom endpoint is set. The endpoint itself is a credential.
    pub custom_endpoint: bool,
    /// Whether the connection uses an API token, so public domains can be looked
    /// up. The token itself is a credential.
    pub uses_token: bool,
    /// Buckets to show when the token is not allowed to list buckets.
    pub buckets: Vec<String>,
    /// Public base URL per bucket, e.g. `media` -> `https://cdn.example.com`, set
    /// by hand. These take precedence over domains looked up with the API token.
    pub domains: BTreeMap<String, String>,

    // Fields written by 0.1.0, which kept these in the config file. They are
    // read once so they can be moved to the credential store, and never
    // written back.
    #[serde(skip_serializing)]
    account_id: String,
    #[serde(skip_serializing)]
    access_key_id: String,
    #[serde(skip_serializing)]
    endpoint: String,
}

impl Connection {
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = "R2".to_string();
        }
        self.jurisdiction = self.jurisdiction.trim().to_ascii_lowercase();
        if self.jurisdiction == "default" {
            self.jurisdiction.clear();
        }
        self.buckets = self
            .buckets
            .iter()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect();
        self.buckets.sort();
        self.buckets.dedup();
    }

    fn has_legacy_fields(&self) -> bool {
        !(self.account_id.is_empty() && self.access_key_id.is_empty() && self.endpoint.is_empty())
    }
}

/// Turns user input such as `cdn.example.com/` into `https://cdn.example.com`.
pub fn normalize_domain(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (scheme, rest),
        Some(_) => return Err("The domain must use http or https".into()),
        None => ("https", trimmed),
    };
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || rest.contains([' ', '?', '#']) {
        return Err("Enter a domain such as cdn.example.com".into());
    }
    Ok(format!("{scheme}://{rest}"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub connections: Vec<Connection>,
    pub active: Option<String>,
}

/// Where credentials are kept. The app uses the OS credential store; tests
/// substitute an in-memory one.
pub trait Vault: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<String>, String>;
    fn set(&self, id: &str, value: &str) -> Result<(), String>;
    fn delete(&self, id: &str);
}

pub struct Keychain;

impl Keychain {
    fn entry(id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, id)
            .map_err(|e| format!("Could not reach the system credential store: {e}"))
    }
}

impl Vault for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        match Self::entry(id)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!(
                "Could not read from the system credential store: {e}"
            )),
        }
    }

    fn set(&self, id: &str, value: &str) -> Result<(), String> {
        Self::entry(id)?
            .set_password(value)
            .map_err(|e| format!("Could not save to the system credential store: {e}"))
    }

    fn delete(&self, id: &str) {
        if let Ok(entry) = Self::entry(id) {
            let _ = entry.delete_credential();
        }
    }
}

pub struct Store {
    path: PathBuf,
    config: Mutex<Config>,
    vault: Box<dyn Vault>,
    clients: Mutex<HashMap<String, Arc<S3Client>>>,
}

impl Store {
    pub fn load(dir: PathBuf) -> Self {
        Self::load_with(dir, Box::new(Keychain))
    }

    pub fn load_with(dir: PathBuf, vault: Box<dyn Vault>) -> Self {
        let path = dir.join("config.json");
        let config = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let store = Self {
            path,
            config: Mutex::new(config),
            vault,
            clients: Mutex::new(HashMap::new()),
        };
        store.migrate();
        store
    }

    /// Moves account details that 0.1.0 wrote to the config file into the
    /// credential store, then rewrites the file without them.
    fn migrate(&self) {
        let pending: Vec<Connection> = self
            .config
            .lock()
            .unwrap()
            .connections
            .iter()
            .filter(|c| c.has_legacy_fields())
            .cloned()
            .collect();
        if pending.is_empty() {
            return;
        }

        let mut moved = Vec::new();
        for connection in pending {
            // The old entry held just the secret; a JSON entry is already current.
            let stored = match self.vault.get(&connection.id) {
                Ok(stored) => stored.unwrap_or_default(),
                Err(_) => continue,
            };
            if serde_json::from_str::<Credentials>(&stored).is_err() {
                let credentials = Credentials {
                    account_id: connection.account_id.clone(),
                    access_key_id: connection.access_key_id.clone(),
                    secret_access_key: stored,
                    endpoint: connection.endpoint.clone(),
                    ..Default::default()
                };
                if self.save_credentials(&connection.id, &credentials).is_err() {
                    continue;
                }
            }
            moved.push(connection.id);
        }

        let _ = self.update(|config| {
            for connection in &mut config.connections {
                if moved.contains(&connection.id) {
                    connection.custom_endpoint = !connection.endpoint.is_empty();
                    connection.account_id.clear();
                    connection.access_key_id.clear();
                    connection.endpoint.clear();
                }
            }
        });
    }

    pub fn snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn connection(&self, id: &str) -> Result<Connection, String> {
        self.config
            .lock()
            .unwrap()
            .connections
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| "This connection no longer exists".to_string())
    }

    /// Applies a change to the config and writes it to disk. If it can't be
    /// written, the change is undone, so what the app shows matches the file.
    pub fn update<T>(&self, change: impl FnOnce(&mut Config) -> T) -> Result<T, String> {
        let mut config = self.config.lock().unwrap();
        let before = config.clone();
        let result = change(&mut config);
        if let Err(error) = self.write(&config) {
            *config = before;
            return Err(error);
        }
        Ok(result)
    }

    fn write(&self, config: &Config) -> Result<(), String> {
        let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            if dir.exists() && !dir.is_dir() {
                return Err(format!(
                    "Could not save settings: Fathom keeps them in the folder {}, but a file \
                     with that name is in the way. Move or delete the file, then try again.",
                    dir.display()
                ));
            }
            fs::create_dir_all(dir).map_err(|e| {
                format!(
                    "Could not create the settings folder {}: {e}",
                    dir.display()
                )
            })?;
        }
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, json)
            .and_then(|()| fs::rename(&temp, &self.path))
            .map_err(|e| format!("Could not save settings to {}: {e}", self.path.display()))
    }

    pub fn credentials(&self, id: &str) -> Result<Credentials, String> {
        let missing = "The credentials for this connection are missing from the system credential \
                       store. Edit the connection and enter them again.";
        let stored = self.vault.get(id)?.ok_or(missing)?;
        serde_json::from_str(&stored).map_err(|_| missing.to_string())
    }

    pub fn save_credentials(&self, id: &str, credentials: &Credentials) -> Result<(), String> {
        let json = serde_json::to_string(credentials).map_err(|e| e.to_string())?;
        self.vault.set(id, &json)?;
        self.forget_client(id);
        Ok(())
    }

    pub fn delete_credentials(&self, id: &str) {
        self.vault.delete(id);
        self.forget_client(id);
    }

    /// Returns the (cached) S3 client for a connection.
    pub fn client(&self, id: &str) -> Result<Arc<S3Client>, String> {
        if let Some(client) = self.clients.lock().unwrap().get(id) {
            return Ok(client.clone());
        }
        let connection = self.connection(id)?;
        let credentials = self.credentials(id)?;
        let client = Arc::new(
            S3Client::new(
                &credentials.endpoint_url(&connection.jurisdiction),
                &credentials.access_key_id,
                &credentials.secret_access_key,
            )
            .map_err(|e| e.to_string())?,
        );
        self.clients
            .lock()
            .unwrap()
            .insert(id.to_string(), client.clone());
        Ok(client)
    }

    /// The Cloudflare API for a connection set up with an API token.
    pub fn cloudflare(&self, id: &str) -> Result<Option<crate::cloudflare::Api>, String> {
        let credentials = self.credentials(id)?;
        if credentials.api_token.is_empty() {
            return Ok(None);
        }
        Ok(Some(crate::cloudflare::Api::new(
            &credentials.account_id,
            &credentials.api_token,
        )))
    }

    pub fn forget_client(&self, id: &str) {
        self.clients.lock().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryVault(Arc<Mutex<HashMap<String, String>>>);

    impl Vault for MemoryVault {
        fn get(&self, id: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
        fn set(&self, id: &str, value: &str) -> Result<(), String> {
            self.0.lock().unwrap().insert(id.into(), value.into());
            Ok(())
        }
        fn delete(&self, id: &str) {
            self.0.lock().unwrap().remove(id);
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fathom-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn derives_endpoints() {
        let mut credentials = Credentials {
            account_id: " https://ABC123.r2.cloudflarestorage.com/ ".into(),
            ..Default::default()
        };
        credentials.normalize();
        assert_eq!(credentials.account_id, "ABC123");
        assert_eq!(
            credentials.endpoint_url(""),
            "https://ABC123.r2.cloudflarestorage.com"
        );
        assert_eq!(
            credentials.endpoint_url("eu"),
            "https://ABC123.eu.r2.cloudflarestorage.com"
        );

        credentials.endpoint = "http://localhost:9000/".into();
        credentials.normalize();
        assert_eq!(credentials.endpoint_url("eu"), "http://localhost:9000");
    }

    #[test]
    fn normalizes_domains() {
        assert_eq!(normalize_domain("").unwrap(), "");
        assert_eq!(
            normalize_domain(" cdn.example.com/ ").unwrap(),
            "https://cdn.example.com"
        );
        assert_eq!(
            normalize_domain("http://cdn.example.com/assets/").unwrap(),
            "http://cdn.example.com/assets"
        );
        assert!(normalize_domain("ftp://cdn.example.com").is_err());
        assert!(normalize_domain("cdn.example.com/?x=1").is_err());
    }

    #[test]
    fn account_details_stay_out_of_the_config_file() {
        let dir = temp_dir("clean");
        let store = Store::load_with(dir.clone(), Box::new(MemoryVault::default()));
        let credentials = Credentials {
            account_id: "ACCOUNT-ID-123".into(),
            api_token: "API-TOKEN-321".into(),
            access_key_id: "ACCESS-KEY-456".into(),
            secret_access_key: "SECRET-789".into(),
            endpoint: "https://ENDPOINT-000.example.com".into(),
        };
        store.save_credentials("one", &credentials).unwrap();
        store
            .update(|config| {
                config.connections.push(Connection {
                    id: "one".into(),
                    name: "Main".into(),
                    custom_endpoint: true,
                    uses_token: true,
                    ..Default::default()
                });
                config.active = Some("one".into());
            })
            .unwrap();

        let on_disk = fs::read_to_string(dir.join("config.json")).unwrap();
        let to_ui = serde_json::to_string(&store.snapshot()).unwrap();
        for text in [&on_disk, &to_ui] {
            for needle in [
                "ACCOUNT-ID",
                "API-TOKEN",
                "ACCESS-KEY",
                "SECRET",
                "ENDPOINT-000",
            ] {
                assert!(!text.contains(needle), "{needle} leaked into {text}");
            }
        }
        assert!(store.credentials("one").unwrap() == credentials);

        store.delete_credentials("one");
        assert!(store.credentials("one").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn moves_account_details_written_by_older_versions() {
        let dir = temp_dir("migrate");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            r#"{"connections":[{"id":"one","name":"Main","accountId":"ACCOUNT-ID-123",
                "accessKeyId":"ACCESS-KEY-456","endpoint":"","jurisdiction":"eu",
                "domains":{"media":"https://cdn.example.com"}}],"active":"one"}"#,
        )
        .unwrap();
        let vault = MemoryVault::default();
        let entries = vault.0.clone();
        vault.set("one", "SECRET-789").unwrap();

        let store = Store::load_with(dir.clone(), Box::new(vault));

        let credentials = store.credentials("one").unwrap();
        assert_eq!(credentials.account_id, "ACCOUNT-ID-123");
        assert_eq!(credentials.access_key_id, "ACCESS-KEY-456");
        assert_eq!(credentials.secret_access_key, "SECRET-789");

        let on_disk = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!on_disk.contains("ACCOUNT-ID"), "{on_disk}");
        assert!(!on_disk.contains("ACCESS-KEY"), "{on_disk}");
        assert!(!on_disk.contains("accountId"), "{on_disk}");

        // Everything else survives, and a second load changes nothing.
        let connection = store.connection("one").unwrap();
        assert_eq!(connection.jurisdiction, "eu");
        assert_eq!(connection.domains["media"], "https://cdn.example.com");
        assert!(!connection.custom_endpoint);
        let before = entries.lock().unwrap().clone();
        drop(store);
        let reloaded = Store::load_with(
            dir.clone(),
            Box::new(MemoryVault(Arc::new(Mutex::new(before.clone())))),
        );
        assert!(reloaded.credentials("one").unwrap() == credentials);
        assert_eq!(reloaded.snapshot().active.as_deref(), Some("one"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn saves_once_the_old_window_state_file_is_moved_aside() {
        // Up to 0.0.2, eframe wrote its state file where the settings folder goes.
        let parent = temp_dir("state");
        let dir = parent.join("dev.den.fathom");
        fs::create_dir_all(&parent).unwrap();
        fs::write(&dir, "(window: ())").unwrap();

        let blocked = Store::load_with(dir.clone(), Box::new(MemoryVault::default()));
        let error = blocked
            .update(|config| config.connections.push(Connection::default()))
            .unwrap_err();
        assert!(error.contains(&dir.display().to_string()), "{error}");
        assert!(
            error.contains("a file with that name is in the way"),
            "{error}"
        );
        // A change that couldn't be saved doesn't linger in memory either.
        assert!(blocked.snapshot().connections.is_empty());

        // On macOS the settings folder kept its name, so old and new are the same.
        crate::system::move_old_settings(&dir, &dir);
        assert!(dir.is_dir());
        let state = crate::system::state_file(&dir);
        assert_eq!(fs::read_to_string(state).unwrap(), "(window: ())");
        let store = Store::load_with(dir.clone(), Box::new(MemoryVault::default()));
        store
            .update(|config| config.active = Some("one".into()))
            .unwrap();
        assert!(dir.join("config.json").is_file());

        // Nothing to move the second time.
        crate::system::move_old_settings(&dir, &dir);
        assert!(dir.join("config.json").is_file());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn moves_settings_from_the_old_folder_name() {
        let parent = temp_dir("rename");
        let old = parent.join("dev.den.fathom");
        let new = parent.join("Fathom");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join("config.json"),
            r#"{"connections":[{"id":"one","name":"Main"}],"active":"one"}"#,
        )
        .unwrap();
        fs::write(old.join("app.ron"), "(window: ())").unwrap();

        crate::system::move_old_settings(&old, &new);
        assert!(!old.exists());
        let store = Store::load_with(new.clone(), Box::new(MemoryVault::default()));
        assert_eq!(store.connection("one").unwrap().name, "Main");
        assert!(crate::system::state_file(&new).is_file());

        // A stray state file at the old path joins the new folder without
        // replacing the state already there.
        fs::write(&old, "(stray: ())").unwrap();
        crate::system::move_old_settings(&old, &new);
        assert!(!old.exists());
        let state = fs::read_to_string(crate::system::state_file(&new)).unwrap();
        assert_eq!(state, "(window: ())");
        assert!(new.join("config.json").is_file());
        let _ = fs::remove_dir_all(parent);
    }
}
