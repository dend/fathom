//! Runs the in-memory S3 server from the test suite with some sample data, so
//! the app can be tried out without an R2 account:
//!
//! ```sh
//! cargo run --example mock_s3                 # listens on http://127.0.0.1:9321
//! cargo run --example mock_s3 -- --register   # also adds a "Mock S3" connection
//! ```
//!
//! It also answers the Cloudflare API calls Fathom makes for API tokens. Run the
//! app with `FATHOM_CLOUDFLARE_API=http://127.0.0.1:9321/client/v4` so they reach
//! this server, and the sample buckets get public URLs.
//!
//! Without `--register`, add a connection in Fathom with any account ID, the API
//! token `test-api-token` and the custom endpoint `http://127.0.0.1:9321` (under
//! Advanced). Set `FATHOM_CONFIG_DIR` to keep this away from your real settings.
//!
//! The command line tool can use it without a saved connection:
//!
//! ```sh
//! export CLOUDFLARE_ACCOUNT_ID=mock CLOUDFLARE_API_TOKEN=test-api-token
//! export FATHOM_ENDPOINT=http://127.0.0.1:9321
//! export FATHOM_CLOUDFLARE_API=http://127.0.0.1:9321/client/v4
//! cargo run --bin fth -- purge media 'photos/**/*.png' --dry-run
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use common::{MockS3, ACCESS_KEY, API_TOKEN, SECRET_KEY, TOKEN_ID};

fn svg(label: &str, from: &str, to: &str) -> Vec<u8> {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 400">
  <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0" stop-color="{from}"/><stop offset="1" stop-color="{to}"/>
  </linearGradient></defs>
  <rect width="640" height="400" fill="url(#g)"/>
  <circle cx="500" cy="110" r="54" fill="#fff" opacity=".85"/>
  <path d="M0 300 Q160 220 320 300 T640 300 V400 H0Z" fill="#fff" opacity=".25"/>
  <path d="M0 340 Q160 270 320 340 T640 340 V400 H0Z" fill="#fff" opacity=".35"/>
  <text x="36" y="72" font-family="sans-serif" font-size="40" font-weight="700" fill="#fff">{label}</text>
</svg>"##
    )
    .into_bytes()
}

/// One second of a 440 Hz tone as an 8-bit mono WAV file.
fn tone() -> Vec<u8> {
    const RATE: u32 = 8000;
    let samples: Vec<u8> = (0..RATE)
        .map(|i| {
            let phase = i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32;
            (128.0 + 90.0 * phase.sin()) as u8
        })
        .collect();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes()); // byte rate
    wav.extend_from_slice(&1u16.to_le_bytes()); // block align
    wav.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

fn seed(server: &MockS3) {
    let palettes = [
        ("Lagoon", "#22d3ee", "#1d4ed8"),
        ("Sunrise", "#fb923c", "#db2777"),
        ("Kelp", "#4ade80", "#0f766e"),
        ("Dusk", "#a78bfa", "#1e1b4b"),
        ("Coral", "#fda4af", "#be123c"),
    ];
    for (name, from, to) in palettes {
        let key = format!("photos/2024/{}.svg", name.to_lowercase());
        server.put("media", &key, &svg(name, from, to), "image/svg+xml");
    }
    server.put(
        "media",
        "photos/2023/archive/old cover (final).svg",
        &svg("Archive", "#94a3b8", "#334155"),
        "image/svg+xml",
    );
    server.put(
        "media",
        "photos/app-icon.png",
        include_bytes!("../assets/icons/128x128@2x.png"),
        "image/png",
    );
    server.put("media", "audio/tone.wav", &tone(), "audio/wav");
    server.put("media", "videos/", b"", "application/x-directory");
    server.put(
        "media",
        "notes/README.md",
        b"# Media bucket\n\nSample content served by the Fathom mock S3 server.\n\n- photos: generated SVG artwork\n- audio: a one second test tone\n- logs: thousands of small files, to exercise the virtualized list\n",
        "text/markdown; charset=utf-8",
    );
    server.put(
        "media",
        "notes/settings.json",
        b"{\n  \"theme\": \"system\",\n  \"concurrency\": 4,\n  \"domains\": [\"cdn.example.com\"]\n}\n",
        "application/json",
    );
    server.put(
        "media",
        "notes/\u{e9}t\u{e9} & more + plus.txt",
        "Unicode and punctuation in keys work too.\n".as_bytes(),
        "text/plain; charset=utf-8",
    );
    server.put(
        "media",
        "data.bin",
        &(0..=255u8).cycle().take(300_000).collect::<Vec<_>>(),
        "application/octet-stream",
    );
    for i in 0..5000 {
        let key = format!("logs/2024-{:02}/app-{i:05}.log", i % 12 + 1);
        let body = format!("{i:05} INFO request served in {} ms\n", i % 97);
        server.put("media", &key, body.as_bytes(), "text/plain; charset=utf-8");
    }

    server.put(
        "website",
        "index.html",
        b"<!doctype html>\n<html>\n  <head><title>Hello</title><link rel=\"stylesheet\" href=\"css/site.css\"></head>\n  <body><h1>Hello from R2</h1></body>\n</html>\n",
        "text/html; charset=utf-8",
    );
    server.put(
        "website",
        "css/site.css",
        b"body {\n  font-family: system-ui;\n  margin: 4rem;\n}\n",
        "text/css; charset=utf-8",
    );
    server.put(
        "website",
        "js/app.js",
        b"console.log('hello from R2');\n",
        "text/javascript; charset=utf-8",
    );
    server.put(
        "website",
        "assets/hero.svg",
        &svg("Hero", "#38bdf8", "#1d4ed8"),
        "image/svg+xml",
    );

    server.create_bucket("backups");
}

/// A gradient "photo" as PNG bytes, so image previews have something to show.
fn photo(width: u32, height: u32, from: [u8; 3], to: [u8; 3]) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let t = (x + y) as f32 / (width + height) as f32;
        let wave = ((x as f32 / 23.0).sin() * (y as f32 / 31.0).cos() * 0.08 + 1.0).min(1.0);
        image::Rgb(std::array::from_fn(|i| {
            ((from[i] as f32 + (to[i] as f32 - from[i] as f32) * t) * wave) as u8
        }))
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

/// Saves a "Mock S3" connection pointing at this server, using the app's own
/// config store (and the OS credential store for the keys). Honours
/// `FATHOM_CONFIG_DIR`, so point that at a scratch directory.
fn register(endpoint: &str) -> Result<(), String> {
    use fathom::config::{Connection, Credentials, Store};
    let store = Store::load(fathom::system::config_dir());
    let id = "mock-s3".to_string();
    // Saved the way the app saves a connection set up with an API token: the
    // token, plus the S3 keys Cloudflare derives from it.
    store.save_credentials(
        &id,
        &Credentials {
            account_id: "mock".into(),
            api_token: API_TOKEN.into(),
            access_key_id: TOKEN_ID.into(),
            secret_access_key: fathom::sigv4::sha256_hex(API_TOKEN.as_bytes()),
            endpoint: endpoint.into(),
        },
    )?;
    let mut connection = Connection::default();
    connection.id = id.clone();
    connection.name = "Mock S3".into();
    connection.custom_endpoint = true;
    connection.uses_token = true;
    connection.normalize();
    store.update(|config| {
        config.connections.retain(|c| c.id != id);
        config.connections.push(connection);
        config.active = Some(id.clone());
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = args.iter().find_map(|arg| arg.parse().ok()).unwrap_or(9321);
    let server = MockS3::start_on(port, 1000);
    seed(&server);
    for (name, from, to) in [
        ("reef.png", [20, 120, 200], [250, 200, 120]),
        ("kelp-forest.png", [10, 60, 40], [120, 220, 160]),
        ("open-water.png", [8, 30, 90], [60, 180, 230]),
    ] {
        server.put(
            "media",
            &format!("photos/2024/{name}"),
            &photo(960, 640, from, to),
            "image/png",
        );
    }
    server.add_custom_domain("media", "cdn.example.com", true);
    server.set_r2_dev("website", "pub-6e8b1fd3a2c94e05.r2.dev", true);

    if args.iter().any(|arg| arg == "--register") {
        match register(&server.endpoint) {
            Ok(()) => println!(
                "Registered the \"Mock S3\" connection in {}",
                fathom::system::config_dir().display()
            ),
            Err(error) => eprintln!("Could not register the connection: {error}"),
        }
    }

    println!("Mock S3 server listening on {}", server.endpoint);
    println!("  API token:         {API_TOKEN}");
    println!("  access key ID:     {ACCESS_KEY}");
    println!("  secret access key: {SECRET_KEY}");
    println!(
        "Run Fathom with FATHOM_CLOUDFLARE_API={}/client/v4 to look up public domains here.",
        server.endpoint
    );
    println!("Press Ctrl+C to stop.");
    loop {
        std::thread::park();
    }
}
