//! A tiny in-memory S3 server, just faithful enough to exercise the client
//! end to end without credentials or network access.
//!
//! It verifies SigV4 signatures against what actually arrived on the wire, so
//! a client that signs one thing and sends another fails here the same way it
//! would against R2.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use fathom::sigv4::{self, Credentials};
use fathom::xml;

pub const ACCESS_KEY: &str = "test-access-key";
pub const SECRET_KEY: &str = "test-secret-key";
/// A Cloudflare API token, and the ID the API reports for it. Requests signed
/// with the S3 keys derived from it are accepted too.
pub const API_TOKEN: &str = "test-api-token";
pub const TOKEN_ID: &str = "test-token-id";

#[derive(Clone)]
pub struct StoredObject {
    pub data: Vec<u8>,
    pub content_type: String,
}

#[derive(Default)]
struct State {
    /// bucket -> key -> object
    buckets: BTreeMap<String, BTreeMap<String, StoredObject>>,
    /// upload ID -> (content type, part number -> data)
    uploads: HashMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    /// bucket -> custom domains as (domain, ownership verified)
    custom_domains: HashMap<String, Vec<(String, bool)>>,
    /// bucket -> (r2.dev domain, public access on)
    managed_domains: HashMap<String, (String, bool)>,
    /// `METHOD path?query` of every request served.
    log: Vec<String>,
}

pub struct MockS3 {
    pub endpoint: String,
    state: Arc<Mutex<State>>,
    page_size: usize,
}

struct HttpRequest {
    method: String,
    raw_path: String,
    raw_query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn xml(status: u16, body: String) -> Self {
        Self::new(status)
            .header("content-type", "application/xml")
            .body(body.into_bytes())
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::xml(
            status,
            format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>"),
        )
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

impl MockS3 {
    /// Starts a server on a random local port.
    pub fn start() -> Self {
        Self::start_on(0, 1000)
    }

    /// Starts a server on `port` (0 picks one) that lists at most `page_size`
    /// entries per page.
    pub fn start_on(port: u16, page_size: usize) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock S3 server");
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));

        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                let host = address.to_string();
                thread::spawn(move || serve(stream, &state, &host, page_size));
            }
        });

        Self {
            endpoint: format!("http://{address}"),
            state,
            page_size,
        }
    }

    pub fn create_bucket(&self, name: &str) {
        self.state
            .lock()
            .unwrap()
            .buckets
            .entry(name.to_string())
            .or_default();
    }

    pub fn put(&self, bucket: &str, key: &str, data: &[u8], content_type: &str) {
        self.state
            .lock()
            .unwrap()
            .buckets
            .entry(bucket.to_string())
            .or_default()
            .insert(
                key.to_string(),
                StoredObject {
                    data: data.to_vec(),
                    content_type: content_type.to_string(),
                },
            );
    }

    pub fn get(&self, bucket: &str, key: &str) -> Option<StoredObject> {
        self.state
            .lock()
            .unwrap()
            .buckets
            .get(bucket)?
            .get(key)
            .cloned()
    }

    pub fn keys(&self, bucket: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .buckets
            .get(bucket)
            .map(|objects| objects.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Attaches a custom domain to a bucket, as the Cloudflare API reports it.
    pub fn add_custom_domain(&self, bucket: &str, domain: &str, active: bool) {
        self.state
            .lock()
            .unwrap()
            .custom_domains
            .entry(bucket.to_string())
            .or_default()
            .push((domain.to_string(), active));
    }

    /// Sets a bucket's r2.dev address and whether public access is on there.
    pub fn set_r2_dev(&self, bucket: &str, domain: &str, enabled: bool) {
        self.state
            .lock()
            .unwrap()
            .managed_domains
            .insert(bucket.to_string(), (domain.to_string(), enabled));
    }

    pub fn pending_uploads(&self) -> usize {
        self.state.lock().unwrap().uploads.len()
    }

    pub fn request_log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>, host: &str, page_size: usize) {
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;

    while let Some(request) = read_request(&mut reader) {
        let is_head = request.method == "HEAD";
        let response = if request.raw_path.starts_with("/client/v4/") {
            handle_api(&request, state)
        } else {
            match check_signature(&request, host) {
                Ok(()) => handle(&request, state, page_size),
                Err(reason) => HttpResponse::error(403, "SignatureDoesNotMatch", &reason),
            }
        };

        let reason = match response.status {
            200 => "OK",
            204 => "No Content",
            206 => "Partial Content",
            _ => "Status",
        };
        let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
        let mut has_length = false;
        for (name, value) in &response.headers {
            has_length |= name == "content-length";
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !has_length {
            head.push_str(&format!("content-length: {}\r\n", response.body.len()));
        }
        head.push_str("\r\n");

        if writer.write_all(head.as_bytes()).is_err() {
            return;
        }
        if !is_head && writer.write_all(&response.body).is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<HttpRequest> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    // The client must always announce a length; S3 rejects chunked uploads.
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;

    Some(HttpRequest {
        method,
        raw_path,
        raw_query,
        headers,
        body,
    })
}

fn query_pairs(raw_query: &str) -> Vec<(String, String)> {
    raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                sigv4::uri_decode(name, false),
                sigv4::uri_decode(value, false),
            )
        })
        .collect()
}

/// The few Cloudflare API routes Fathom uses: verifying a token and reading a
/// bucket's domains.
fn handle_api(request: &HttpRequest, state: &Mutex<State>) -> HttpResponse {
    let mut state = state.lock().unwrap();
    state
        .log
        .push(format!("{} {}", request.method, request.raw_path));
    let json = |status: u16, body: serde_json::Value| {
        HttpResponse::new(status)
            .header("content-type", "application/json")
            .body(body.to_string().into_bytes())
    };
    let failure = |status: u16, code: u32, message: &str| {
        json(
            status,
            serde_json::json!({"success": false, "errors": [{"code": code, "message": message}], "messages": [], "result": null}),
        )
    };
    let success = |result: serde_json::Value| {
        json(
            200,
            serde_json::json!({"success": true, "errors": [], "messages": [], "result": result}),
        )
    };

    if request.headers.get("authorization").map(String::as_str)
        != Some(&format!("Bearer {API_TOKEN}"))
    {
        return failure(401, 1000, "Invalid API Token");
    }
    let path = request.raw_path.trim_start_matches("/client/v4/");
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        ["accounts", _, "tokens", "verify"] | ["user", "tokens", "verify"] => {
            success(serde_json::json!({"id": TOKEN_ID, "status": "active"}))
        }
        ["accounts", _, "r2", "buckets", bucket, "domains", "custom"] => {
            let domains: Vec<serde_json::Value> = state
                .custom_domains
                .get(*bucket)
                .into_iter()
                .flatten()
                .map(|(domain, active)| {
                    let status = if *active { "active" } else { "pending" };
                    serde_json::json!({"domain": domain, "enabled": true, "status": {"ownership": status, "ssl": status}, "minTLS": "1.0"})
                })
                .collect();
            success(serde_json::json!({"domains": domains}))
        }
        ["accounts", _, "r2", "buckets", bucket, "domains", "managed"] => {
            let (domain, enabled) = state
                .managed_domains
                .get(*bucket)
                .cloned()
                .unwrap_or_else(|| (format!("pub-{bucket}.r2.dev"), false));
            success(serde_json::json!({"bucketId": bucket, "domain": domain, "enabled": enabled}))
        }
        _ => failure(404, 7003, "No route for that URI"),
    }
}

/// Accepts requests signed with the fixed test keys, or with the keys Cloudflare
/// derives from the test API token.
fn check_signature(request: &HttpRequest, host: &str) -> Result<(), String> {
    let token_keys = Credentials {
        access_key_id: TOKEN_ID.to_string(),
        secret_access_key: sigv4::sha256_hex(API_TOKEN.as_bytes()),
        region: "auto".to_string(),
    };
    let fixed_keys = Credentials {
        access_key_id: ACCESS_KEY.to_string(),
        secret_access_key: SECRET_KEY.to_string(),
        region: "auto".to_string(),
    };
    check_signature_with(request, host, &token_keys)
        .or_else(|_| check_signature_with(request, host, &fixed_keys))
}

fn check_signature_with(
    request: &HttpRequest,
    host: &str,
    creds: &Credentials,
) -> Result<(), String> {
    let pairs = query_pairs(&request.raw_query);

    // Presigned URL: the signature travels in the query string.
    if let Some((_, given)) = pairs.iter().find(|(name, _)| name == "X-Amz-Signature") {
        let value = |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let extra: Vec<(&str, &str)> = pairs
            .iter()
            .filter(|(name, _)| !name.starts_with("X-Amz-"))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let expected = sigv4::presign_query(
            creds,
            &value("X-Amz-Date"),
            &request.method,
            host,
            &request.raw_path,
            &extra,
            value("X-Amz-Expires").parse().unwrap_or(0),
        );
        return if expected.ends_with(&format!("X-Amz-Signature={given}")) {
            Ok(())
        } else {
            Err("presigned signature mismatch".into())
        };
    }

    let authorization = request
        .headers
        .get("authorization")
        .ok_or("missing Authorization header")?;
    let signed_names = authorization
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .ok_or("missing SignedHeaders")?;
    let mut signed = Vec::new();
    for name in signed_names.split(';') {
        let value = request
            .headers
            .get(name)
            .ok_or_else(|| format!("signed header {name} was not sent"))?;
        signed.push((name.to_string(), value.clone()));
    }
    if request.headers.get("host").map(String::as_str) != Some(host) {
        return Err("unexpected Host header".into());
    }

    let pairs_ref: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let payload_hash = request
        .headers
        .get("x-amz-content-sha256")
        .ok_or("missing x-amz-content-sha256")?;
    if payload_hash != sigv4::UNSIGNED_PAYLOAD && *payload_hash != sigv4::sha256_hex(&request.body)
    {
        return Err("payload hash mismatch".into());
    }
    let expected = sigv4::authorization(
        creds,
        request
            .headers
            .get("x-amz-date")
            .ok_or("missing x-amz-date")?,
        &request.method,
        &request.raw_path,
        &sigv4::canonical_query(&pairs_ref),
        &signed,
        payload_hash,
    );
    if &expected == authorization {
        Ok(())
    } else {
        Err("signature mismatch".into())
    }
}

fn etag(data: &[u8]) -> String {
    format!("\"{}\"", &sigv4::sha256_hex(data)[..32])
}

fn parse_range(header: &str, length: usize) -> Option<(usize, usize)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: usize = start.parse().ok()?;
    let end: usize = match end {
        "" => length.checked_sub(1)?,
        end => end.parse::<usize>().ok()?.min(length.checked_sub(1)?),
    };
    (start <= end).then_some((start, end))
}

fn handle(request: &HttpRequest, state: &Mutex<State>, page_size: usize) -> HttpResponse {
    static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

    let mut state = state.lock().unwrap();
    state.log.push(format!(
        "{} {}{}{}",
        request.method,
        request.raw_path,
        if request.raw_query.is_empty() {
            ""
        } else {
            "?"
        },
        request.raw_query
    ));

    let path = sigv4::uri_decode(&request.raw_path, false);
    let query: HashMap<String, String> = query_pairs(&request.raw_query).into_iter().collect();
    let (bucket, key) = match path.trim_start_matches('/').split_once('/') {
        Some((bucket, key)) => (bucket.to_string(), key.to_string()),
        None => (path.trim_start_matches('/').to_string(), String::new()),
    };
    let method = request.method.as_str();

    if bucket.is_empty() {
        let buckets: String = state
            .buckets
            .keys()
            .map(|name| {
                format!(
                    "<Bucket><Name>{}</Name><CreationDate>2024-05-01T10:00:00.000Z</CreationDate></Bucket>",
                    xml::escape(name)
                )
            })
            .collect();
        return HttpResponse::xml(
            200,
            format!(
                "<ListAllMyBucketsResult><Buckets>{buckets}</Buckets></ListAllMyBucketsResult>"
            ),
        );
    }
    if !state.buckets.contains_key(&bucket) {
        return HttpResponse::error(404, "NoSuchBucket", "The specified bucket does not exist.");
    }

    if key.is_empty() {
        return match method {
            "GET" => list_objects(&state.buckets[&bucket], &bucket, &query, page_size),
            _ => HttpResponse::error(405, "MethodNotAllowed", "Unsupported bucket operation."),
        };
    }

    let upload_id = query.get("uploadId").cloned();
    match method {
        "HEAD" | "GET" => {
            let Some(object) = state.buckets[&bucket].get(&key) else {
                return HttpResponse::error(404, "NoSuchKey", "The specified key does not exist.");
            };
            let base = HttpResponse::new(200)
                .header("content-type", &object.content_type)
                .header("etag", &etag(&object.data))
                .header("last-modified", "Wed, 01 May 2024 10:00:00 GMT")
                .header("accept-ranges", "bytes");
            if method == "HEAD" {
                return base.header("content-length", &object.data.len().to_string());
            }
            match request.headers.get("range") {
                None => base.body(object.data.clone()),
                Some(range) => match parse_range(range, object.data.len()) {
                    Some((start, end)) => {
                        let mut response = base
                            .header(
                                "content-range",
                                &format!("bytes {start}-{end}/{}", object.data.len()),
                            )
                            .body(object.data[start..=end].to_vec());
                        response.status = 206;
                        response
                    }
                    None => HttpResponse::error(
                        416,
                        "InvalidRange",
                        "The requested range is not satisfiable.",
                    ),
                },
            }
        }
        "PUT" => {
            let data = match request.headers.get("x-amz-copy-source") {
                None => request.body.clone(),
                Some(source) => {
                    let source = sigv4::uri_decode(source, false);
                    let Some((src_bucket, src_key)) =
                        source.trim_start_matches('/').split_once('/')
                    else {
                        return HttpResponse::error(400, "InvalidArgument", "Bad copy source.");
                    };
                    let Some(object) = state.buckets.get(src_bucket).and_then(|b| b.get(src_key))
                    else {
                        return HttpResponse::error(
                            404,
                            "NoSuchKey",
                            "The copy source does not exist.",
                        );
                    };
                    let data = match request.headers.get("x-amz-copy-source-range") {
                        None => object.data.clone(),
                        Some(range) => match parse_range(range, object.data.len()) {
                            Some((start, end)) => object.data[start..=end].to_vec(),
                            None => {
                                return HttpResponse::error(416, "InvalidRange", "Bad copy range.")
                            }
                        },
                    };
                    if upload_id.is_none() {
                        let copy = StoredObject {
                            data,
                            content_type: object.content_type.clone(),
                        };
                        let tag = etag(&copy.data);
                        state.buckets.get_mut(&bucket).unwrap().insert(key, copy);
                        return HttpResponse::xml(
                            200,
                            format!(
                                "<CopyObjectResult><ETag>{}</ETag></CopyObjectResult>",
                                xml::escape(&tag)
                            ),
                        );
                    }
                    data
                }
            };
            let tag = etag(&data);
            match upload_id {
                Some(upload_id) => {
                    let number: u32 = query
                        .get("partNumber")
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    let Some((_, parts)) = state.uploads.get_mut(&upload_id) else {
                        return HttpResponse::error(
                            404,
                            "NoSuchUpload",
                            "The upload does not exist.",
                        );
                    };
                    parts.insert(number, data);
                    if request.headers.contains_key("x-amz-copy-source") {
                        HttpResponse::xml(
                            200,
                            format!(
                                "<CopyPartResult><ETag>{}</ETag></CopyPartResult>",
                                xml::escape(&tag)
                            ),
                        )
                    } else {
                        HttpResponse::new(200).header("etag", &tag)
                    }
                }
                None => {
                    let content_type = request
                        .headers
                        .get("content-type")
                        .cloned()
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    state
                        .buckets
                        .get_mut(&bucket)
                        .unwrap()
                        .insert(key, StoredObject { data, content_type });
                    HttpResponse::new(200).header("etag", &tag)
                }
            }
        }
        "POST" if query.contains_key("uploads") => {
            let id = format!("upload-{}", NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed));
            let content_type = request
                .headers
                .get("content-type")
                .cloned()
                .unwrap_or_default();
            state
                .uploads
                .insert(id.clone(), (content_type, BTreeMap::new()));
            HttpResponse::xml(
                200,
                format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"),
            )
        }
        "POST" => {
            let Some((content_type, parts)) = upload_id.and_then(|id| state.uploads.remove(&id))
            else {
                return HttpResponse::error(404, "NoSuchUpload", "The upload does not exist.");
            };
            // Every part listed by the client must exist with the ETag it reported.
            let body = String::from_utf8_lossy(&request.body).into_owned();
            let mut data = Vec::new();
            for part in xml::elements(&body, "Part") {
                let number: u32 = xml::text(part, "PartNumber")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                match parts.get(&number) {
                    Some(bytes) if Some(etag(bytes)) == xml::text(part, "ETag") => {
                        data.extend_from_slice(bytes)
                    }
                    _ => {
                        return HttpResponse::error(
                            400,
                            "InvalidPart",
                            "A part is missing or has the wrong ETag.",
                        )
                    }
                }
            }
            state
                .buckets
                .get_mut(&bucket)
                .unwrap()
                .insert(key, StoredObject { data, content_type });
            HttpResponse::xml(
                200,
                "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>".into(),
            )
        }
        "DELETE" => {
            match upload_id {
                Some(id) => {
                    state.uploads.remove(&id);
                }
                None => {
                    state.buckets.get_mut(&bucket).unwrap().remove(&key);
                }
            }
            HttpResponse::new(204)
        }
        _ => HttpResponse::error(405, "MethodNotAllowed", "Unsupported operation."),
    }
}

fn list_objects(
    objects: &BTreeMap<String, StoredObject>,
    bucket: &str,
    query: &HashMap<String, String>,
    page_size: usize,
) -> HttpResponse {
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let delimiter = query.get("delimiter").cloned().unwrap_or_default();
    let after = query.get("continuation-token").cloned().unwrap_or_default();
    let encode = query.get("encoding-type").map(String::as_str) == Some("url");
    let show = |value: &str| {
        if encode {
            sigv4::uri_encode(value, false)
        } else {
            xml::escape(value)
        }
    };

    // Entries in key order: either an object or a rolled-up common prefix.
    let mut entries: Vec<(String, Option<&StoredObject>)> = Vec::new();
    for (key, object) in objects.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        let rest = &key[prefix.len()..];
        let entry = match (!delimiter.is_empty())
            .then(|| rest.find(&delimiter))
            .flatten()
        {
            Some(index) => (
                format!("{prefix}{}", &rest[..index + delimiter.len()]),
                None,
            ),
            None => (key.clone(), Some(object)),
        };
        if entries.last().map(|(name, _)| name) != Some(&entry.0) {
            entries.push(entry);
        }
    }

    let remaining: Vec<_> = entries
        .into_iter()
        .filter(|(name, _)| *name > after)
        .collect();
    let truncated = remaining.len() > page_size;
    let page = &remaining[..remaining.len().min(page_size)];

    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>{}</Name><Prefix>{}</Prefix>",
        xml::escape(bucket),
        show(&prefix)
    );
    if encode {
        body.push_str("<EncodingType>url</EncodingType>");
    }
    body.push_str(&format!(
        "<KeyCount>{}</KeyCount><IsTruncated>{truncated}</IsTruncated>",
        page.len()
    ));
    if truncated {
        let token = &page.last().unwrap().0;
        body.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml::escape(token)
        ));
    }
    for (name, object) in page {
        match object {
            Some(object) => body.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>2024-05-01T10:00:00.000Z</LastModified>\
                 <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                show(name),
                xml::escape(&etag(&object.data)),
                object.data.len()
            )),
            None => body.push_str(&format!(
                "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                show(name)
            )),
        }
    }
    body.push_str("</ListBucketResult>");
    HttpResponse::xml(200, body)
}
