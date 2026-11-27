//! L2 probes under §10.2's rules, and the HTTPS fetcher they use.

use std::time::Duration;

use openchef_runtime::BoxFuture;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url, redirect};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::providers::{Credentials, Login};
use crate::state::{Observation, Source};

/// The time limit of one probe request.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a response body a probe reads.
const MAX_BODY: usize = 1 << 20;
/// The shortest time between two probe runs.
pub const MIN_INTERVAL: Duration = Duration::from_secs(30);
/// The first backoff after a 429; it doubles on each further 429.
const BACKOFF_ON_429: Duration = Duration::from_mins(5);
/// The longest 429 backoff.
const MAX_BACKOFF_ON_429: Duration = Duration::from_hours(1);
/// How many server errors in a row mute the probe.
const MUTE_AFTER_5XX: u32 = 3;
/// How long repeated server errors or a rejected login mute the probe.
const MUTE_FOR: Duration = Duration::from_mins(30);

/// Performs one probe request and returns the body of a 200 response; any other status
/// is [`Error::Status`], which the [`Prober`] turns into backoff.
///
/// Asynchronous because the real fetcher does network I/O; a boxed future keeps the
/// trait object-safe so a prober can hold the real fetcher or a test's fake alike.
pub trait Fetcher: Send + Sync {
    /// Fetches the body; stops early with [`Error::Cancelled`] when `cancel` fires.
    fn fetch(&self, cancel: CancellationToken) -> BoxFuture<'_, Result<Vec<u8>, Error>>;
}

/// Turns a response body observed at a time into an observation, or reports a shape
/// change ([`Error::ShapeChanged`]).
pub type Parser = fn(&[u8], OffsetDateTime) -> Result<Observation, Error>;

/// Builds the provider-specific headers (authorization) from a login.
pub(crate) type AuthHeaders = fn(&Login) -> Result<HeaderMap, Error>;

/// A fetcher for one fixed `https` URL. Redirects are refused, so the token can never
/// be sent anywhere but the provider's own host; errors never contain the request
/// headers.
pub struct HttpsFetcher {
    url: Url,
    host: String,
    client: Client,
    credentials: Box<dyn Credentials>,
    auth: AuthHeaders,
}

impl HttpsFetcher {
    /// A fetcher for `url`, authorized from `credentials` at each request (logins are
    /// read fresh every time, never cached).
    pub(crate) fn new(
        url: &str,
        credentials: Box<dyn Credentials>,
        auth: AuthHeaders,
    ) -> Result<Self, Error> {
        let url = parse_https(url)?;
        let client = client_builder().https_only(true).build();
        Self::with_client(url, client, credentials, auth)
    }

    fn with_client(
        url: Url,
        client: reqwest::Result<Client>,
        credentials: Box<dyn Credentials>,
        auth: AuthHeaders,
    ) -> Result<Self, Error> {
        let host = url.host_str().unwrap_or_default().to_owned();
        let client = client.map_err(|e| Error::Request {
            host: host.clone(),
            reason: describe(&e),
        })?;
        Ok(Self {
            url,
            host,
            client,
            credentials,
            auth,
        })
    }

    async fn get(&self) -> Result<Vec<u8>, Error> {
        let login = self.credentials.read().await?;
        let mut headers = (self.auth)(&login)?;
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let mut resp = self
            .client
            .get(self.url.clone())
            .headers(headers)
            .send()
            .await
            .map_err(|e| self.request_error(&e))?;
        if resp.status() != StatusCode::OK {
            return Err(Error::Status {
                host: self.host.clone(),
                code: resp.status().as_u16(),
            });
        }
        // Read at most MAX_BODY bytes, as Go's LimitReader did: a longer body is cut
        // off and then fails validation like any other unexpected shape.
        let mut body = Vec::new();
        while body.len() < MAX_BODY {
            let Some(chunk) = resp.chunk().await.map_err(|e| self.request_error(&e))? else {
                break;
            };
            let room = MAX_BODY - body.len();
            body.extend_from_slice(chunk.get(..room).unwrap_or(&chunk));
        }
        Ok(body)
    }

    fn request_error(&self, e: &reqwest::Error) -> Error {
        Error::Request {
            host: self.host.clone(),
            reason: describe(e),
        }
    }
}

impl Fetcher for HttpsFetcher {
    fn fetch(&self, cancel: CancellationToken) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            tokio::select! {
                () = cancel.cancelled() => Err(Error::Cancelled { host: self.host.clone() }),
                r = self.get() => r,
            }
        })
    }
}

/// The client settings every probe shares: no redirects, a hard timeout, our user agent.
fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .redirect(redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .user_agent("openchef")
}

fn parse_https(raw: &str) -> Result<Url, Error> {
    let invalid = |reason: &str| Error::InvalidUrl {
        url: raw.to_owned(),
        reason: reason.to_owned(),
    };
    let url = Url::parse(raw).map_err(|e| invalid(&e.to_string()))?;
    if url.scheme() != "https" {
        return Err(invalid("scheme is not https"));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid("no host"));
    }
    Ok(url)
}

/// A reqwest error with its causes, without the URL (it is in the error's host field)
/// and never with headers, which reqwest errors do not carry.
fn describe(e: &reqwest::Error) -> String {
    let mut out = String::new();
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        let text = err.to_string();
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&text);
        cur = err.source();
    }
    if let Some(url) = e.url() {
        out = out.replace(url.as_str(), "<probe url>");
    }
    out
}

/// One L2 probe under §10.2's rules: a minimum interval, backoff on 429, muting after
/// repeated server errors or a rejected login, a kill switch, and self-disabling when
/// the response changes shape.
///
/// A plain state machine: the quota actor owns it and passes `now`, so there is no
/// clock and no lock here.
pub struct Prober {
    pool: String,
    name: String,
    fetcher: Box<dyn Fetcher>,
    parse: Parser,
    enabled: bool,
    /// The shape-change error that turned the probe off until restart.
    disabled: Option<Error>,
    last: Option<OffsetDateTime>,
    muted_until: Option<OffsetDateTime>,
    fails_5xx: u32,
    backoff: Option<Duration>,
}

impl Prober {
    /// A probe of `pool` called `name`, enabled.
    #[must_use]
    pub fn new(
        pool: impl Into<String>,
        name: impl Into<String>,
        fetcher: Box<dyn Fetcher>,
        parse: Parser,
    ) -> Self {
        Self {
            pool: pool.into(),
            name: name.into(),
            fetcher,
            parse,
            enabled: true,
            disabled: None,
            last: None,
            muted_until: None,
            fails_5xx: 0,
            backoff: None,
        }
    }

    /// The pool that receives the observations.
    #[must_use]
    pub fn pool(&self) -> &str {
        &self.pool
    }

    /// The probe's name ("claude", "chatgpt").
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The kill switch (`openchef quota probes off`, config `quota.probes`). It does not
    /// revive a probe disabled by a shape change.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether the probe can run at `now` and, if not, why (for `openchef doctor`).
    /// The minimum interval is not reported: it only throttles manual refreshes.
    pub fn status(&self, now: OffsetDateTime) -> Result<(), Error> {
        if let Some(e) = &self.disabled {
            return Err(e.clone());
        }
        if !self.enabled {
            return Err(Error::Disabled);
        }
        match self.muted_until {
            Some(until) if now < until => Err(Error::Muted { until }),
            Some(_) | None => Ok(()),
        }
    }

    /// Performs the probe at `now` unless it is off, muted, or ran less than
    /// [`MIN_INTERVAL`] ago. Failed attempts count for the interval too.
    pub async fn run(
        &mut self,
        now: OffsetDateTime,
        cancel: CancellationToken,
    ) -> Result<Observation, Error> {
        self.status(now)?;
        if let Some(last) = self.last {
            let until = last + MIN_INTERVAL;
            if now < until {
                return Err(Error::Muted { until });
            }
        }
        self.last = Some(now);

        let body = match self.fetcher.fetch(cancel).await {
            Ok(body) => body,
            Err(e) => {
                self.failed(&e, now);
                return Err(e);
            }
        };
        let mut obs = match (self.parse)(&body, now) {
            Ok(obs) => obs,
            Err(e) => {
                let e = self.shape_changed(e);
                self.disabled = Some(e.clone());
                return Err(e);
            }
        };
        self.fails_5xx = 0;
        self.backoff = None;
        obs.pool.clone_from(&self.pool);
        obs.source = Source::Probe;
        Ok(obs)
    }

    /// Names this probe in a parser's error; any parser error is a shape change.
    fn shape_changed(&self, e: Error) -> Error {
        let reason = match e {
            Error::ShapeChanged { reason, .. } => reason,
            other @ (Error::NoCredentials { .. }
            | Error::ReadCredentials { .. }
            | Error::Muted { .. }
            | Error::Disabled
            | Error::Status { .. }
            | Error::Request { .. }
            | Error::InvalidUrl { .. }
            | Error::Cancelled { .. }) => other.to_string(),
        };
        Error::ShapeChanged {
            probe: self.name.clone(),
            reason,
        }
    }

    /// Applies backoff for a failed fetch. Only HTTP statuses change the schedule;
    /// a missing login or a network error just fails this run.
    ///
    /// `now` is the run's start rather than the response time (Go read the clock
    /// again): the difference is at most the 10 s request timeout.
    fn failed(&mut self, e: &Error, now: OffsetDateTime) {
        let Error::Status { code, .. } = *e else {
            return;
        };
        match code {
            429 => {
                let backoff = self
                    .backoff
                    .map_or(BACKOFF_ON_429, |b| (2 * b).min(MAX_BACKOFF_ON_429));
                self.backoff = Some(backoff);
                self.muted_until = Some(now + backoff);
            }
            500.. => {
                self.fails_5xx += 1;
                if self.fails_5xx >= MUTE_AFTER_5XX {
                    self.muted_until = Some(now + MUTE_FOR);
                    self.fails_5xx = 0;
                }
            }
            // The stored login expired or was revoked; OpenChef never refreshes tokens.
            401 | 403 => self.muted_until = Some(now + MUTE_FOR),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{Arc, Mutex};

    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::providers::{Login, Token};

    struct Fixed;

    impl Credentials for Fixed {
        fn read(&self) -> BoxFuture<'_, Result<Login, Error>> {
            Box::pin(async {
                Ok(Login {
                    token: Token::new("sekret-token".to_owned()),
                    account: None,
                })
            })
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "the AuthHeaders signature")]
    fn bearer(login: &Login) -> Result<HeaderMap, Error> {
        let mut h = HeaderMap::new();
        let v = HeaderValue::from_str(&format!("Bearer {}", login.token.expose())).unwrap();
        h.insert(AUTHORIZATION, v);
        Ok(h)
    }

    #[test]
    fn refuses_urls_that_are_not_https() {
        for url in [
            "http://api.anthropic.com/api/oauth/usage",
            "ftp://api.anthropic.com/x",
            "https://",
            "not a url",
            "/api/oauth/usage",
        ] {
            let got = HttpsFetcher::new(url, Box::new(Fixed), bearer);
            assert!(
                matches!(got, Err(Error::InvalidUrl { .. })),
                "{url} accepted"
            );
        }
        assert!(
            HttpsFetcher::new(
                "https://api.anthropic.com/api/oauth/usage",
                Box::new(Fixed),
                bearer
            )
            .is_ok()
        );
    }

    /// A one-request plain-HTTP server on loopback: it records the request head and
    /// answers with `response`. Plain HTTP because the test has no certificate; the
    /// fetcher's https-only rule is covered above, this covers headers and redirects.
    async fn serve_once(response: String) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            *record.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        });
        (format!("http://{addr}/api/oauth/usage"), seen)
    }

    fn loopback_fetcher(url: &str) -> HttpsFetcher {
        let client = client_builder().no_proxy().build();
        HttpsFetcher::with_client(Url::parse(url).unwrap(), client, Box::new(Fixed), bearer)
            .unwrap()
    }

    #[tokio::test]
    async fn sends_the_login_and_user_agent() {
        let body = r#"{"ok": true}"#;
        let (url, seen) = serve_once(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let got = loopback_fetcher(&url)
            .fetch(CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(got, body.as_bytes());
        let head = seen.lock().unwrap().to_lowercase();
        assert!(
            head.contains("authorization: bearer sekret-token"),
            "{head}"
        );
        assert!(head.contains("user-agent: openchef"), "{head}");
        assert!(head.contains("accept: application/json"), "{head}");
    }

    // A redirect is never followed, so the token cannot leave the provider's host.
    #[tokio::test]
    async fn refuses_redirects() {
        let (target, reached) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_owned()).await;
        let (url, _) = serve_once(format!(
            "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;
        let got = loopback_fetcher(&url).fetch(CancellationToken::new()).await;
        assert!(
            matches!(got, Err(Error::Status { code: 302, .. })),
            "{got:?}"
        );
        assert!(
            reached.lock().unwrap().is_empty(),
            "request reached the redirect target"
        );
    }

    #[tokio::test]
    async fn errors_do_not_leak_the_token() {
        let (url, _) =
            serve_once("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n".to_owned())
                .await;
        let err = loopback_fetcher(&url)
            .fetch(CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { code: 429, .. }));
        assert!(!err.to_string().contains("sekret"), "{err}");
        assert!(!format!("{err:?}").contains("sekret"), "{err:?}");
    }

    #[tokio::test]
    async fn cancellation_stops_the_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        // Accept and never answer, so only cancellation can end the fetch.
        tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let got = loopback_fetcher(&url).fetch(cancel).await;
        assert!(matches!(got, Err(Error::Cancelled { .. })), "{got:?}");
    }
}
