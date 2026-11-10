// SPDX-License-Identifier: MIT OR Apache-2.0

//! Purging through [`BunnyCdn`] against a local mock server (I3 plan T10): the URL purge's
//! query, the whole-zone path, a 429 waited out and a rejected key. Nothing here reaches
//! api.bunny.net.

use std::time::{Duration, Instant};

use httporter_bunny::{BunnyAccount, BunnyCdn};
use httporter_core::{AuthHint, CdnPurger, CdnZone, ProviderError};
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "acct-key-7f3a9c1e-0b52-4d68-a1c4-5e9d2b8f6a30";

fn cdn(server: &MockServer) -> BunnyCdn {
    let account = BunnyAccount::with_base_url(KEY.to_owned(), server.uri()).unwrap();
    BunnyCdn::new(
        account,
        vec![CdnZone {
            id: 4,
            name: "httporter-test-sg".into(),
            hostnames: vec!["httporter-test-sg.b-cdn.net".into()],
        }],
    )
}

#[tokio::test]
async fn a_url_purge_sends_the_whole_url_encoded_in_one_query_parameter() {
    let server = MockServer::start().await;
    let url = "https://httporter-test-sg.b-cdn.net/blog/café menu.html?v=1&x=2#top";
    Mock::given(method("POST"))
        .and(path("/purge"))
        .and(query_param("url", url))
        .and(query_param("async", "false"))
        .and(header("accesskey", KEY))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    cdn(&server).purge_url(url).await.unwrap();

    // Everything but the unreserved characters is encoded, so the URL's own `?`, `&` and `#`
    // stay inside the one parameter.
    let requests = server.received_requests().await.unwrap();
    let query = requests[0].url.query().unwrap();
    assert_eq!(
        query,
        "url=https%3A%2F%2Fhttporter-test-sg.b-cdn.net%2Fblog%2Fcaf%C3%A9%20menu.html\
         %3Fv%3D1%26x%3D2%23top&async=false"
    );
    assert!(requests[0].body.is_empty());
}

#[tokio::test]
async fn a_zone_purge_posts_an_empty_json_object_to_purge_cache() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pullzone/4/purgeCache"))
        .and(header("accesskey", KEY))
        .and(header("content-type", "application/json"))
        .and(body_string("{}"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let cdn = cdn(&server);
    assert_eq!(cdn.zones()[0].id, 4);
    cdn.purge_zone(4).await.unwrap();
}

#[tokio::test]
async fn a_429_is_waited_out_for_its_retry_after_and_tried_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/purge"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/purge"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let started = Instant::now();
    cdn(&server)
        .purge_url("https://httporter-test-sg.b-cdn.net/index.html")
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the second try came before the Retry-After"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_zone_purge_rate_limited_three_times_gives_up_as_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pullzone/4/purgeCache"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .expect(3)
        .mount(&server)
        .await;

    let err = cdn(&server).purge_zone(4).await.unwrap_err();
    assert!(matches!(err, ProviderError::RateLimited { .. }), "{err:?}");
}

#[tokio::test]
async fn a_rejected_key_is_auth_and_not_tried_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"HttpCode":401,"Message":"Authorization has been denied for this request."}"#,
        ))
        .expect(2)
        .mount(&server)
        .await;
    let cdn = cdn(&server);

    let url_err = cdn
        .purge_url("https://httporter-test-sg.b-cdn.net/a.html")
        .await
        .unwrap_err();
    let zone_err = cdn.purge_zone(4).await.unwrap_err();

    for err in [url_err, zone_err] {
        assert_eq!(
            err,
            ProviderError::Auth {
                hint: AuthHint::Unknown
            }
        );
        assert!(!format!("{err} {err:?}").contains(KEY));
    }
}
