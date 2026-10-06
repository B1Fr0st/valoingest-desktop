//! Browser sign-in brokered by the Valoingest Worker.
//!
//! The app opens `/v1/auth/desktop/start` in the system browser with a
//! loopback port, a random state and a PKCE challenge. After Google sign-in
//! the Worker redirects to `http://127.0.0.1:<port>/callback` with a one-time
//! code, which only this process can redeem because only it holds the PKCE
//! verifier. No Google client ID or secret is needed here.

use crate::api::{self, Api, ApiError, DesktopSession};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(300);

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    getrandom::fill(&mut buffer).expect("operating system randomness is unavailable");
    URL_SAFE_NO_PAD.encode(buffer)
}

pub fn pkce() -> Pkce {
    let verifier = random_token(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

pub fn start_url(api: &str, port: u16, state: &str, challenge: &str) -> String {
    format!(
        "{api}/v1/auth/desktop/start?port={port}&state={}&code_challenge={}&code_challenge_method=S256",
        api::encode(state),
        api::encode(challenge)
    )
}

/// Outcome of one request to the loopback listener.
#[derive(Debug, PartialEq)]
pub enum Callback {
    Code(String),
    WrongState,
    Ignored,
}

/// Parses `GET /callback?code=..&state=.. HTTP/1.1`.
pub fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignored;
    };
    let Some(query) = target.strip_prefix("/callback?") else {
        return Callback::Ignored;
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", value)) => code = Some(value.to_owned()),
            Some(("state", value)) => state = Some(value.to_owned()),
            _ => {}
        }
    }
    let state_ok = state
        .as_deref()
        .is_some_and(|state| constant_time_eq(state.as_bytes(), expected_state.as_bytes()));
    match (code, state_ok) {
        (Some(code), true)
            if code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Callback::Code(code)
        }
        _ => Callback::WrongState,
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn respond(mut stream: TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Valoingest</title>\
         <body style=\"font:16px system-ui;background:#0f1923;color:#ece8e1;display:grid;place-items:center;height:90vh\">\
         <p>{message}</p></body>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
}

/// Runs the whole sign-in. `open` launches the system browser.
pub fn sign_in(
    api: &Api,
    open: impl Fn(&str),
    cancel: &AtomicBool,
) -> Result<DesktopSession, String> {
    // Loopback only, on an OS-assigned port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("could not listen for sign-in: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let state = random_token(32);
    let pkce = pkce();
    open(&start_url(api.base(), port, &state, &pkce.challenge));

    let deadline = Instant::now() + TIMEOUT;
    let code = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".into());
        }
        if Instant::now() > deadline {
            return Err("sign-in timed out".into());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                match parse_callback(&line, &state) {
                    Callback::Code(code) => {
                        respond(
                            stream,
                            "200 OK",
                            "Signed in to Valoingest. You can close this tab.",
                        );
                        break code;
                    }
                    Callback::WrongState => {
                        respond(
                            stream,
                            "400 Bad Request",
                            "This sign-in link is not for this app. Try again from the tray.",
                        );
                    }
                    Callback::Ignored => respond(stream, "404 Not Found", "Not found."),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150))
            }
            Err(error) => return Err(format!("sign-in listener failed: {error}")),
        }
    };
    api.desktop_token(&code, &pkce.verifier)
        .map_err(|error| match error {
            ApiError::Unauthorized => "the sign-in code expired; please try again".to_owned(),
            other => other.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_shape() {
        let pair = pkce();
        assert_eq!(pair.challenge.len(), 43);
        assert!((43..=128).contains(&pair.verifier.len()));
        assert_eq!(
            pair.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pair.verifier.as_bytes()))
        );
    }

    #[test]
    fn callback_requires_matching_state() {
        assert_eq!(
            parse_callback("GET /callback?code=abc_123&state=s1 HTTP/1.1\r\n", "s1"),
            Callback::Code("abc_123".into())
        );
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=s2 HTTP/1.1", "s1"),
            Callback::WrongState
        );
        assert_eq!(
            parse_callback("GET /callback?code=abc HTTP/1.1", "s1"),
            Callback::WrongState
        );
        assert_eq!(
            parse_callback("GET /callback?code=a%3Cb&state=s1 HTTP/1.1", "s1"),
            Callback::WrongState
        );
        assert_eq!(
            parse_callback("GET /favicon.ico HTTP/1.1", "s1"),
            Callback::Ignored
        );
        assert_eq!(
            parse_callback("POST /callback?code=a&state=s1 HTTP/1.1", "s1"),
            Callback::Ignored
        );
    }

    #[test]
    fn start_url_carries_only_port_state_and_challenge() {
        let url = start_url("https://example.test", 53001, "st", "ch");
        assert_eq!(
            url,
            "https://example.test/v1/auth/desktop/start?port=53001&state=st&code_challenge=ch&code_challenge_method=S256"
        );
    }
}
