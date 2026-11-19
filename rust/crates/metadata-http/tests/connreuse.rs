
mod common;

use std::time::Duration;

use common::{Reply, TestServer};
use metadata_http::{decode_capped, Cancel, HttpClient};

#[derive(serde::Deserialize)]
struct Meta {
    ok: bool,
}

/// Proves the keep-alive fix: N sequential `get`+`decode_capped` round trips
/// from one shared client against the same host must reuse ONE TCP
/// connection (baseline: N connections).
///
/// The server pads its JSON with trailing whitespace (valid JSON: the decode
/// succeeds). Padding past the decoder's read buffer is what makes the test
/// discriminate: a tiny body is buffered whole, so even the baseline reuses
/// that connection; with the padding outstanding, the decoder returns while
/// most of the body is still unread, and only the bounded post-decode drain
/// returns the connection to the pool.
///
/// Padding is 256 KiB here where Go uses 64 KiB: the transport (ureq) reads
/// through a 128 KiB input buffer, so the pad must clear that buffer plus the
/// decoder's own read-ahead for the assertion to discriminate at all. The
/// drain cap (512 KiB) is unchanged, so the whole body is still drained.
#[test]
fn decode_capped_reuses_connection() {
    const PAD: usize = 256 << 10;
    const N: usize = 5;
    let body = format!("{{\"ok\":true}}{}", " ".repeat(PAD));

    let server = TestServer::start(vec![Reply::ok_json(body)]);
    let client = HttpClient::new(Duration::from_secs(8));
    let cancel = Cancel::background();

    for i in 0..N {
        let response = client
            .get(&cancel, &server.url("/meta"))
            .unwrap_or_else(|err| panic!("fetch {i}: {err}"));
        let meta: Meta = decode_capped(response).unwrap_or_else(|err| panic!("decode {i}: {err}"));
        assert!(meta.ok, "decode {i}: ok=false, want true");
    }

    assert_eq!(
        server.connections(),
        1,
        "server connections = {} over {N} sequential fetches: the bounded post-decode drain must \
         return the connection to the pool",
        server.connections()
    );
}
