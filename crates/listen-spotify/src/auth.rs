//! Spotify sign-in: OAuth 2 authorization code flow with PKCE (no client secret), the flow Spotify
//! recommends for desktop apps. Spotify sends the browser back to a loopback address, where a
//! one-shot HTTP listener picks up the code.
//!
//! The user brings their own client id (an app on developer.spotify.com with
//! [`REDIRECT_URI`] as its redirect URI): Spotify gives every app its own user quota.

use base64::Engine;
use listen_source::SourceError;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const REDIRECT_PORT: u16 = 8898;
/// The redirect URI to register on the Spotify app (Spotify only allows loopback IPs, not `localhost`).
pub const REDIRECT_URI: &str = "http://127.0.0.1:8898/callback";
/// Where the user creates the app that gives them a client id.
pub const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";
pub const SCOPES: &str = "user-read-private user-library-read user-library-modify playlist-read-private \
                          playlist-read-collaborative playlist-modify-private playlist-modify-public \
                          user-follow-read user-follow-modify user-read-recently-played user-top-read";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub(crate) const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// How long the browser has to come back.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// What a sign-in that was replaced or cancelled returns (not an error to show).
pub const SUPERSEDED: &str = "Spotify sign-in replaced by a newer one.";

/// The sign-in waiting for the browser, to stop when another starts or the user cancels.
static PENDING: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> = std::sync::Mutex::new(None);

/// Stops a sign-in that is waiting for the browser (it returns [`SUPERSEDED`]).
pub fn cancel_sign_in() {
    if let Some(cancel) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = cancel.send(());
    }
}

/// What is kept in the keyring for a Spotify account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    pub client_id: String,
    pub refresh_token: String,
}

impl Secret {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str::<Self>(s)
            .ok()
            .filter(|s| !s.client_id.is_empty() && !s.refresh_token.is_empty())
    }
}

/// The PKCE pair: the verifier stays here, its hash goes to Spotify.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

fn random_string(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| UNRESERVED[rng.random_range(0..UNRESERVED.len())] as char).collect()
}

pub fn challenge_of(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

pub fn pkce() -> Pkce {
    let verifier = random_string(64);
    Pkce { challenge: challenge_of(&verifier), verifier }
}

pub fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge)
        .append_pair("scope", SCOPES)
        .append_pair("state", state);
    url.into()
}

/// A token response.
#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Spotify may hand out a new refresh token; the old one then stops working.
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default = "default_expiry")]
    pub expires_in: u64,
}

fn default_expiry() -> u64 {
    3600
}

#[derive(Debug, Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)]) -> Result<Tokens, SourceError> {
    let resp =
        http.post(TOKEN_URL).form(form).send().await.map_err(|e| SourceError::Network(e.to_string()))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| SourceError::Network(e.to_string()))?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| SourceError::Parse(format!("token: {e}")));
    }
    let message = serde_json::from_str::<TokenError>(&body)
        .map(|e| match e.error_description {
            Some(d) => format!("{} ({d})", e.error),
            None => e.error,
        })
        .unwrap_or_else(|_| format!("HTTP {status}"));
    if status.as_u16() == 400 || status.as_u16() == 401 {
        Err(SourceError::Auth(format!("Spotify refused the sign-in: {message}")))
    } else {
        Err(SourceError::Server { code: status.as_u16() as i32, message })
    }
}

pub async fn exchange(
    http: &reqwest::Client,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> Result<Tokens, SourceError> {
    token_request(
        http,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ],
    )
    .await
}

pub async fn refresh(
    http: &reqwest::Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<Tokens, SourceError> {
    token_request(
        http,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", client_id)],
    )
    .await
}

/// What came back to the redirect URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Code {
        code: String,
        state: String,
    },
    /// The user declined, or Spotify refused (`error` parameter).
    Denied(String),
    /// Some other request (the browser's favicon).
    Other,
}

/// Reads the request line (`GET /callback?code=…&state=… HTTP/1.1`).
pub fn parse_callback(request_line: &str) -> Callback {
    let Some(target) = request_line.split_whitespace().nth(1) else { return Callback::Other };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else { return Callback::Other };
    if url.path() != "/callback" {
        return Callback::Other;
    }
    let get = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    if let Some(error) = get("error") {
        return Callback::Denied(error);
    }
    match (get("code"), get("state")) {
        (Some(code), Some(state)) => Callback::Code { code, state },
        _ => Callback::Other,
    }
}

fn page(title: &str, text: &str) -> String {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Listen</title>\
         <body style=\"font-family:system-ui,sans-serif;text-align:center;padding-top:15vh\">\
         <h2>{title}</h2><p>{text}</p></body>"
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Reads the head of one request from the browser.
async fn read_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).lines().next().unwrap_or_default().to_string())
}

/// Signs in: opens Spotify's consent page with `open` and waits for the browser to come back.
pub async fn sign_in(client_id: &str, open: impl FnOnce(&str)) -> Result<Secret, String> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err("Enter the client ID of your Spotify app.".into());
    }
    // A sign-in still waiting (its browser tab never came back) gives the port up for this one.
    cancel_sign_in();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let mut tries = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).await {
            Ok(l) => break l,
            Err(_) if tries < 30 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(format!(
                    "Could not listen on port {REDIRECT_PORT} for Spotify's answer ({e}); another program uses it."
                ));
            }
        }
    };
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel_tx);
    let pkce = pkce();
    let state = random_string(24);
    open(&authorize_url(client_id, &pkce.challenge, &state));

    let wait = async {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let line = read_request(&mut stream).await.unwrap_or_default();
            let (reply, result) = match parse_callback(&line) {
                Callback::Other => {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    continue;
                }
                Callback::Denied(error) => (
                    page("Spotify sign-in cancelled", "You can close this tab."),
                    Err(if error == "access_denied" {
                        "Spotify sign-in was cancelled.".to_string()
                    } else {
                        format!(
                            "Spotify refused the sign-in ({error}). Check the client ID and the redirect URI."
                        )
                    }),
                ),
                Callback::Code { state: got, .. } if got != state => (
                    page("Sign-in failed", "Start it again from Listen."),
                    Err("Spotify's answer did not match this sign-in.".into()),
                ),
                Callback::Code { code, .. } => {
                    (page("Signed in to Spotify", "You can close this tab and go back to Listen."), Ok(code))
                }
            };
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
            return result;
        }
    };
    let code = tokio::select! {
        r = tokio::time::timeout(SIGN_IN_TIMEOUT, wait) => r.map_err(|_| {
            "Spotify did not send the browser back within 5 minutes. If Spotify showed an error page, check \
             that the redirect URI in your Spotify app is exactly http://127.0.0.1:8898/callback."
                .to_string()
        })?,
        _ = cancel_rx => Err(SUPERSEDED.to_string()),
    };
    drop(listener);
    let code = code?;

    let http = reqwest::Client::new();
    let tokens = exchange(&http, client_id, &code, &pkce.verifier).await.map_err(|e| e.to_string())?;
    let refresh_token = tokens.refresh_token.ok_or("Spotify sent no refresh token.")?;
    Ok(Secret { client_id: client_id.to_string(), refresh_token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_rfc_7636_example() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let p = pkce();
        assert_eq!(p.verifier.len(), 64);
        assert_eq!(p.challenge, challenge_of(&p.verifier));
    }

    #[test]
    fn authorize_url_carries_the_pkce_parameters() {
        let url = url::Url::parse(&authorize_url("abc", "chal", "st")).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "abc");
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["state"], "st");
        assert!(q["scope"].contains("user-library-read"));
    }

    #[test]
    fn reads_the_callback() {
        assert_eq!(
            parse_callback("GET /callback?code=AQB%2Fx&state=s1 HTTP/1.1"),
            Callback::Code { code: "AQB/x".into(), state: "s1".into() }
        );
        assert_eq!(
            parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1"),
            Callback::Denied("access_denied".into())
        );
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1"), Callback::Other);
        assert_eq!(parse_callback(""), Callback::Other);
    }

    #[test]
    fn secret_round_trips() {
        let s = Secret { client_id: "c".into(), refresh_token: "r".into() };
        assert_eq!(Secret::from_json(&s.to_json()), Some(s));
        assert_eq!(Secret::from_json("not json"), None);
        assert_eq!(Secret::from_json(r#"{"client_id":"","refresh_token":"r"}"#), None);
    }

    #[tokio::test]
    async fn a_new_sign_in_replaces_one_still_waiting() {
        let first = tokio::spawn(sign_in("client", |_| {}));
        // Until the first one holds the port.
        while PENDING.lock().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second = tokio::spawn(sign_in("client", |_| {}));
        assert_eq!(first.await.unwrap(), Err(SUPERSEDED.to_string()));
        // The second one registers once it holds the port; cancel it then.
        while !second.is_finished() {
            cancel_sign_in();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(second.await.unwrap(), Err(SUPERSEDED.to_string()));
    }

    #[tokio::test]
    async fn sign_in_hands_the_code_over_and_rejects_a_wrong_state() {
        // The browser comes back with the wrong state: the sign-in fails without a token request.
        let task = tokio::spawn(sign_in("client", |_| {}));
        let mut stream = loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", REDIRECT_PORT)).await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        stream.write_all(b"GET /callback?code=c&state=wrong HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"));
        assert_eq!(task.await.unwrap(), Err("Spotify's answer did not match this sign-in.".to_string()));
    }
}
