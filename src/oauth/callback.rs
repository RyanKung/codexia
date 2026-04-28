use crate::{Error, Result};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};
use url::Url;

const CALLBACK_ADDR: &str = "127.0.0.1:1455";
const MAX_REQUEST_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Outcome of waiting for the local OAuth redirect callback.
pub enum CallbackOutcome {
    /// Received a valid authorization code from the callback request.
    Code(String),
    /// No valid callback arrived before the timeout elapsed.
    TimedOut,
    /// Binding the local callback listener failed with the given message.
    BindFailed(String),
}

/// Local HTTP listener that receives the OAuth redirect callback.
pub struct CallbackServer {
    listener: TcpListener,
}

impl CallbackServer {
    /// Binds the local callback listener on the fixed OAuth redirect address.
    ///
    /// # Errors
    ///
    /// Returns an error when the local callback port cannot be bound.
    pub async fn bind() -> Result<Self> {
        let listener = TcpListener::bind(CALLBACK_ADDR).await?;
        Ok(Self { listener })
    }

    /// Waits for a callback request with the expected state and returns its outcome.
    ///
    /// # Errors
    ///
    /// Returns an error when reading callback requests or writing the browser
    /// response fails.
    pub async fn receive_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<CallbackOutcome> {
        time::timeout(
            timeout,
            receive_valid_callback(self.listener, expected_state),
        )
        .await
        .map_or_else(
            |_| Ok(CallbackOutcome::TimedOut),
            |result| result.map(CallbackOutcome::Code),
        )
    }
}

async fn receive_valid_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    for _ in 0..8 {
        let (mut stream, _) = listener.accept().await?;
        match read_callback_code(&mut stream, expected_state).await {
            Ok(code) => return Ok(code),
            Err(error) => {
                let body = oauth_error_page(&error.to_string());
                write_http_response(&mut stream, 400, &body).await?;
            }
        }
    }

    Err(Error::oauth("too many invalid callback attempts"))
}

async fn read_callback_code(stream: &mut TcpStream, expected_state: &str) -> Result<String> {
    let request = read_http_head(stream).await?;
    let target = parse_request_target(&request)?;
    let code = parse_callback_target(&target, expected_state)?;
    let body = oauth_success_page();
    write_http_response(stream, 200, &body).await?;
    Ok(code)
}

async fn read_http_head(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = vec![0_u8; MAX_REQUEST_BYTES];
    let mut read = 0;

    loop {
        let bytes = stream.read(&mut buffer[read..]).await?;
        if bytes == 0 {
            break;
        }
        read += bytes;
        if buffer[..read]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            break;
        }
        if read == MAX_REQUEST_BYTES {
            return Err(Error::oauth("callback request is too large"));
        }
    }

    String::from_utf8(buffer[..read].to_vec())
        .map_err(|_| Error::oauth("callback request is not UTF-8"))
}

fn parse_request_target(request: &str) -> Result<String> {
    let line = request
        .lines()
        .next()
        .ok_or_else(|| Error::oauth("empty callback request"))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method != "GET" || target.is_empty() {
        return Err(Error::oauth("callback request must be a GET"));
    }

    Ok(target.to_owned())
}

fn parse_callback_target(target: &str, expected_state: &str) -> Result<String> {
    let url = Url::parse(&format!("http://localhost{target}"))?;
    if url.path() != "/auth/callback" {
        return Err(Error::oauth("callback route not found"));
    }

    let state = query_value(&url, "state").ok_or_else(|| Error::oauth("missing state"))?;
    if state != expected_state {
        return Err(Error::oauth("state mismatch"));
    }

    query_value(&url, "code").ok_or_else(|| Error::oauth("missing authorization code"))
}

fn query_value(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

fn oauth_success_page() -> String {
    html_page("OpenAI authentication completed. You can close this window.")
}

fn oauth_error_page(message: &str) -> String {
    html_page(message)
}

fn html_page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Codexia OAuth</title></head><body><p>{}</p></body></html>",
        html_escape(message)
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_target() {
        let request = "GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n";

        assert_eq!(
            parse_request_target(request).unwrap(),
            "/auth/callback?code=abc&state=xyz"
        );
    }

    #[test]
    fn extracts_code_when_state_matches() {
        let code = parse_callback_target("/auth/callback?code=abc&state=xyz", "xyz").unwrap();

        assert_eq!(code, "abc");
    }

    #[test]
    fn rejects_state_mismatch() {
        let error = parse_callback_target("/auth/callback?code=abc&state=nope", "xyz").unwrap_err();

        assert!(error.to_string().contains("state mismatch"));
    }
}
