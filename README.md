# Stenosaurus

A self-hosted, single-user dictation site. Speak into the page; the transcript
appears while you talk, and the final text is saved as a plain text file.

Speech recognition runs entirely in the browser: microphone audio is captured
as raw PCM, resampled to 16 kHz mono, and streamed to Whistle running as
WebAssembly in a module worker (via the pinned `cactus-needle` browser package).
Audio never leaves the browser. The Rust server only receives the final text,
stores it, and serves the 16.9 MB model to authenticated clients.

No account registration, no email, no database: one username/password in
`data/auth.toml`, and one file per dictation.

## Requirements

- Linux x86-64 (glibc), Rust 1.85+
- Node 22+ (only to run the JavaScript unit tests)
- `curl` and `tar` for the asset setup script
- A Chromium-class browser with WebAssembly and module workers

## Setup

```sh
./scripts/setup-whistle.sh      # vendors the engine + model, verifies sha256
cargo build --release
./target/release/stenosaurus user set --username alice
./target/release/stenosaurus serve
```

Open <http://127.0.0.1:3000/> and sign in.

`setup-whistle.sh` downloads pinned artifacts and verifies them:

- `cactus-needle@0.3.0` from npm (tarball sha256 checked)
- `needle.wasm` 0.3.0 (sha256 checked)
- `whistle.cact` model revision `b358dda` (sha256 checked)

Generated assets are **not** committed: `vendor/` and `static/vendor/` are in
`.gitignore`. Re-run the script after a fresh clone. The first dictation in a
browser downloads about 17.8 MB (engine + model) from your own server into
CacheStorage; later sessions load from cache.

## Configuration

|Variable|Flag|Default|
|---|---|---|
|`STENO_DATA_DIR`|`--data-dir`|`data`|
|`STENO_BIND`|`serve --bind`|`127.0.0.1:3000`|
|`STENO_MODEL`|`serve --model`|`vendor/whistle.cact`|
|`STENO_STATIC_DIR`|`serve --static-dir`|`static`|
|`STENO_SECURE_COOKIES`|`serve --secure-cookies`|off|
|`STENO_USERNAME`|`user set --username`|required|
|`RUST_LOG`|–|`stenosaurus=info,tower_http=warn`|

`user set` prompts for the password twice on a terminal; when stdin is piped it
reads one line (`printf 'password\n' | stenosaurus user set --username alice`).
Restart the server after changing the account.

## Data and backups

```
data/
├── auth.toml                 # username + Argon2id PHC hash (0600)
└── dictations/
    └── 2026-10-20T12-34-56.123456Z.txt
```

Filenames are UTC timestamps; the file content is the transcript plus a trailing
newline. Back up `data/dictations/*.txt` (and `data/auth.toml`). Delete a file to
delete a dictation. The directory is the source of truth; there is no index to
rebuild.

Sessions are in memory, so restarting the server signs everyone out.

## HTTPS and phones

Microphone capture and WASM integrity checks require a secure context: HTTPS or
`http://localhost`. To use the app from a phone, put it behind a TLS reverse
proxy (Caddy, nginx) and run the server with `--secure-cookies` so the session
cookie is marked `Secure`. The app itself serves plain HTTP only; it warns at
startup when bound to a non-loopback address without that flag.

Example Caddyfile:

```
dictation.example.com {
    reverse_proxy 127.0.0.1:3000
}
```

The server sets a strict CSP (`script-src 'self' 'wasm-unsafe-eval';
worker-src 'self'; connect-src 'self'`), `X-Content-Type-Options: nosniff`, and
`Referrer-Policy: same-origin`. The model route requires a session, so the
browser worker fetches it with same-origin credentials.

## Tests

```sh
cargo test                                # storage, auth, and HTTP integration tests
node --test tests/pcm.test.mjs            # resampler, accumulator, async queue
```

## Browser support

The vendored `cactus-needle` browser build documents Chromium as tested; Safari
and iOS are unverified. The site is mobile-first, but the acceptance browser is
Chromium-class on desktop. Search is server-rendered: the index page has a
search box, and results highlight matching text and work without JavaScript.

## License

Stenosaurus is licensed under the GNU General Public License v3.0; see
[LICENSE](LICENSE). Vendored Whistle assets retain their upstream licenses:
`cactus-needle` is Apache-2.0, and the script copies its `LICENSE` and `NOTICE`
into `static/vendor/cactus-needle-0.3.0/`.
