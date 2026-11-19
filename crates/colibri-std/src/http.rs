//! `http`: a small blocking HTTP/1.1 client behind the `net` capability, on `ureq` with
//! rustls and the bundled webpki roots (`https://` works with no system TLS library).
//!
//! * `http.get(url)`, `http.post(url, body)`, `http.post_as(url, body, content_type)` and the
//!   general `http.request(method, url, headers, body)` return `Result[Response]`.
//! * A response with any status is an `ok`: `404` is data (`response.status`), not an error.
//!   An `err` means no response arrived: a bad URL, a refused connection, a TLS failure, a
//!   timeout, a body over the size limit.
//! * `Response` is a struct: `status: int`, `headers: Map[str, str]` (names lowercased; a
//!   header sent several times is joined with `, `) and `body: str`. A body that is not valid
//!   UTF-8 is decoded lossily (invalid sequences become U+FFFD): v1 has no byte-body variant.
//!   Parse JSON with `json.decode[T](response.body)`.
//! * Redirects are followed (up to 10). The proxy variables of the process (`HTTP_PROXY`,
//!   `HTTPS_PROXY`, `NO_PROXY`) are honoured.
//! * Timeouts: 30 seconds for a whole request by default; `http.set_timeout(seconds)` changes
//!   it for the rest of the program (`0` means no timeout).
//!
//! A call **blocks the whole Vm**, coroutines included, until the response is complete: a host
//! function cannot wait (ARCHITECTURE §16). Use short timeouts in a program that must keep
//! animating.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use colibri_runtime::{Cap, Ctx, Cx, Error, IntoColibri, TypeDesc, TypeRegistry, Value, Vm};

use crate::needs;

/// The default for a whole request, from connecting to the last byte of the body.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest response body read, in bytes.
const MAX_BODY: u64 = 64 << 20;

/// What `http.set_timeout` changes.
struct HttpState {
    timeout: Option<Duration>,
}

struct Response {
    status: i64,
    headers: BTreeMap<String, String>,
    body: String,
}

impl IntoColibri for Response {
    fn ty(reg: &mut TypeRegistry) -> TypeDesc {
        let status = <i64 as IntoColibri>::ty(reg);
        let headers = <BTreeMap<String, String> as IntoColibri>::ty(reg);
        let body = <String as IntoColibri>::ty(reg);
        reg.define_struct(
            "Response",
            vec![("status", status), ("headers", headers), ("body", body)],
        )
    }

    fn into_colibri(self, cx: &mut Cx<'_>) -> Value {
        let fields = vec![
            self.status.into_colibri(cx),
            self.headers.into_colibri(cx),
            self.body.into_colibri(cx),
        ];
        cx.new_struct("Response", fields)
    }
}

fn perform(
    timeout: Option<Duration>,
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<String>,
) -> Result<Response, String> {
    let fail = |e: &dyn std::fmt::Display| format!("http: {method} {url} failed: {e}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(timeout)
        // A 404 is a response, not a failure.
        .http_status_as_error(false)
        .build()
        .into();
    let mut builder = ureq::http::Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(body.unwrap_or_default())
        .map_err(|e| fail(&e))?;
    let mut response = agent.run(request).map_err(|e| fail(&e))?;
    let status = i64::from(response.status().as_u16());
    let mut out_headers: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in response.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        out_headers
            .entry(name.as_str().to_ascii_lowercase())
            .and_modify(|old| {
                old.push_str(", ");
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
        .map_err(|e| fail(&e))?;
    if bytes.len() as u64 > MAX_BODY {
        return Err(fail(&format!(
            "the response body is over {} MiB",
            MAX_BODY >> 20
        )));
    }
    Ok(Response {
        status,
        headers: out_headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn run(
    cx: &mut Ctx,
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<String>,
) -> Result<Response, String> {
    let timeout = cx
        .state_mut::<HttpState>()
        .map_or(Some(DEFAULT_TIMEOUT), |s| s.timeout);
    perform(timeout, method, url, headers, body)
}

pub(crate) fn register(vm: &mut Vm) -> Result<(), Error> {
    vm.insert_state(HttpState {
        timeout: Some(DEFAULT_TIMEOUT),
    });
    let mut m = vm.std_module("http");
    needs(
        Cap::Net,
        m.register_fn_named("get", ["url"], |cx: &mut Ctx, url: String| {
            run(cx, "GET", &url, &BTreeMap::new(), None)
        }),
    )?;
    needs(
        Cap::Net,
        m.register_fn_named(
            "post",
            ["url", "body"],
            |cx: &mut Ctx, url: String, body: String| {
                let headers = content_type("text/plain; charset=utf-8");
                run(cx, "POST", &url, &headers, Some(body))
            },
        ),
    )?;
    needs(
        Cap::Net,
        m.register_fn_named(
            "post_as",
            ["url", "body", "content_type"],
            |cx: &mut Ctx, url: String, body: String, kind: String| {
                run(cx, "POST", &url, &content_type(&kind), Some(body))
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
                if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
                    return Err(format!("http: `{method}` is not an HTTP method"));
                }
                let method = method.to_ascii_uppercase();
                let body = if body.is_empty() { None } else { Some(body) };
                run(cx, &method, &url, &headers, body)
            },
        ),
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

fn content_type(kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("content-type".to_string(), kind.to_string())])
}
