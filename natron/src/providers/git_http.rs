//! HTTP for the `git` provider: gix's smart-HTTP transport on top of
//! Natron's `ureq` agent.
//!
//! gix ships HTTP backends for reqwest and curl. Using the agent Natron
//! already has for downloads keeps one HTTP and TLS stack in the binary,
//! with the same proxy and trust-store behaviour as every other download.
//!
//! A fetch is one `GET <repo>/info/refs` and one or two small
//! `POST <repo>/git-upload-pack` requests, the last of which answers with
//! the pack. Request bodies are buffered in memory (they list what we want,
//! a few hundred bytes); responses are streamed.

use gix::protocol::transport::client::blocking_io::http::{GetResponse, Http, PostBodyDataKind, PostResponse};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ureq::ResponseExt;

/// Connecting includes the TLS handshake. Nothing bounds the transfer itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

type BodyReader = BufReader<Box<dyn Read + Send + Sync + 'static>>;

#[derive(Default)]
pub struct UreqHttp {
    /// Set when the first GET was redirected: the repository's new base URL,
    /// which gix then uses for the POSTs (as git does).
    redirected_base_url: Option<String>,
}

fn err(msg: String) -> gix_error::Error {
    gix_error::Error::from_error(std::io::Error::other(msg))
}

/// `"Name: value"` lines, as gix hands them over.
fn split_header(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once(':')?;
    Some((name.trim(), value.trim()))
}

/// Response headers as `Name: value` lines, the form gix reads them in.
fn header_lines(res: &ureq::http::Response<ureq::Body>) -> Cursor<Vec<u8>> {
    let mut out = Vec::new();
    for (name, value) in res.headers() {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    Cursor::new(out)
}

fn check_status(url: &str, res: &ureq::http::Response<ureq::Body>) -> gix_error::Result<()> {
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let hint = match status.as_u16() {
        401 | 403 | 404 => " (is the repository public and the URL right?)",
        _ => "",
    };
    Err(err(format!("HTTP status {status} for {url}{hint}")))
}

fn body_reader(res: ureq::http::Response<ureq::Body>) -> BodyReader {
    BufReader::new(Box::new(res.into_body().into_reader()))
}

impl Http for UreqHttp {
    type Headers = PostHeaders;
    type ResponseBody = PostBody;
    type PostBody = PostWriter;

    fn get(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> gix_error::Result<GetResponse<Self::Headers, Self::ResponseBody>> {
        let mut req = crate::download::agent()
            .get(url)
            .config()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            .build();
        for h in headers {
            if let Some((name, value)) = split_header(h.as_ref()) {
                req = req.header(name, value);
            }
        }
        let res = req.call().map_err(|e| err(format!("GET {url}: {e}")))?;
        check_status(url, &res)?;

        // A moved repository: remember where it went, so the POSTs go there
        // directly. `url` is `base_url` plus a tail; so is the final URL.
        let final_url = res.get_uri().to_string();
        if final_url != url {
            let tail = url.strip_prefix(base_url).unwrap_or("");
            match final_url.strip_suffix(tail) {
                Some(new_base) if !tail.is_empty() => self.redirected_base_url = Some(new_base.to_string()),
                _ => return Err(err(format!("GET {url}: unexpected redirect to {final_url}"))),
            }
        }

        Ok(GetResponse {
            headers: PostHeaders(Lazy::ready_headers(header_lines(&res))),
            body: PostBody(Lazy::ready_body(body_reader(res))),
        })
    }

    fn post(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        _body: PostBodyDataKind,
    ) -> gix_error::Result<PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>> {
        // gix writes the request body after this returns, then reads the
        // response. So the request is sent on the first read.
        let request = Arc::new(Mutex::new(Exchange::Pending {
            url: url.to_string(),
            headers: headers.into_iter().map(|h| h.as_ref().to_string()).collect(),
            body: Vec::new(),
        }));
        Ok(PostResponse {
            post_body: PostWriter(request.clone()),
            headers: PostHeaders(Lazy::new(request.clone())),
            body: PostBody(Lazy::new(request)),
        })
    }

    fn configure(&mut self, _config: &dyn std::any::Any) -> gix_error::Result<()> {
        Ok(())
    }

    fn redirected_base_url(&self) -> Option<String> {
        self.redirected_base_url.clone()
    }
}

/// One POST: the request while gix is still writing it, then the response.
enum Exchange {
    Pending { url: String, headers: Vec<String>, body: Vec<u8> },
    Done { headers: Option<Cursor<Vec<u8>>>, body: Option<BodyReader> },
    Failed(String),
}

impl Exchange {
    /// Sends the request if that hasn't happened yet.
    fn send(&mut self) -> std::io::Result<()> {
        let Exchange::Pending { url, headers, body } = self else {
            return match self {
                Exchange::Failed(msg) => Err(std::io::Error::other(msg.clone())),
                _ => Ok(()),
            };
        };
        // No redirects: git only follows them on the first GET.
        let mut req = crate::download::agent()
            .post(url.as_str())
            .config()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        for h in headers.iter() {
            if let Some((name, value)) = split_header(h) {
                req = req.header(name, value);
            }
        }
        let result = req
            .send(&body[..])
            .map_err(|e| format!("POST {url}: {e}"))
            .and_then(|res| match check_status(url, &res) {
                Ok(()) => Ok(res),
                Err(e) => Err(e.to_string()),
            });
        match result {
            Ok(res) => {
                *self = Exchange::Done { headers: Some(header_lines(&res)), body: Some(body_reader(res)) };
                Ok(())
            }
            Err(msg) => {
                *self = Exchange::Failed(msg.clone());
                Err(std::io::Error::other(msg))
            }
        }
    }
}

pub struct PostWriter(Arc<Mutex<Exchange>>);

impl Write for PostWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut *self.0.lock().expect("no panic while locked") {
            Exchange::Pending { body, .. } => {
                body.extend_from_slice(buf);
                Ok(buf.len())
            }
            _ => Err(std::io::Error::other("request body written after the request was sent")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One half of a response (headers or body), taken out of the shared
/// [`Exchange`] on first use.
struct Lazy<T> {
    exchange: Option<Arc<Mutex<Exchange>>>,
    value: Option<T>,
}

impl<T> Lazy<T> {
    fn new(exchange: Arc<Mutex<Exchange>>) -> Self {
        Lazy { exchange: Some(exchange), value: None }
    }

    fn get(&mut self, take: impl FnOnce(&mut Exchange) -> Option<T>) -> std::io::Result<&mut T> {
        if self.value.is_none() {
            let exchange = self.exchange.take().ok_or_else(|| std::io::Error::other("response already read"))?;
            let mut exchange = exchange.lock().expect("no panic while locked");
            exchange.send()?;
            self.value = take(&mut exchange);
        }
        self.value.as_mut().ok_or_else(|| std::io::Error::other("response already read"))
    }
}

impl Lazy<Cursor<Vec<u8>>> {
    fn ready_headers(headers: Cursor<Vec<u8>>) -> Self {
        Lazy { exchange: None, value: Some(headers) }
    }

    fn headers(&mut self) -> std::io::Result<&mut Cursor<Vec<u8>>> {
        self.get(|e| match e {
            Exchange::Done { headers, .. } => headers.take(),
            _ => None,
        })
    }
}

impl Lazy<BodyReader> {
    fn ready_body(body: BodyReader) -> Self {
        Lazy { exchange: None, value: Some(body) }
    }

    fn body(&mut self) -> std::io::Result<&mut BodyReader> {
        self.get(|e| match e {
            Exchange::Done { body, .. } => body.take(),
            _ => None,
        })
    }
}

pub struct PostHeaders(Lazy<Cursor<Vec<u8>>>);

impl Read for PostHeaders {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.headers()?.read(buf)
    }
}

impl BufRead for PostHeaders {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.headers()?.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        if let Some(h) = self.0.value.as_mut() {
            h.consume(amt);
        }
    }
}

pub struct PostBody(Lazy<BodyReader>);

impl Read for PostBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.body()?.read(buf)
    }
}

impl BufRead for PostBody {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.body()?.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        if let Some(b) = self.0.value.as_mut() {
            b.consume(amt);
        }
    }
}
