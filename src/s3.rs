//! A small S3 client covering the subset of the API that R2 implements and
//! the app uses. Requests are signed with SigV4 and sent over a blocking
//! HTTP agent; callers are expected to run it off the UI thread.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use ureq::http;

use crate::sigv4::{self, Credentials, UNSIGNED_PAYLOAD};
use crate::xml;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

const MAX_PARTS: u64 = 10_000;
const ATTEMPTS: u32 = 3;
const IO_BUFFER: usize = 256 * 1024;

#[derive(Debug)]
pub enum Error {
    /// The service answered with an error status.
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// The request never got a usable answer (DNS, TLS, timeouts, ...).
    Transport(String),
    Io(io::Error),
    Cancelled,
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api {
                status,
                code,
                message,
            } => {
                if message.is_empty() {
                    write!(f, "{code} (HTTP {status})")
                } else {
                    write!(f, "{message} ({code}, HTTP {status})")
                }
            }
            Error::Transport(message) => write!(f, "Network error: {message}"),
            Error::Io(err) => write!(f, "{err}"),
            Error::Cancelled => write!(f, "Cancelled"),
            Error::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl Error {
    fn is_retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Api { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Api { status: 404, .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub name: String,
    pub created: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Object {
    pub key: String,
    pub size: u64,
    /// ISO 8601 timestamp as returned by the service.
    pub modified: String,
    pub etag: String,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPage {
    pub prefixes: Vec<String>,
    pub objects: Vec<Object>,
    pub next_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectInfo {
    pub size: u64,
    pub content_type: String,
    pub etag: String,
    /// HTTP date as returned by the service.
    pub modified: String,
    pub cache_control: String,
    pub metadata: Vec<(String, String)>,
}

/// Size limits that decide when multipart operations kick in.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Files at or above this size are uploaded in parts.
    pub multipart_threshold: u64,
    pub min_part_size: u64,
    /// A single CopyObject call cannot copy more than this.
    pub single_copy_limit: u64,
    pub copy_part_size: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            multipart_threshold: 64 * MIB,
            min_part_size: 32 * MIB,
            single_copy_limit: 5 * GIB,
            copy_part_size: GIB,
        }
    }
}

enum Payload<'a> {
    Empty,
    Bytes(&'a [u8]),
    Stream(&'a mut dyn Read, u64),
}

/// Reports the absolute number of bytes transferred so far.
pub type Progress<'a> = &'a (dyn Fn(u64) + Sync);

pub struct S3Client {
    agent: ureq::Agent,
    /// `scheme://host[:port]`, no trailing slash.
    base: String,
    /// Value of the `Host` header, which is part of the signature.
    host: String,
    creds: Credentials,
    pub tuning: Tuning,
}

impl S3Client {
    /// `endpoint` is the S3 API endpoint, for example
    /// `https://<account-id>.r2.cloudflarestorage.com`.
    pub fn new(endpoint: &str, access_key_id: &str, secret_access_key: &str) -> Result<Self> {
        let endpoint = endpoint.trim().trim_end_matches('/');
        let (scheme, host) = match endpoint.split_once("://") {
            Some((scheme @ ("http" | "https"), host)) => (scheme, host),
            Some(_) => return Err(Error::Invalid("Endpoint must use http or https".into())),
            None => ("https", endpoint),
        };
        if host.is_empty() || host.contains(['/', '?', '#', ' ', '@']) {
            return Err(Error::Invalid(
                "Endpoint must be a bare host such as <account-id>.r2.cloudflarestorage.com".into(),
            ));
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(concat!("fathom/", env!("CARGO_PKG_VERSION")))
            .timeout_resolve(Some(Duration::from_secs(15)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .timeout_per_write(Some(Duration::from_secs(60)))
            .max_idle_connections_per_host(16)
            .build()
            .into();

        Ok(Self {
            agent,
            base: format!("{scheme}://{host}"),
            host: host.to_string(),
            creds: Credentials {
                access_key_id: access_key_id.trim().to_string(),
                secret_access_key: secret_access_key.trim().to_string(),
                region: "auto".to_string(),
            },
            tuning: Tuning::default(),
        })
    }

    /// Removes account details from text that may end up on screen. Error
    /// messages from the network stack and the service can echo the endpoint
    /// (which contains the account ID) or the access key.
    fn redact(&self, text: &str) -> String {
        let account = self.host.split('.').next().unwrap_or_default();
        let mut text = text.replace(&self.host, "[endpoint]");
        for (secret, label) in [
            (self.creds.secret_access_key.as_str(), "[secret]"),
            (self.creds.access_key_id.as_str(), "[access key]"),
            (account, "[account]"),
        ] {
            if secret.len() >= 8 {
                text = text.replace(secret, label);
            }
        }
        text
    }

    fn transport_error(&self, err: impl fmt::Display) -> Error {
        Error::Transport(self.redact(&err.to_string()))
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        payload: Payload<'_>,
    ) -> Result<http::Response<ureq::Body>> {
        let canonical_path = sigv4::uri_encode(path, false);
        let canonical_query = sigv4::canonical_query(query);
        let amz_date = sigv4::amz_date(unix_now());
        let payload_hash = match &payload {
            Payload::Empty => sigv4::sha256_hex(b""),
            Payload::Bytes(bytes) => sigv4::sha256_hex(bytes),
            Payload::Stream(..) => UNSIGNED_PAYLOAD.to_string(),
        };

        let mut signed = vec![
            ("host".to_string(), self.host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        for (name, value) in headers {
            if name.starts_with("x-amz-") {
                signed.push((name.to_string(), value.to_string()));
            }
        }
        let authorization = sigv4::authorization(
            &self.creds,
            &amz_date,
            method,
            &canonical_path,
            &canonical_query,
            &signed,
            &payload_hash,
        );

        let mut url = format!("{}{}", self.base, canonical_path);
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }

        let mut builder = http::Request::builder()
            .method(method)
            .uri(url)
            .header("authorization", authorization)
            .header("x-amz-date", amz_date)
            .header("x-amz-content-sha256", payload_hash);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        let invalid = |err: http::Error| Error::Invalid(format!("Invalid request: {err}"));
        let result = match payload {
            Payload::Empty => self.agent.run(builder.body(()).map_err(invalid)?),
            Payload::Bytes(bytes) => self.agent.run(builder.body(bytes).map_err(invalid)?),
            Payload::Stream(reader, length) => self.agent.run(
                builder
                    .header("content-length", length)
                    .body(ureq::SendBody::from_reader(reader))
                    .map_err(invalid)?,
            ),
        };
        let mut response = result.map_err(|err| self.transport_error(err))?;

        let status = response.status().as_u16();
        if status < 300 {
            return Ok(response);
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();
        Err(api_error(status, &self.redact(&body)))
    }

    /// Sends a request whose body can be replayed, retrying transient failures.
    fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<http::Response<ureq::Body>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let payload = body.map_or(Payload::Empty, Payload::Bytes);
            match self.send(method, path, query, headers, payload) {
                Err(err) if err.is_retryable() && attempt < ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));
                }
                other => return other,
            }
        }
    }

    /// Like [`Self::call`], reading the whole response as text. Some S3
    /// operations report failures inside a `200 OK` body, so that is checked too.
    fn call_xml(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<String> {
        let mut response = self.call(method, path, query, headers, body)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(64 * MIB)
            .read_to_string()
            .map_err(|err| self.transport_error(err))?;
        if xml::element(&text, "Error").is_some() && xml::element(&text, "Code").is_some() {
            return Err(api_error(status, &self.redact(&text)));
        }
        Ok(text)
    }

    pub fn list_buckets(&self) -> Result<Vec<Bucket>> {
        let mut buckets = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = Vec::new();
            if let Some(token) = &token {
                query.push(("continuation-token", token.as_str()));
            }
            let body = self.call_xml("GET", "/", &query, &[], None)?;
            for bucket in xml::elements(&body, "Bucket") {
                buckets.push(Bucket {
                    name: xml::text(bucket, "Name").unwrap_or_default(),
                    created: xml::text(bucket, "CreationDate").unwrap_or_default(),
                });
            }
            token = xml::text(&body, "ContinuationToken").filter(|t| !t.is_empty());
            if token.is_none() {
                break;
            }
        }
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(buckets)
    }

    /// Lists one page of objects. With a `delimiter`, keys are grouped into
    /// `prefixes` the way a file browser shows folders.
    pub fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<ListPage> {
        let mut query = vec![("list-type", "2"), ("encoding-type", "url")];
        if !prefix.is_empty() {
            query.push(("prefix", prefix));
        }
        if let Some(delimiter) = delimiter {
            query.push(("delimiter", delimiter));
        }
        if let Some(token) = token {
            query.push(("continuation-token", token));
        }
        let body = self.call_xml("GET", &format!("/{bucket}"), &query, &[], None)?;

        // Keys are only percent-encoded when the service honoured `encoding-type`.
        let encoded = xml::text(&body, "EncodingType").as_deref() == Some("url");
        let key_text = |raw: &str, tag: &str| {
            let value = xml::text(raw, tag).unwrap_or_default();
            if encoded {
                sigv4::uri_decode(&value, true)
            } else {
                value
            }
        };

        let mut page = ListPage::default();
        for item in xml::elements(&body, "Contents") {
            page.objects.push(Object {
                key: key_text(item, "Key"),
                size: xml::text(item, "Size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                modified: xml::text(item, "LastModified").unwrap_or_default(),
                etag: xml::text(item, "ETag")
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string(),
            });
        }
        for item in xml::elements(&body, "CommonPrefixes") {
            page.prefixes.push(key_text(item, "Prefix"));
        }
        if xml::text(&body, "IsTruncated").as_deref() == Some("true") {
            page.next_token = xml::text(&body, "NextContinuationToken").filter(|t| !t.is_empty());
        }
        Ok(page)
    }

    /// Lists every object under `prefix`, however deep.
    pub fn list_all(&self, bucket: &str, prefix: &str, cancel: &AtomicBool) -> Result<Vec<Object>> {
        let mut objects = Vec::new();
        let mut token: Option<String> = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let page = self.list(bucket, prefix, None, token.as_deref())?;
            objects.extend(page.objects);
            token = page.next_token;
            if token.is_none() {
                return Ok(objects);
            }
        }
    }

    pub fn head(&self, bucket: &str, key: &str) -> Result<ObjectInfo> {
        let response = self.call("HEAD", &object_path(bucket, key), &[], &[], None)?;
        let headers = response.headers();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let mut metadata: Vec<(String, String)> = headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.as_str().strip_prefix("x-amz-meta-")?;
                Some((name.to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();
        metadata.sort();

        Ok(ObjectInfo {
            size: header("content-length").parse().unwrap_or(0),
            content_type: header("content-type"),
            etag: header("etag").trim_matches('"').to_string(),
            modified: header("last-modified"),
            cache_control: header("cache-control"),
            metadata,
        })
    }

    /// Opens an object for streaming, optionally limited to an HTTP `Range`.
    pub fn open(
        &self,
        bucket: &str,
        key: &str,
        range: Option<&str>,
    ) -> Result<http::Response<ureq::Body>> {
        let headers: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        self.send(
            "GET",
            &object_path(bucket, key),
            &[],
            &headers,
            Payload::Empty,
        )
    }

    /// Reads at most `max_bytes` from the start of an object.
    /// Returns the bytes and the full size of the object.
    pub fn read_head(&self, bucket: &str, key: &str, max_bytes: u64) -> Result<(Vec<u8>, u64)> {
        let range = format!("bytes=0-{}", max_bytes.saturating_sub(1));
        let headers = [("range", range.as_str())];
        let response = match self.call("GET", &object_path(bucket, key), &[], &headers, None) {
            // An empty object has no satisfiable range.
            Err(Error::Api { status: 416, .. }) => return Ok((Vec::new(), 0)),
            other => other?,
        };

        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok());
        let total = header("content-range")
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| header("content-length").and_then(|v| v.parse().ok()))
            .unwrap_or(0);

        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(max_bytes)
            .read_to_end(&mut bytes)?;
        let total = total.max(bytes.len() as u64);
        Ok((bytes, total))
    }

    pub fn put_bytes(
        &self,
        bucket: &str,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<()> {
        let headers = [("content-type", content_type)];
        self.call("PUT", &object_path(bucket, key), &[], &headers, Some(bytes))?;
        Ok(())
    }

    pub fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        match self.call("DELETE", &object_path(bucket, key), &[], &[], None) {
            Err(err) if err.is_not_found() => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// Copies an object server-side. `size` picks between a single
    /// CopyObject call and a multipart copy for very large objects.
    pub fn copy(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
        size: u64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let source = format!("/{src_bucket}/{}", sigv4::uri_encode(src_key, false));
        let dst_path = object_path(dst_bucket, dst_key);

        if size <= self.tuning.single_copy_limit {
            let headers = [("x-amz-copy-source", source.as_str())];
            self.call_xml("PUT", &dst_path, &[], &headers, Some(&[]))?;
            return Ok(());
        }

        let info = self.head(src_bucket, src_key)?;
        let upload_id = self.create_multipart(dst_bucket, dst_key, &info.content_type)?;
        let copy_parts = || -> Result<Vec<String>> {
            let mut etags = Vec::new();
            let mut offset = 0;
            while offset < size {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let end = (offset + self.tuning.copy_part_size).min(size) - 1;
                let number = (etags.len() + 1).to_string();
                let range = format!("bytes={offset}-{end}");
                let query = [
                    ("partNumber", number.as_str()),
                    ("uploadId", upload_id.as_str()),
                ];
                let headers = [
                    ("x-amz-copy-source", source.as_str()),
                    ("x-amz-copy-source-range", range.as_str()),
                ];
                let body = self.call_xml("PUT", &dst_path, &query, &headers, Some(&[]))?;
                etags.push(xml::text(&body, "ETag").unwrap_or_default());
                offset = end + 1;
            }
            Ok(etags)
        };
        match copy_parts()
            .and_then(|etags| self.complete_multipart(dst_bucket, dst_key, &upload_id, &etags))
        {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = self.abort_multipart(dst_bucket, dst_key, &upload_id);
                Err(err)
            }
        }
    }

    fn create_multipart(&self, bucket: &str, key: &str, content_type: &str) -> Result<String> {
        let content_type = if content_type.is_empty() {
            "application/octet-stream"
        } else {
            content_type
        };
        let headers = [("content-type", content_type)];
        let body = self.call_xml(
            "POST",
            &object_path(bucket, key),
            &[("uploads", "")],
            &headers,
            Some(&[]),
        )?;
        xml::text(&body, "UploadId")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::Invalid("The service did not return an upload ID".into()))
    }

    fn complete_multipart(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (index, etag) in etags.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                index + 1,
                xml::escape(etag)
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        self.call_xml(
            "POST",
            &object_path(bucket, key),
            &[("uploadId", upload_id)],
            &[("content-type", "application/xml")],
            Some(body.as_bytes()),
        )?;
        Ok(())
    }

    fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<()> {
        self.call(
            "DELETE",
            &object_path(bucket, key),
            &[("uploadId", upload_id)],
            &[],
            None,
        )?;
        Ok(())
    }

    /// Uploads a local file, switching to a multipart upload for large files.
    pub fn upload_file(
        &self,
        bucket: &str,
        key: &str,
        path: &Path,
        progress: Progress<'_>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let content_type = content_type_for(key);
        let object_path = object_path(bucket, key);

        // Streams `length` bytes of the file starting at `offset`, retrying from
        // the same offset if the connection drops.
        let mut put_range = |offset: u64,
                             length: u64,
                             query: &[(&str, &str)],
                             headers: &[(&str, &str)]|
         -> Result<http::Response<ureq::Body>> {
            let mut attempt = 0;
            loop {
                attempt += 1;
                file.seek(SeekFrom::Start(offset))?;
                let mut reader = Tracked {
                    inner: (&mut file).take(length),
                    base: offset,
                    read: 0,
                    progress,
                    cancel,
                };
                let payload = Payload::Stream(&mut reader, length);
                match self.send("PUT", &object_path, query, headers, payload) {
                    Ok(response) => return Ok(response),
                    Err(_) if cancel.load(Ordering::Relaxed) => return Err(Error::Cancelled),
                    Err(err) if err.is_retryable() && attempt < ATTEMPTS => {
                        std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
                    }
                    Err(err) => return Err(err),
                }
            }
        };

        if size < self.tuning.multipart_threshold {
            put_range(0, size, &[], &[("content-type", content_type)])?;
            progress(size);
            return Ok(());
        }

        let part_size = self.tuning.min_part_size.max(size.div_ceil(MAX_PARTS));
        let upload_id = self.create_multipart(bucket, key, content_type)?;
        let mut upload_parts = || -> Result<Vec<String>> {
            let mut etags = Vec::new();
            let mut offset = 0;
            while offset < size {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let length = part_size.min(size - offset);
                let number = (etags.len() + 1).to_string();
                let query = [
                    ("partNumber", number.as_str()),
                    ("uploadId", upload_id.as_str()),
                ];
                let response = put_range(offset, length, &query, &[])?;
                let etag = response
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                etags.push(etag);
                offset += length;
            }
            Ok(etags)
        };
        match upload_parts()
            .and_then(|etags| self.complete_multipart(bucket, key, &upload_id, &etags))
        {
            Ok(()) => {
                progress(size);
                Ok(())
            }
            Err(err) => {
                let _ = self.abort_multipart(bucket, key, &upload_id);
                Err(err)
            }
        }
    }

    /// Downloads an object to `dest`. Data is written to a temporary file next
    /// to the destination and moved into place once complete.
    pub fn download_file(
        &self,
        bucket: &str,
        key: &str,
        dest: &Path,
        progress: Progress<'_>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut partial = dest.as_os_str().to_owned();
        partial.push(".part");
        let partial = Path::new(&partial);

        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            match self.download_to(bucket, key, partial, progress, cancel) {
                Err(err) if err.is_retryable() && attempt < ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(500 * u64::from(attempt)));
                }
                other => break other,
            }
        };
        match result {
            Ok(()) => {
                std::fs::rename(partial, dest)?;
                Ok(())
            }
            Err(err) => {
                let _ = std::fs::remove_file(partial);
                Err(err)
            }
        }
    }

    fn download_to(
        &self,
        bucket: &str,
        key: &str,
        partial: &Path,
        progress: Progress<'_>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let response = self.send("GET", &object_path(bucket, key), &[], &[], Payload::Empty)?;
        let mut reader = response.into_body().into_reader();
        let mut file = File::create(partial)?;
        let mut buffer = vec![0u8; IO_BUFFER];
        let mut written = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                // A failed read is the network's fault, not the local disk's.
                Err(err) => return Err(self.transport_error(err)),
            };
            file.write_all(&buffer[..read])?;
            written += read as u64;
            progress(written);
        }
        file.flush()?;
        Ok(())
    }

    /// Returns a time-limited URL that grants read access to one object.
    pub fn presign_get(&self, bucket: &str, key: &str, expires_secs: u64) -> String {
        let canonical_path = sigv4::uri_encode(&object_path(bucket, key), false);
        let query = sigv4::presign_query(
            &self.creds,
            &sigv4::amz_date(unix_now()),
            "GET",
            &self.host,
            &canonical_path,
            &[],
            expires_secs,
        );
        format!("{}{canonical_path}?{query}", self.base)
    }
}

/// Feeds upload progress back to the caller and aborts the stream on cancel.
struct Tracked<'a, R> {
    inner: R,
    base: u64,
    read: u64,
    progress: Progress<'a>,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Tracked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let read = self.inner.read(buf)?;
        self.read += read as u64;
        (self.progress)(self.base + self.read);
        Ok(read)
    }
}

fn object_path(bucket: &str, key: &str) -> String {
    format!("/{bucket}/{key}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn api_error(status: u16, body: &str) -> Error {
    let code = xml::text(body, "Code").unwrap_or_else(|| {
        match status {
            301 | 307 => "Redirect",
            400 => "BadRequest",
            401 => "Unauthorized",
            403 => "AccessDenied",
            404 => "NotFound",
            _ => "Error",
        }
        .to_string()
    });
    Error::Api {
        status,
        code,
        message: xml::text(body, "Message").unwrap_or_default(),
    }
}

/// Guesses a content type from a key's extension so that uploaded files are
/// served correctly from public buckets and custom domains.
pub fn content_type_for(key: &str) -> &'static str {
    let name = key.rsplit('/').next().unwrap_or(key);
    let extension = match name.rsplit_once('.') {
        Some((_, extension)) => extension.to_ascii_lowercase(),
        None => return "application/octet-stream",
    };
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "txt" | "log" | "ini" | "conf" | "cfg" => "text/plain; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "opus" => "audio/opus",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        let client = S3Client::new("abc123.r2.cloudflarestorage.com/", "id", "secret").unwrap();
        assert_eq!(client.base, "https://abc123.r2.cloudflarestorage.com");
        assert_eq!(client.host, "abc123.r2.cloudflarestorage.com");

        let client = S3Client::new("http://127.0.0.1:9000", "id", "secret").unwrap();
        assert_eq!(client.base, "http://127.0.0.1:9000");
        assert_eq!(client.host, "127.0.0.1:9000");

        assert!(S3Client::new("ftp://example.com", "id", "secret").is_err());
        assert!(S3Client::new("https://example.com/path", "id", "secret").is_err());
        assert!(S3Client::new("", "id", "secret").is_err());
    }

    #[test]
    fn presigned_urls_point_at_the_object() {
        let client = S3Client::new("abc123.r2.cloudflarestorage.com", "id", "secret").unwrap();
        let url = client.presign_get("media", "a b/c+d.png", 3600);
        assert!(url.starts_with("https://abc123.r2.cloudflarestorage.com/media/a%20b/c%2Bd.png?"));
        assert!(url.contains("X-Amz-Expires=3600"));
        assert!(url.contains("X-Amz-Signature="));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn error_text_never_contains_account_details() {
        let account = "0123456789abcdef0123456789abcdef";
        let endpoint = format!("{account}.eu.r2.cloudflarestorage.com");
        let client = S3Client::new(&endpoint, "ACCESSKEY1234", "SECRETKEY5678").unwrap();
        let text = format!(
            "dns error for {endpoint}; key ACCESSKEY1234 on account {account} signed with SECRETKEY5678"
        );
        assert_eq!(
            client.redact(&text),
            "dns error for [endpoint]; key [access key] on account [account] signed with [secret]"
        );
    }

    #[test]
    fn guesses_content_types() {
        assert_eq!(content_type_for("a/b/photo.JPG"), "image/jpeg");
        assert_eq!(content_type_for("clip.mp4"), "video/mp4");
        assert_eq!(
            content_type_for("folder.with.dots/README"),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for("archive.unknown"),
            "application/octet-stream"
        );
    }
}
