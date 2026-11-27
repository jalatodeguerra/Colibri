//! `http`: a small blocking HTTP/1.1 client behind the `net` capability, on `ureq` with
//! rustls and the bundled webpki roots (`https://` works with no system TLS library).
//!
//! * `http.get(url)`, `http.post(url, body)`, `http.post_as(url, body, content_type)` and the
//!   general `http.request(method, url, headers, body)` return `Result[Response]`.
//! * A response with any status is an `ok`: `404` is data (`response.status`), not an error.
//!   An `err` means no response arrived: a bad URL, a refused connection, a TLS failure, a
//!   timeout, a body over the size limit.
//! * `Response` is a struct: `status: int`, `headers: Map[str, str]` (names lowercased; a
//!   header sent several times is joined with `, `, except `set-cookie`, whose values are
//!   joined with `\n`: a cookie's `Expires` date holds a comma, so `, ` cannot be split
//!   apart again, and a header value never holds a newline) and `body: str`. A body that is not valid
//!   UTF-8 is decoded lossily (invalid sequences become U+FFFD). Parse JSON with
//!   `json.decode[T](response.body)`.
//! * For a body that is not text there is a second set of calls, with the body as `Bytes` in
//!   both directions (like `fs.read_bytes` and `fs.write_bytes`): `http.get_bytes(url)` and the
//!   general `http.request_bytes(method, url, headers, data)` return
//!   `Result[BytesResponse]`, a struct with the same `status` and `headers` and `body: Bytes`,
//!   exactly as it arrived; no text copy is made. `http.to_text(response)` turns a
//!   `BytesResponse` into a `Response` (lossy), for a text answer to an upload.
//! * Connections are kept and reused: every request goes through one `ureq::Agent`.
//! * Redirects are followed (up to 10). The proxy variables of the process (`HTTP_PROXY`,
//!   `HTTPS_PROXY`, `NO_PROXY`) are honoured.
//! * Timeouts: 30 seconds for a whole request by default; `http.set_timeout(seconds)` changes
//!   it for the rest of the program (`0` means no timeout).
//!
//! A call runs the request on a background thread (`Ctx::run_async`, ARCHITECTURE §22). Inside
//! a coroutine only that coroutine waits: the others keep running and several requests
//! overlap. On the main fiber the call runs the event loop while it waits, so coroutines
//! started earlier keep running there too. Cancelling the waiting coroutine abandons the
//! request (the thread finishes on its own and its result is dropped).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use colibri_runtime::{Bytes, Cap, Ctx, Error, TypedError, Vm};

use crate::errors::error_kind;
use crate::needs;
use crate::values::value_struct;

error_kind!(
    "http",
    /// Why a request failed (`http.Error.Timeout`). A response with any status is not an
    /// error, so there is no variant for one.
    Kind {
        Timeout,
        Connect,
        Dns,
        Tls,
        InvalidUrl,
        TooLarge,
        Other,
    }
);

/// The variant of `http.Error` a failure of the request is.
fn classify(e: &ureq::Error) -> Kind {
    use std::io::ErrorKind;
    match e {
        ureq::Error::Timeout(_) => Kind::Timeout,
        ureq::Error::HostNotFound => Kind::Dns,
        ureq::Error::ConnectionFailed => Kind::Connect,
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => Kind::Tls,
        ureq::Error::BadUri(_) | ureq::Error::Http(_) | ureq::Error::RequireHttpsOnly(_) => {
            Kind::InvalidUrl
        }
        ureq::Error::BodyExceedsLimit(_) | ureq::Error::LargeResponseHeader(..) => Kind::TooLarge,
        ureq::Error::Io(io) if is_tls(io) => Kind::Tls,
        ureq::Error::Io(io) if is_lookup_failure(io) => Kind::Dns,
        ureq::Error::Io(io) => match io.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Kind::Timeout,
            ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::AddrNotAvailable
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof => Kind::Connect,
            _ => Kind::Other,
        },
        _ => Kind::Other,
    }
}

/// Whether an I/O error is a failed name lookup. ureq turns a failed `to_socket_addrs()` into
/// `Error::Io` (it only reports `HostNotFound` for a lookup that succeeds with no address), and
/// the standard library gives that error no kind of its own (`Uncategorized`): only its message
/// tells, and the message starts with this fixed text on the platforms that use `getaddrinfo`
/// ("failed to lookup address information: Name or service not known").
fn is_lookup_failure(e: &std::io::Error) -> bool {
    e.to_string()
        .starts_with("failed to lookup address information")
}

/// The variant of `http.Error` a failure to build the request is. Only a URL that does not parse
/// is `InvalidUrl`; a bad method, header name or header value is `Other`.
fn classify_build(e: &ureq::http::Error) -> Kind {
    use ureq::http::uri::{InvalidUri, InvalidUriParts};
    if e.is::<InvalidUri>() || e.is::<InvalidUriParts>() {
        Kind::InvalidUrl
    } else {
        Kind::Other
    }
}

/// Whether an I/O error is a TLS failure that the connection wrapped in it.
fn is_tls(e: &std::io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
}

/// Reading the body fails with an `io::Error` that may wrap the `ureq::Error` behind it.
fn classify_io(e: &std::io::Error) -> Kind {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
    {
        Some(inner) => classify(inner),
        None if is_tls(e) => Kind::Tls,
        None => match e.kind() {
            std::io::ErrorKind::TimedOut => Kind::Timeout,
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof => Kind::Connect,
            _ => Kind::Other,
        },
    }
}

/// The default for a whole request, from connecting to the last byte of the body.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest response body read, in bytes.
const MAX_BODY: u64 = 64 << 20;

/// What `http.set_timeout` changes.
struct HttpState {
    timeout: Option<Duration>,
}

/// What a request got back, before it is given a shape for the script.
struct Raw {
    status: i64,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// The answer to `http.get`, `http.post`, `http.post_as` and `http.request`.
#[derive(Clone)]
struct Response {
    status: i64,
    headers: BTreeMap<String, String>,
    body: String,
}

value_struct!(Response {
    status: i64,
    headers: BTreeMap<String, String>,
    body: String,
});

/// The answer to `http.get_bytes` and `http.request_bytes`: the body as it arrived.
#[derive(Clone)]
struct BytesResponse {
    status: i64,
    headers: BTreeMap<String, String>,
    body: Bytes,
}

value_struct!(BytesResponse {
    status: i64,
    headers: BTreeMap<String, String>,
    body: Bytes,
});

impl From<Raw> for Response {
    fn from(raw: Raw) -> Response {
        Response {
            status: raw.status,
            headers: raw.headers,
            body: String::from_utf8_lossy(&raw.body).into_owned(),
        }
    }
}

impl From<Raw> for BytesResponse {
    fn from(raw: Raw) -> BytesResponse {
        BytesResponse {
            status: raw.status,
            headers: raw.headers,
            body: Bytes(raw.body),
        }
    }
}

/// The agent every request goes through, so connections to the same host are kept and reused.
/// It has no timeout of its own: each request sets it.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(None)
            // A 404 is a response, not a failure.
            .http_status_as_error(false)
            .build()
            .into()
    })
}

fn perform(
    timeout: Option<Duration>,
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<Vec<u8>>,
) -> Result<Raw, TypedError> {
    let fail = |kind: Kind, e: &dyn std::fmt::Display| {
        kind.error(format!("http: {method} {url} failed: {e}"))
    };
    let mut builder = ureq::http::Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(body.unwrap_or_default())
        .map_err(|e| fail(classify_build(&e), &e))?;
    // The timeout is the request's own, so one agent serves every setting.
    let agent = agent();
    let request = agent
        .configure_request(request)
        .timeout_global(timeout)
        .build();
    let mut response = agent.run(request).map_err(|e| fail(classify(&e), &e))?;
    let status = i64::from(response.status().as_u16());
    let mut out_headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        let name = name.as_str().to_ascii_lowercase();
        // Joining with a comma is how HTTP says "the same header several times", except for
        // `Set-Cookie`, which is not a list: its values have commas of their own.
        let separator = if name == "set-cookie" { "\n" } else { ", " };
        out_headers
            .entry(name)
            .and_modify(|old| {
                old.push_str(separator);
                old.push_str(&value);
            })
            .or_insert(value);
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(classify_io(&e), &e))?;
    if bytes.len() as u64 > MAX_BODY {
        return Err(fail(
            Kind::TooLarge,
            &format!("the response body is over {} MiB", MAX_BODY >> 20),
        ));
    }
    Ok(Raw {
        status,
        headers: out_headers,
        body: bytes,
    })
}

fn run(
    cx: &mut Ctx,
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<Vec<u8>>,
) -> Result<Raw, TypedError> {
    let timeout = cx
        .state_mut::<HttpState>()
        .map_or(Some(DEFAULT_TIMEOUT), |s| s.timeout);
    let (method, url, headers) = (method.to_string(), url.to_string(), headers.clone());
    // On a background thread: inside a coroutine only this coroutine waits.
    let raw = cx.run_async(move || perform(timeout, &method, &url, &headers, body))?;
    // The body was read on another thread, outside the `Vm`'s account; it is about to be copied
    // into a Colibri value.
    cx.reserve_memory(raw.body.len().saturating_mul(2));
    Ok(raw)
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    Kind::declare(vm)?;
    vm.insert_state(HttpState {
        timeout: Some(DEFAULT_TIMEOUT),
    });
    let mut m = vm.std_module("http");
    // The text calls: the body goes in as text and comes back as lossy text.
    needs(
        Cap::Net,
        m.register_fn_named("get", ["url"], |cx: &mut Ctx, url: String| {
            run(cx, "GET", &url, &BTreeMap::new(), None).map(Response::from)
        }),
    )?;
    needs(
        Cap::Net,
        m.register_fn_named(
            "post",
            ["url", "body"],
            |cx: &mut Ctx, url: String, body: String| {
                let headers = content_type("text/plain; charset=utf-8");
                run(cx, "POST", &url, &headers, Some(body.into_bytes())).map(Response::from)
            },
        ),
    )?;
    needs(
        Cap::Net,
        m.register_fn_named(
            "post_as",
            ["url", "body", "content_type"],
            |cx: &mut Ctx, url: String, body: String, kind: String| {
                run(
                    cx,
                    "POST",
                    &url,
                    &content_type(&kind),
                    Some(body.into_bytes()),
                )
                .map(Response::from)
            },
        ),
    )?;
    needs(
        Cap::Net,
        m.register_fn_named(
            "request",
            ["method", "url", "headers", "body"],
            |cx: &mut Ctx,
             method: String,
             url: String,
             headers: BTreeMap<String, String>,
             body: String| {
                let method = check_method(&method)?;
                let body = if body.is_empty() {
                    None
                } else {
                    Some(body.into_bytes())
                };
                run(cx, &method, &url, &headers, body).map(Response::from)
            },
        ),
    )?;
    // The `Bytes` calls: the body goes in and comes back as it is, and no text copy is made.
    needs(
        Cap::Net,
        m.register_fn_named("get_bytes", ["url"], |cx: &mut Ctx, url: String| {
            run(cx, "GET", &url, &BTreeMap::new(), None).map(BytesResponse::from)
        }),
    )?;
    // Like `request`, with a body that is not text (an empty `Bytes` sends no body).
    needs(
        Cap::Net,
        m.register_fn_named(
            "request_bytes",
            ["method", "url", "headers", "data"],
            |cx: &mut Ctx,
             method: String,
             url: String,
             headers: BTreeMap<String, String>,
             data: Bytes| {
                let method = check_method(&method)?;
                let body = if data.is_empty() { None } else { Some(data.0) };
                run(cx, &method, &url, &headers, body).map(BytesResponse::from)
            },
        ),
    )?;
    // A `BytesResponse` as a `Response`: the body decoded as text, lossily.
    needs(
        Cap::Net,
        m.register_fn_named("to_text", ["response"], |response: BytesResponse| {
            Response {
                status: response.status,
                headers: response.headers,
                body: String::from_utf8_lossy(&response.body).into_owned(),
            }
        }),
    )?;
    // Gated like the rest of the module, so `import http` never needs a second look.
    needs(
        Cap::Net,
        m.register_fn_named("set_timeout", ["seconds"], |cx: &mut Ctx, seconds: f64| {
            if !seconds.is_finite() || seconds < 0.0 {
                cx.raise(format!(
                    "http.set_timeout: {seconds} is not a number of seconds (it must be finite and not negative; 0 means no timeout)"
                ));
            }
            let timeout = (seconds > 0.0)
                .then(|| Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX));
            if let Some(state) = cx.state_mut::<HttpState>() {
                state.timeout = timeout;
            }
        }),
    )?;
    Ok(())
}

/// The method in upper case, or an error when it is not a method name.
fn check_method(method: &str) -> Result<String, TypedError> {
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(Kind::Other.error(format!("http: `{method}` is not an HTTP method")));
    }
    Ok(method.to_ascii_uppercase())
}

fn content_type(kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("content-type".to_string(), kind.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_name_lookup_is_dns() {
        // What ureq returns when `to_socket_addrs()` fails (no real lookup in a test).
        let lookup = ureq::Error::Io(std::io::Error::other(
            "failed to lookup address information: Name or service not known",
        ));
        assert_eq!(classify(&lookup), Kind::Dns);
        assert_eq!(classify(&ureq::Error::HostNotFound), Kind::Dns);
        // Other I/O failures keep their kinds.
        let refused = ureq::Error::Io(std::io::ErrorKind::ConnectionRefused.into());
        assert_eq!(classify(&refused), Kind::Connect);
        let other = ureq::Error::Io(std::io::Error::other("something else"));
        assert_eq!(classify(&other), Kind::Other);
    }

    #[test]
    fn only_a_bad_url_is_an_invalid_url() {
        let build = |name: &str, value: &str, uri: &str| {
            ureq::http::Request::builder()
                .uri(uri)
                .header(name, value)
                .body(())
                .unwrap_err()
        };
        assert_eq!(
            classify_build(&build("x-ok", "1", "http://exa mple.com/")),
            Kind::InvalidUrl
        );
        assert_eq!(
            classify_build(&build("bad name", "1", "http://example.com/")),
            Kind::Other
        );
        assert_eq!(
            classify_build(&build("x-ok", "line\nbreak", "http://example.com/")),
            Kind::Other
        );
        let method = ureq::http::Request::builder()
            .method("BAD METHOD")
            .uri("http://example.com/")
            .body(())
            .unwrap_err();
        assert_eq!(classify_build(&method), Kind::Other);
    }
}
