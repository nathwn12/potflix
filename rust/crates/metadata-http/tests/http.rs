
mod common;

use std::time::{Duration, Instant};

use common::{Reply, TestServer};
use metadata_http::{MAX_429_RETRIES, decode_capped, Cancel, CancelError, Error, HttpClient};

#[derive(serde::Deserialize, Debug)]
struct Meta {
    ok: bool,
}

#[derive(serde::Serialize)]
struct GraphQl<'a> {
    query: &'a str,
}

fn header_values(head: &str, name: &str) -> Vec<String> {
    head.lines()
        .filter_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
        .collect()
}

/// The wire constants are contract: `User-Agent: potflix-metadata/1.0` and
/// `Accept: application/json`, exactly once each (http.go:16, :105-106).
#[test]
fn get_sends_the_shared_user_agent_and_accept() {
    let server = TestServer::start(vec![Reply::ok_json("{\"ok\":true}")]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let _: Meta = decode_capped(response).expect("decode");

    let heads = server.heads();
    assert_eq!(heads.len(), 1, "one request expected");
    assert!(
        heads[0].starts_with("GET /meta HTTP/1.1"),
        "request line: {:?}",
        heads[0].lines().next()
    );
    assert_eq!(
        header_values(&heads[0], "user-agent"),
        vec!["potflix-metadata/1.0".to_string()]
    );
    assert_eq!(
        header_values(&heads[0], "accept"),
        vec!["application/json".to_string()]
    );
    for value in header_values(&heads[0], "accept-encoding") {
        assert!(
            !value.contains("gzip") && !value.contains("br"),
            "compression must not be negotiated: accept-encoding: {value}"
        );
    }
}

/// `fetchPostJSON` adds `Content-Type: application/json` and the JSON body
/// (http.go:112-127).
#[test]
fn post_json_sends_content_type_and_payload() {
    let server = TestServer::start(vec![Reply::ok_json("{\"ok\":true}")]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .post_json(
            &Cancel::background(),
            &server.url("/graphql"),
            &GraphQl {
                query: "query ($search:String){Media(search:$search)}",
            },
        )
        .expect("post");
    let _: Meta = decode_capped(response).expect("decode");

    let heads = server.heads();
    assert_eq!(heads.len(), 1, "one request expected");
    assert!(
        heads[0].starts_with("POST /graphql HTTP/1.1"),
        "request line: {:?}",
        heads[0].lines().next()
    );
    assert_eq!(
        header_values(&heads[0], "content-type"),
        vec!["application/json".to_string()]
    );
    assert_eq!(
        header_values(&heads[0], "user-agent"),
        vec!["potflix-metadata/1.0".to_string()]
    );
    assert_eq!(
        server.bodies(),
        vec!["{\"query\":\"query ($search:String){Media(search:$search)}\"}".to_string()]
    );
}

/// A non-2xx status is an error, and the response body never reaches the
/// message (http.go:143-145).
#[test]
fn decode_capped_rejects_non_2xx_without_the_body() {
    let server = TestServer::start(vec![Reply::too_many_requests(None)]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("429 is a response, not a transport error");
    assert_eq!(response.status().as_u16(), 429);
    let err = decode_capped::<Meta>(response).expect_err("429 must not decode");
    assert!(
        matches!(err, Error::Status(status) if status.as_u16() == 429),
        "unexpected error: {err}"
    );
    let message = err.to_string();
    assert!(
        message.contains("unexpected HTTP status 429"),
        "message: {message}"
    );
    assert!(
        !message.contains("rate limited"),
        "the response body must never be logged: {message}"
    );
    assert_eq!(
        server.hits(),
        MAX_429_RETRIES as usize + 1,
        "one initial attempt plus the bounded retries"
    );
}

/// A persistent 429 never loops: 1 initial attempt plus exactly
/// MAX_429_RETRIES retries, then the last 429 is handed back.
#[test]
fn persistent_429_stops_after_the_bounded_retries() {
    let server = TestServer::start(vec![Reply::too_many_requests(None)]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("429 is a response, not a transport error");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        server.hits(),
        MAX_429_RETRIES as usize + 1,
        "retries must be capped"
    );
}

/// A 429 followed by a 200 returns the 200: idempotent GETs retry with
/// backoff and jitter (http.go:53-96).
#[test]
fn one_retry_on_429_returns_the_second_response() {
    let server = TestServer::start(vec![
        Reply::too_many_requests(Some("0")),
        Reply::ok_json("{\"ok\":true}"),
    ]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let meta: Meta = decode_capped(response).expect("decode");
    assert!(meta.ok);
    assert_eq!(
        server.hits(),
        2,
        "one initial attempt plus exactly one retry"
    );
}

/// A plain POST is NEVER retried, even on 429: POST is not idempotent in
/// general, so the first 429 is handed back untouched.
#[test]
fn post_json_is_never_retried_on_429() {
    let server = TestServer::start(vec![
        Reply::too_many_requests(Some("0")),
        Reply::ok_json("{\"ok\":true}"),
    ]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .post_json(
            &Cancel::background(),
            &server.url("/graphql"),
            &GraphQl { query: "query {Media}" },
        )
        .expect("429 is a response, not a transport error");
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(server.hits(), 1, "a non-idempotent POST must not retry");
}

/// A read-only search POST retries like a GET: the idempotent opt-in exists
/// for the AniList / JustWatch GraphQL searches, which are side-effect-free.
#[test]
fn post_json_idempotent_retries_on_429() {
    let server = TestServer::start(vec![
        Reply::too_many_requests(Some("0")),
        Reply::ok_json("{\"ok\":true}"),
    ]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .post_json_idempotent(
            &Cancel::background(),
            &server.url("/graphql"),
            &GraphQl { query: "query {Media}" },
        )
        .expect("fetch");
    let meta: Meta = decode_capped(response).expect("decode");
    assert!(meta.ok);
    assert_eq!(server.hits(), 2, "an idempotent POST retries like a GET");
}

/// Without `Retry-After` the retry waits for the backoff floor (200 ms plus
/// up to 100 ms jitter): this check CAN go red, a missing backoff returns
/// near-instantly.
#[test]
fn retry_without_retry_after_waits_for_backoff() {
    let server = TestServer::start(vec![
        Reply::too_many_requests(None),
        Reply::ok_json("{\"ok\":true}"),
    ]);
    let client = HttpClient::new(Duration::from_secs(8));
    let started = Instant::now();
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let meta: Meta = decode_capped(response).expect("decode");
    assert!(meta.ok);
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the backoff floor was skipped: {:?}",
        started.elapsed()
    );
    assert_eq!(server.hits(), 2);
}

/// A 1 s `Retry-After` is honored before the retry succeeds.
#[test]
fn retry_after_delays_the_retry() {
    let server = TestServer::start(vec![
        Reply::too_many_requests(Some("1")),
        Reply::ok_json("{\"ok\":true}"),
    ]);
    let client = HttpClient::new(Duration::from_secs(8));
    let started = Instant::now();
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let meta: Meta = decode_capped(response).expect("decode");
    assert!(meta.ok);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the Retry-After was not honored: {:?}",
        started.elapsed()
    );
    assert_eq!(server.hits(), 2);
}

/// A 500 is NOT retried: only 429 is (http.go:68-70).
#[test]
fn a_500_is_not_retried() {
    let server = TestServer::start(vec![Reply {
        status: 500,
        reason: "Internal Server Error",
        retry_after: None,
        body: "{\"error\":\"boom\"}".to_string(),
    }]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    assert!(decode_capped::<Meta>(response).is_err());
    assert_eq!(server.hits(), 1, "a 500 must not be retried");
}

/// `Retry-After` is honored but bounded by `maxRetryWait` (10 s), and the wait
/// is cancellable by the scope (http.go:71-87).
#[test]
fn retry_after_is_bounded_and_cancellable() {
    let server = TestServer::start(vec![Reply::too_many_requests(Some("3600"))]);
    let client = HttpClient::new(Duration::from_secs(8));
    let cancel = Cancel::with_deadline(Duration::from_millis(50));
    let started = Instant::now();
    let err = client
        .get(&cancel, &server.url("/meta"))
        .expect_err("the deadline must abort the retry wait");
    assert!(
        matches!(err, Error::Cancelled(CancelError::DeadlineExceeded)),
        "unexpected error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the bounded wait took {:?}",
        started.elapsed()
    );
    assert_eq!(
        server.hits(),
        1,
        "the retry must not be sent after the abort"
    );
}

/// An already-cancelled scope never sends the request.
#[test]
fn a_cancelled_scope_aborts_before_the_request() {
    let server = TestServer::start(vec![Reply::ok_json("{\"ok\":true}")]);
    let client = HttpClient::new(Duration::from_secs(8));
    let cancel = Cancel::background();
    cancel.cancel();
    let err = client
        .get(&cancel, &server.url("/meta"))
        .expect_err("a cancelled scope must abort");
    assert!(
        matches!(err, Error::Cancelled(CancelError::Cancelled)),
        "unexpected error: {err}"
    );
    assert_eq!(server.hits(), 0);
}

/// The decoder reads the first JSON value and ignores trailing content, like
/// `json.Decoder.Decode`; an empty body is a decode error (http.go:141-152).
#[test]
fn decode_capped_ignores_trailing_content() {
    let padded = format!("{{\"ok\":true}}{}", " ".repeat(64 << 10));
    let server = TestServer::start(vec![Reply::ok_json(padded)]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let meta: Meta = decode_capped(response).expect("decode");
    assert!(meta.ok);
}

#[test]
fn decode_capped_errors_on_an_empty_body() {
    let server = TestServer::start(vec![Reply::ok_json("")]);
    let client = HttpClient::new(Duration::from_secs(8));
    let response = client
        .get(&Cancel::background(), &server.url("/meta"))
        .expect("fetch");
    let err = decode_capped::<Meta>(response).expect_err("an empty body must not decode");
    assert!(matches!(err, Error::Decode(_)), "unexpected error: {err}");
}
