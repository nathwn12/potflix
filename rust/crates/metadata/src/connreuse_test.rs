
use std::time::Duration;

use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped};

use crate::testsupport::{FakeServer, Request, Response};

/// `TestDecodeCappedReusesConnection`: N sequential `fetchGet`+`decodeCapped`
/// round trips from one shared client against the same host must reuse ONE TCP
/// connection. The server pads its JSON with 64 KB of trailing whitespace, so
/// only the bounded post-decode drain returns the connection to the pool.
#[test]
fn decode_capped_reuses_connection() {
    const PAD: usize = 64 << 10;
    let body = format!("{{\"ok\":true}}{}", " ".repeat(PAD));
    let server = FakeServer::start(move |_req: &Request| Response::json(&body));
    let client = HttpClient::new(Duration::from_secs(5));

    #[derive(serde::Deserialize)]
    struct Payload {
        ok: bool,
    }

    const N: usize = 5;
    for i in 0..N {
        let response = client
            .get(&Cancel::background(), &format!("{}/meta", server.url()))
            .unwrap_or_else(|err| panic!("fetch {i}: {err}"));
        let value: Payload =
            decode_capped(response).unwrap_or_else(|err| panic!("decode {i}: {err}"));
        assert!(value.ok, "decode {i}: ok=false, want true");
    }
    assert_eq!(
        server.connections(),
        1,
        "{N} sequential fetches must reuse one connection"
    );
}
