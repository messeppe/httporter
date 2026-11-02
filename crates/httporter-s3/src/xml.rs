//! The XML documents of the S3 API: listings, the bucket list, error documents and the
//! `DeleteObjects` request and answer. Parsing is lenient about what it does not need (unknown
//! elements, namespaces, the order of elements) and strict about what it does: a document of
//! the wrong kind is an error, never an empty result.

use httporter_core::ProviderError;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;

use crate::errors;

/// The most keys one `DeleteObjects` request can name.
pub(crate) const DELETE_MAX: usize = 1000;

/// What an `<Error>` document says. Both parts are `None` when the body is not one (an HTML
/// page from a proxy, an empty answer to a `HEAD`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ErrorDoc {
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Deserialize)]
struct ErrorXml {
    #[serde(rename = "Code")]
    code: Option<String>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

pub(crate) fn parse_error(body: &[u8]) -> ErrorDoc {
    let text = String::from_utf8_lossy(body);
    match quick_xml::de::from_str::<ErrorXml>(&text) {
        Ok(doc) => ErrorDoc {
            code: doc
                .code
                .map(|c| c.trim().to_owned())
                .filter(|c| !c.is_empty()),
            message: doc.message,
        },
        Err(_) => ErrorDoc::default(),
    }
}

/// One page of `ListObjectsV2`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ListPage {
    pub truncated: bool,
    pub next_token: Option<String>,
    /// The service says keys and prefixes are URL-encoded (`EncodingType` is `url`), which it
    /// does only when asked to.
    pub url_encoded: bool,
    pub objects: Vec<ListedObject>,
    pub prefixes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListedObject {
    pub key: String,
    pub modified: Option<String>,
    pub etag: Option<String>,
    pub size: u64,
}

#[derive(Deserialize)]
struct ListXml {
    #[serde(rename = "IsTruncated", default)]
    truncated: Option<String>,
    #[serde(rename = "NextContinuationToken", default)]
    next_token: Option<String>,
    #[serde(rename = "EncodingType", default)]
    encoding_type: Option<String>,
    #[serde(rename = "Contents", default)]
    contents: Vec<ObjectXml>,
    #[serde(rename = "CommonPrefixes", default)]
    prefixes: Vec<PrefixXml>,
}

#[derive(Deserialize)]
struct ObjectXml {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "LastModified", default)]
    modified: Option<String>,
    #[serde(rename = "ETag", default)]
    etag: Option<String>,
    #[serde(rename = "Size", default)]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct PrefixXml {
    #[serde(rename = "Prefix")]
    prefix: String,
}

/// Parses a `ListBucketResult`. `status` is the HTTP status of the answer, for the case where
/// the body turns out to be an `<Error>`.
pub(crate) fn parse_list(body: &[u8], status: u16) -> Result<ListPage, ProviderError> {
    let text = expect_root(body, "ListBucketResult", status)?;
    let list: ListXml = quick_xml::de::from_str(&text).map_err(|e| unrecognised("listing", &e))?;
    Ok(ListPage {
        truncated: list
            .truncated
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("true")),
        next_token: list.next_token.filter(|t| !t.is_empty()),
        url_encoded: list
            .encoding_type
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("url")),
        objects: list
            .contents
            .into_iter()
            .map(|o| ListedObject {
                key: o.key,
                modified: o.modified,
                etag: o.etag,
                size: o.size.unwrap_or(0),
            })
            .collect(),
        prefixes: list.prefixes.into_iter().map(|p| p.prefix).collect(),
    })
}

#[derive(Deserialize)]
struct BucketsXml {
    #[serde(rename = "Buckets", default)]
    buckets: BucketList,
}

#[derive(Default, Deserialize)]
struct BucketList {
    #[serde(rename = "Bucket", default)]
    list: Vec<BucketXml>,
}

#[derive(Deserialize)]
struct BucketXml {
    #[serde(rename = "Name")]
    name: String,
}

/// The bucket names in a `ListAllMyBucketsResult`.
pub(crate) fn parse_buckets(body: &[u8], status: u16) -> Result<Vec<String>, ProviderError> {
    let text = expect_root(body, "ListAllMyBucketsResult", status)?;
    let doc: BucketsXml =
        quick_xml::de::from_str(&text).map_err(|e| unrecognised("list of buckets", &e))?;
    Ok(doc.buckets.list.into_iter().map(|b| b.name).collect())
}

/// One key a `DeleteObjects` request could not delete. `key` is `None` when the service did
/// not say which.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeleteFailure {
    pub key: Option<String>,
    pub code: Option<String>,
    pub message: Option<String>,
}

#[derive(Deserialize)]
struct DeleteResultXml {
    #[serde(rename = "Error", default)]
    errors: Vec<DeleteErrorXml>,
}

#[derive(Deserialize)]
struct DeleteErrorXml {
    #[serde(rename = "Key", default)]
    key: Option<String>,
    #[serde(rename = "Code", default)]
    code: Option<String>,
    #[serde(rename = "Message", default)]
    message: Option<String>,
}

/// The failures in a `DeleteResult`. The keys deleted are not read: the request asks for quiet
/// mode, and a key that is not named here counts as deleted.
pub(crate) fn parse_delete_result(
    body: &[u8],
    status: u16,
) -> Result<Vec<DeleteFailure>, ProviderError> {
    let text = expect_root(body, "DeleteResult", status)?;
    let doc: DeleteResultXml =
        quick_xml::de::from_str(&text).map_err(|e| unrecognised("delete result", &e))?;
    Ok(doc
        .errors
        .into_iter()
        .map(|e| DeleteFailure {
            key: e.key,
            code: e.code.map(|c| c.trim().to_owned()),
            message: e.message,
        })
        .collect())
}

/// The body as text, after checking that its root element is `root`. A body that is an
/// `<Error>` (some services answer 200 with one) becomes that error; anything else is an
/// unexpected answer. Without this check a document of the wrong kind would parse as an empty
/// result, which for a listing means "there is nothing here".
fn expect_root(body: &[u8], root: &str, status: u16) -> Result<String, ProviderError> {
    let text = String::from_utf8_lossy(body).into_owned();
    match root_name(&text).as_deref() {
        Some(name) if name == root => Ok(text),
        Some("Error") => {
            let doc = parse_error(body);
            Err(errors::from_parts(
                status,
                doc.code.as_deref(),
                doc.message.as_deref(),
                None,
            ))
        }
        _ => Err(ProviderError::Permanent(format!(
            "unexpected response from server (not an S3 {})",
            match root {
                "ListBucketResult" => "listing",
                "ListAllMyBucketsResult" => "list of buckets",
                _ => "result",
            }
        ))),
    }
}

/// The local name of the first element, if the text is XML at all.
fn root_name(text: &str) -> Option<String> {
    let mut reader = Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return Some(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

fn unrecognised(what: &str, error: &quick_xml::DeError) -> ProviderError {
    ProviderError::Permanent(format!(
        "unexpected response from server (unrecognised {what}: {error})"
    ))
}

/// The body of a `DeleteObjects` request for `keys` (at most [`DELETE_MAX`]), in quiet mode so
/// the answer names only the failures. Every key must pass [`is_xml_safe`].
pub(crate) fn delete_request(keys: &[&str]) -> String {
    debug_assert!(keys.len() <= DELETE_MAX);
    let mut xml = String::with_capacity(
        160 + keys
            .iter()
            .map(|key| key.len() + "<Object><Key></Key></Object>".len())
            .sum::<usize>(),
    );
    xml.push_str(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Quiet>true</Quiet>",
    );
    for key in keys {
        xml.push_str("<Object><Key>");
        escape_into(&mut xml, key);
        xml.push_str("</Key></Object>");
    }
    xml.push_str("</Delete>");
    xml
}

/// Whether `key` can be written in an XML 1.0 document at all. Control characters other than
/// tab, line feed and carriage return cannot, not even as character references. Such a key is
/// deleted with its own request instead.
pub(crate) fn is_xml_safe(key: &str) -> bool {
    key.chars().all(|c| {
        !(c < ' ' && !matches!(c, '\t' | '\n' | '\r')) && c != '\u{FFFE}' && c != '\u{FFFF}'
    })
}

/// Text for element content. Tab, line feed and carriage return are written as character
/// references because an XML parser would normalise them away (a `\r` into a `\n`).
fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

    fn list_doc(inner: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\"?><ListBucketResult xmlns=\"{NS}\">{inner}</ListBucketResult>"
        )
        .into_bytes()
    }

    #[test]
    fn a_listing_is_read_whatever_the_order_of_its_elements() {
        let body = list_doc(
            "<Name>b</Name><IsTruncated>true</IsTruncated>\
             <NextContinuationToken>tok</NextContinuationToken>\
             <Contents><Key>a.txt</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified>\
             <ETag>&quot;abc&quot;</ETag><Size>12</Size><StorageClass>STANDARD</StorageClass></Contents>\
             <CommonPrefixes><Prefix>dir/</Prefix></CommonPrefixes>\
             <Contents><Key>b.txt</Key></Contents>\
             <CommonPrefixes><Prefix>other/</Prefix></CommonPrefixes>",
        );
        let page = parse_list(&body, 200).unwrap();
        assert!(page.truncated);
        assert_eq!(page.next_token.as_deref(), Some("tok"));
        assert!(!page.url_encoded);
        assert_eq!(page.prefixes, ["dir/", "other/"]);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "a.txt");
        assert_eq!(page.objects[0].etag.as_deref(), Some("\"abc\""));
        assert_eq!(page.objects[0].size, 12);
        assert_eq!(page.objects[1].size, 0);
    }

    #[test]
    fn an_empty_listing_is_empty() {
        let page = parse_list(&list_doc("<IsTruncated>false</IsTruncated>"), 200).unwrap();
        assert_eq!(page, ListPage::default());
        let page = parse_list(&list_doc(""), 200).unwrap();
        assert_eq!(page, ListPage::default());
    }

    #[test]
    fn white_space_in_a_key_is_kept() {
        let page = parse_list(
            &list_doc("<Contents><Key>  two  words </Key></Contents>"),
            200,
        )
        .unwrap();
        assert_eq!(page.objects[0].key, "  two  words ");
    }

    #[test]
    fn escapes_in_keys_are_undone() {
        let page = parse_list(
            &list_doc(
                "<Contents><Key>a&amp;b&lt;c&gt;&quot;d&apos;&#233;&#x1F600;</Key></Contents>",
            ),
            200,
        )
        .unwrap();
        assert_eq!(page.objects[0].key, "a&b<c>\"d'é😀");
    }

    #[test]
    fn url_encoding_is_reported() {
        let page = parse_list(&list_doc("<EncodingType>url</EncodingType>"), 200).unwrap();
        assert!(page.url_encoded);
    }

    #[test]
    fn a_document_of_the_wrong_kind_is_an_error_not_an_empty_listing() {
        let wrong = b"<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>";
        assert!(matches!(
            parse_list(wrong, 200),
            Err(ProviderError::Permanent(m)) if m.contains("not an S3 listing")
        ));
        assert!(matches!(
            parse_list(b"<html><body>Sign in</body></html>", 200),
            Err(ProviderError::Permanent(_))
        ));
        assert!(matches!(
            parse_list(b"", 200),
            Err(ProviderError::Permanent(_))
        ));
        assert!(matches!(
            parse_list(b"not xml at all", 200),
            Err(ProviderError::Permanent(_))
        ));
    }

    #[test]
    fn an_error_document_with_a_good_status_is_still_that_error() {
        let body = b"<Error><Code>SlowDown</Code><Message>Please reduce</Message></Error>";
        assert_eq!(
            parse_list(body, 200),
            Err(ProviderError::RateLimited { retry_after: None })
        );
    }

    #[test]
    fn error_documents_are_read_leniently() {
        let doc = parse_error(
            b"<?xml version=\"1.0\"?><Error><Code> NoSuchKey </Code><Message>Gone</Message>\
              <RequestId>X</RequestId><Resource>/b/k</Resource></Error>",
        );
        assert_eq!(doc.code.as_deref(), Some("NoSuchKey"));
        assert_eq!(doc.message.as_deref(), Some("Gone"));
        for junk in [&b""[..], b"<html>503</html>", b"plain text", b"<Error/>"] {
            assert_eq!(parse_error(junk), ErrorDoc::default(), "{junk:?}");
        }
    }

    #[test]
    fn buckets_are_listed_by_name() {
        let body = format!(
            "<ListAllMyBucketsResult xmlns=\"{NS}\"><Owner><ID>x</ID></Owner><Buckets>\
             <Bucket><Name>alpha</Name><CreationDate>2020-01-01T00:00:00.000Z</CreationDate></Bucket>\
             <Bucket><Name>beta</Name></Bucket></Buckets></ListAllMyBucketsResult>"
        );
        assert_eq!(
            parse_buckets(body.as_bytes(), 200).unwrap(),
            ["alpha", "beta"]
        );
        for none in [
            "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>",
            "<ListAllMyBucketsResult><Buckets></Buckets></ListAllMyBucketsResult>",
            "<ListAllMyBucketsResult></ListAllMyBucketsResult>",
        ] {
            assert_eq!(
                parse_buckets(none.as_bytes(), 200).unwrap(),
                Vec::<String>::new(),
                "{none}"
            );
        }
    }

    #[test]
    fn delete_failures_are_read_and_successes_ignored() {
        let body = b"<DeleteResult xmlns=\"x\"><Deleted><Key>a</Key></Deleted>\
                     <Error><Key>b</Key><Code>AccessDenied</Code><Message>No</Message></Error>\
                     <Error><Code>InternalError</Code></Error></DeleteResult>";
        let failures = parse_delete_result(body, 200).unwrap();
        assert_eq!(
            failures,
            [
                DeleteFailure {
                    key: Some("b".into()),
                    code: Some("AccessDenied".into()),
                    message: Some("No".into())
                },
                DeleteFailure {
                    key: None,
                    code: Some("InternalError".into()),
                    message: None
                },
            ]
        );
        assert!(
            parse_delete_result(b"<DeleteResult/>", 200)
                .unwrap()
                .is_empty()
        );
        assert!(parse_delete_result(b"<oops/>", 200).is_err());
    }

    #[test]
    fn the_delete_request_names_every_key_escaped() {
        let xml = delete_request(&["a.txt", "q&a/<b>.txt", "tab\there", "line\r\nbreak", "\"'"]);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Delete xmlns="));
        assert!(xml.contains("<Quiet>true</Quiet>"));
        assert!(xml.contains("<Object><Key>a.txt</Key></Object>"));
        assert!(xml.contains("<Key>q&amp;a/&lt;b&gt;.txt</Key>"));
        assert!(xml.contains("<Key>tab&#9;here</Key>"));
        assert!(xml.contains("<Key>line&#13;&#10;break</Key>"));
        assert!(xml.contains("<Key>&quot;&apos;</Key>"));
        assert!(xml.ends_with("</Delete>"));
        assert_eq!(xml.matches("<Object>").count(), 5);
    }

    /// What a service reads back from the request is what was meant.
    #[test]
    fn the_delete_request_round_trips_through_an_xml_parser() {
        #[derive(Deserialize)]
        struct Req {
            #[serde(rename = "Object")]
            objects: Vec<Obj>,
        }
        #[derive(Deserialize)]
        struct Obj {
            #[serde(rename = "Key")]
            key: String,
        }
        let keys = [
            "a&b",
            "<x>",
            "line\r\nbreak",
            "tab\there",
            "ü 😀",
            "  padded  ",
        ];
        let parsed: Req = quick_xml::de::from_str(&delete_request(&keys)).unwrap();
        let back: Vec<&str> = parsed.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(back, keys);
    }

    #[test]
    fn keys_xml_cannot_hold_are_recognised() {
        assert!(is_xml_safe("plain/ü/😀 name\t.txt"));
        assert!(is_xml_safe("line\nbreak"));
        for bad in [
            "a\u{0}b",
            "a\u{1}",
            "x\u{8}y",
            "\u{1F}",
            "a\u{FFFE}",
            "a\u{FFFF}",
        ] {
            assert!(!is_xml_safe(bad), "{bad:?}");
        }
    }
}
