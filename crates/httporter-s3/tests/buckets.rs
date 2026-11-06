// SPDX-License-Identifier: MIT OR Apache-2.0

//! The bucket tools (I6, §8.8.4–8.8.5) against a local mock server: `CreateBucket`, the CORS
//! rules and the static website. Nothing here contacts a real service; every mock demands a
//! validly signed request ([`Signed`]) unless it says why not.

use httporter_core::{AuthHint, ProviderError};
use httporter_s3::{
    BUCKET_NAME_TAKEN, CorsRule, Endpoint, NOT_OFFERED, S3Config, S3Storage, Url, Website,
};
use wiremock::matchers::{body_string, body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

mod common;

use common::*;

const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// A provider for the bucket `bucket` (empty: none, as the Add Site sheet's Create Bucket… has).
fn provider_for(server: &MockServer, bucket: &str, region: &str) -> S3Storage {
    let endpoint = Endpoint {
        base: Url::parse(&server.uri()).unwrap(),
        path_style: true,
        region: region.into(),
    };
    S3Storage::new(S3Config::for_site(bucket, endpoint, creds(), &settings())).unwrap()
}

fn cors_mock(verb: &str) -> wiremock::MockBuilder {
    Mock::given(method(verb))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("cors", ""))
        .and(Signed::new())
}

fn website_mock(verb: &str) -> wiremock::MockBuilder {
    Mock::given(method(verb))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("website", ""))
        .and(Signed::new())
}

// ---- CreateBucket ----

#[tokio::test]
async fn create_bucket_names_the_region_outside_us_east_1() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/new-site"))
        .and(header("content-type", "application/xml"))
        .and(body_string(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CreateBucketConfiguration xmlns=\"{NS}\">\
             <LocationConstraint>{REGION}</LocationConstraint></CreateBucketConfiguration>"
        )))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200).insert_header("location", "/new-site"))
        .expect(1)
        .mount(&server)
        .await;

    provider_for(&server, "", REGION)
        .create_bucket("new-site")
        .await
        .unwrap();
}

/// `us-east-1` takes no `LocationConstraint` (S3 refuses one there), and neither does R2's
/// `auto`. The signature is for another region than [`Signed`] checks, so only the shape is.
#[tokio::test]
async fn create_bucket_sends_no_body_in_us_east_1_or_auto() {
    for region in ["us-east-1", "auto"] {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/new-site"))
            .and(header("content-length", "0"))
            .and(move |request: &Request| {
                request.body.is_empty()
                    && header_of(request, "authorization")
                        .is_some_and(|auth| auth.contains(&format!("/{region}/s3/aws4_request")))
            })
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        provider_for(&server, "", region)
            .create_bucket("new-site")
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_name_taken_by_someone_else_is_refused_and_ones_own_bucket_is_fine() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/taken"))
        .and(Signed::new())
        .respond_with(s3_error(
            409,
            "BucketAlreadyExists",
            "The requested bucket name is not available.",
        ))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/mine"))
        .and(Signed::new())
        .respond_with(s3_error(
            409,
            "BucketAlreadyOwnedByYou",
            "Your previous request to create the named bucket succeeded.",
        ))
        .mount(&server)
        .await;
    let s3 = provider_for(&server, "", REGION);

    assert_eq!(
        s3.create_bucket("taken").await,
        Err(ProviderError::Refused(BUCKET_NAME_TAKEN.into()))
    );
    s3.create_bucket("mine").await.unwrap();
}

#[tokio::test]
async fn create_bucket_reports_a_refused_key_and_other_errors_as_s3_does() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/denied"))
        .respond_with(s3_error(403, "AccessDenied", "Access Denied"))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/too-many"))
        .respond_with(s3_error(
            400,
            "TooManyBuckets",
            "You have attempted to create more buckets than allowed",
        ))
        .mount(&server)
        .await;
    let s3 = provider_for(&server, "", REGION);

    assert_eq!(
        s3.create_bucket("denied").await,
        Err(ProviderError::Auth {
            hint: AuthHint::Unknown
        })
    );
    match s3.create_bucket("too-many").await {
        Err(ProviderError::Permanent(message)) => {
            assert!(message.starts_with("TooManyBuckets"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

// ---- CORS ----

#[tokio::test]
async fn cors_rules_are_read() {
    let server = MockServer::start().await;
    cors_mock("GET")
        .respond_with(xml(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CORSConfiguration xmlns=\"{NS}\">\
             <CORSRule><AllowedHeader>*</AllowedHeader><AllowedMethod>GET</AllowedMethod>\
             <AllowedMethod>HEAD</AllowedMethod><AllowedOrigin>https://example.com</AllowedOrigin>\
             <ExposeHeader>ETag</ExposeHeader><MaxAgeSeconds>3000</MaxAgeSeconds></CORSRule>\
             <CORSRule><ID>uploads</ID><AllowedMethod>PUT</AllowedMethod>\
             <AllowedOrigin>*</AllowedOrigin></CORSRule></CORSConfiguration>"
        )))
        .expect(1)
        .mount(&server)
        .await;

    let rules = provider(&server).cors().await.unwrap();

    assert_eq!(
        rules,
        [
            CorsRule {
                origins: vec!["https://example.com".into()],
                methods: vec!["GET".into(), "HEAD".into()],
                headers: vec!["*".into()],
                max_age: Some(3000),
                expose_headers: vec!["ETag".into()],
                id: None,
            },
            CorsRule {
                origins: vec!["*".into()],
                methods: vec!["PUT".into()],
                id: Some("uploads".into()),
                ..CorsRule::default()
            },
        ]
    );
}

#[tokio::test]
async fn a_bucket_without_cors_has_no_rules_but_a_missing_bucket_is_not_found() {
    let server = MockServer::start().await;
    cors_mock("GET")
        .respond_with(s3_error(
            404,
            "NoSuchCORSConfiguration",
            "The CORS configuration does not exist",
        ))
        .mount(&server)
        .await;
    assert_eq!(provider(&server).cors().await.unwrap(), []);

    let server = MockServer::start().await;
    cors_mock("GET")
        .respond_with(s3_error(
            404,
            "NoSuchBucket",
            "The specified bucket does not exist",
        ))
        .mount(&server)
        .await;
    assert_eq!(provider(&server).cors().await, Err(ProviderError::NotFound));
}

#[tokio::test]
async fn set_cors_puts_the_rules_with_content_md5() {
    let server = MockServer::start().await;
    cors_mock("PUT")
        .and(header("content-type", "application/xml"))
        .and(ContentMd5)
        .and(body_string_contains(
            "<CORSRule><AllowedOrigin>https://a.example</AllowedOrigin>\
             <AllowedOrigin>https://b.example</AllowedOrigin><AllowedMethod>GET</AllowedMethod>\
             <AllowedMethod>HEAD</AllowedMethod><MaxAgeSeconds>600</MaxAgeSeconds></CORSRule>",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .set_cors(&[CorsRule {
            origins: vec!["https://a.example".into(), "https://b.example".into()],
            methods: vec!["GET".into(), "HEAD".into()],
            max_age: Some(600),
            ..CorsRule::default()
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn no_rules_deletes_the_cors_configuration() {
    let server = MockServer::start().await;
    cors_mock("DELETE")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server).set_cors(&[]).await.unwrap();

    // Already gone is deleted.
    let server = MockServer::start().await;
    cors_mock("DELETE")
        .respond_with(s3_error(404, "NoSuchCORSConfiguration", "none"))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server).set_cors(&[]).await.unwrap();
}

/// A service without the call says `NotImplemented` (MinIO, R2's website): never a server error
/// worth retrying.
#[tokio::test]
async fn a_call_the_service_does_not_offer_is_refused() {
    let server = MockServer::start().await;
    cors_mock("PUT")
        .respond_with(s3_error(
            501,
            "NotImplemented",
            "A header you provided implies functionality that is not implemented",
        ))
        .mount(&server)
        .await;
    website_mock("GET")
        .respond_with(ResponseTemplate::new(501))
        .mount(&server)
        .await;
    let s3 = provider(&server);

    let refused = Err(ProviderError::Refused(NOT_OFFERED.into()));
    assert_eq!(
        s3.set_cors(&[CorsRule {
            origins: vec!["*".into()],
            methods: vec!["GET".into()],
            ..CorsRule::default()
        }])
        .await,
        refused
    );
    assert_eq!(s3.website().await.map(|_| ()), refused);
}

#[tokio::test]
async fn bucket_settings_need_a_bucket() {
    let server = MockServer::start().await;
    let s3 = provider_for(&server, "", REGION);
    assert!(matches!(s3.cors().await, Err(ProviderError::Permanent(_))));
    assert!(matches!(
        s3.website().await,
        Err(ProviderError::Permanent(_))
    ));
}

// ---- Static website ----

#[tokio::test]
async fn the_website_is_read_and_a_bucket_without_one_is_none() {
    let server = MockServer::start().await;
    website_mock("GET")
        .respond_with(xml(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><WebsiteConfiguration xmlns=\"{NS}\">\
             <IndexDocument><Suffix>index.html</Suffix></IndexDocument>\
             <ErrorDocument><Key>404.html</Key></ErrorDocument></WebsiteConfiguration>"
        )))
        .mount(&server)
        .await;
    assert_eq!(
        provider(&server).website().await.unwrap(),
        Some(Website {
            index: "index.html".into(),
            error: Some("404.html".into()),
        })
    );

    let server = MockServer::start().await;
    website_mock("GET")
        .respond_with(s3_error(
            404,
            "NoSuchWebsiteConfiguration",
            "The specified bucket does not have a website configuration",
        ))
        .mount(&server)
        .await;
    assert_eq!(provider(&server).website().await.unwrap(), None);
}

#[tokio::test]
async fn set_website_puts_the_documents_and_none_deletes_them() {
    let server = MockServer::start().await;
    website_mock("PUT")
        .and(header("content-type", "application/xml"))
        .and(ContentMd5)
        .and(body_string(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><WebsiteConfiguration xmlns=\"{NS}\">\
             <IndexDocument><Suffix>index.html</Suffix></IndexDocument>\
             <ErrorDocument><Key>errors/404.html</Key></ErrorDocument></WebsiteConfiguration>"
        )))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    website_mock("DELETE")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let s3 = provider(&server);

    s3.set_website(Some(&Website {
        index: "index.html".into(),
        error: Some("errors/404.html".into()),
    }))
    .await
    .unwrap();
    s3.set_website(None).await.unwrap();
    // An empty index document is never sent.
    assert!(matches!(
        s3.set_website(Some(&Website {
            index: " ".into(),
            error: None
        }))
        .await,
        Err(ProviderError::Permanent(_))
    ));
}
