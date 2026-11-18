//! Serves object contents to the UI for image, video and audio previews.
//!
//! The webview cannot be handed a presigned URL: it would contain the account
//! ID and access key ID. Instead it gets a link to this loopback server,
//! which fetches the object with the real credentials and relays the bytes.
//! Links are random, short-lived and only reachable from this machine.

use std::collections::hash_map::RandomState;
use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::s3::{Error, S3Client};

/// Links older than the most recent ones stop working.
const MAX_TICKETS: usize = 128;
/// Headers of the object that are passed on to the webview.
const FORWARDED: [&str; 6] = [
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
];

struct Ticket {
    id: String,
    client: Arc<S3Client>,
    bucket: String,
    key: String,
}

pub struct PreviewServer {
    port: u16,
    tickets: Arc<Mutex<VecDeque<Ticket>>>,
}

/// 128 random bits as hex. `RandomState` is seeded by the OS for every
/// instance, which is enough for an unguessable loopback link.
fn random_token() -> String {
    let word = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", word(), word())
}

impl PreviewServer {
    pub fn start() -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let tickets: Arc<Mutex<VecDeque<Ticket>>> = Arc::default();

        let shared = tickets.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tickets = shared.clone();
                thread::spawn(move || {
                    let _ = serve(stream, &tickets);
                });
            }
        });
        Ok(Self { port, tickets })
    }

    /// Returns a link the webview can load to read one object.
    pub fn link(&self, client: Arc<S3Client>, bucket: &str, key: &str) -> String {
        let id = random_token();
        let mut tickets = self.tickets.lock().unwrap();
        if tickets.len() >= MAX_TICKETS {
            tickets.pop_front();
        }
        tickets.push_back(Ticket {
            id: id.clone(),
            client,
            bucket: bucket.to_string(),
            key: key.to_string(),
        });
        format!("http://127.0.0.1:{}/{id}", self.port)
    }
}

fn respond(stream: &mut TcpStream, status: u16, headers: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n{headers}cache-control: no-store\r\n\
         x-content-type-options: nosniff\r\ncontent-security-policy: sandbox\r\n\
         connection: close\r\n\r\n"
    )
}

fn serve(mut stream: TcpStream, tickets: &Mutex<VecDeque<Ticket>>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let mut range = None;
    let mut body_length = 0u64;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                body_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    // Read (and ignore) a small request body, so the client sees our answer
    // instead of a reset connection.
    io::copy(
        &mut (&mut reader).take(body_length.min(64 * 1024)),
        &mut io::sink(),
    )?;

    let target = {
        let tickets = tickets.lock().unwrap();
        tickets
            .iter()
            .find(|t| path.strip_prefix('/') == Some(t.id.as_str()))
            .map(|t| (t.client.clone(), t.bucket.clone(), t.key.clone()))
    };
    let Some((client, bucket, key)) = target.filter(|_| method == "GET") else {
        return respond(&mut stream, 404, "content-length: 0\r\n");
    };

    // Errors carry no detail: the status is all the webview needs.
    let response = match client.open(&bucket, &key, range.as_deref()) {
        Ok(response) => response,
        Err(Error::Api { status: 416, .. }) => {
            return respond(&mut stream, 416, "content-length: 0\r\n")
        }
        Err(_) => return respond(&mut stream, 404, "content-length: 0\r\n"),
    };

    let mut headers = String::new();
    for name in FORWARDED {
        if let Some(value) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
            headers.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    respond(&mut stream, response.status().as_u16(), &headers)?;

    // The webview drops the connection when it has seen enough (seeking in a
    // video, closing the preview); that simply ends the copy.
    let mut body = response.into_body().into_reader();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = body.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
    }
    stream.flush()
}
