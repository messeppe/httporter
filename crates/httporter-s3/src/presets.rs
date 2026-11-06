// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where each S3-compatible service lives, and how a bucket or object turns into a URL.
//! Quirks per service are recorded in `docs/notes/s3-presets.md`.

use httporter_core::{RemotePath, S3Preset, encode_segment};
use url::{Host, Url};

/// Amazon S3 Regions (commercial and GovCloud), from the AWS "Amazon S3 endpoints and quotas"
/// page, nearest-first within each continent.
pub const REGIONS_AWS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "ca-west-1",
    "mx-central-1",
    "sa-east-1",
    "eu-central-1",
    "eu-central-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-north-1",
    "eu-south-1",
    "eu-south-2",
    "il-central-1",
    "me-central-1",
    "me-south-1",
    "af-south-1",
    "ap-east-1",
    "ap-east-2",
    "ap-south-1",
    "ap-south-2",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-southeast-3",
    "ap-southeast-4",
    "ap-southeast-5",
    "ap-southeast-6",
    "ap-southeast-7",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "us-gov-east-1",
    "us-gov-west-1",
];

/// Wasabi storage regions, from the Wasabi "service URLs" page.
pub const REGIONS_WASABI: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-central-1",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "eu-central-1",
    "eu-central-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-south-1",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-southeast-1",
    "ap-southeast-2",
];

/// DigitalOcean Spaces regions (Standard storage), from the DigitalOcean availability matrix.
pub const REGIONS_SPACES: &[&str] = &[
    "nyc3", "ams3", "sfo2", "sfo3", "sgp1", "lon1", "fra1", "tor1", "blr1", "syd1", "atl1", "ric1",
    "mkc1",
];

/// Backblaze B2 S3 regions. An account lives in exactly one; the console's "S3 Endpoint" field
/// shows which.
pub const REGIONS_B2: &[&str] = &[
    "us-west-001",
    "us-west-002",
    "us-west-004",
    "us-east-005",
    "eu-central-003",
    "ca-east-006",
];

/// Where an S3 service answers: its address without the bucket, how buckets are addressed, and
/// the region requests are signed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `scheme://host[:port]/`, without the bucket.
    pub base: Url,
    /// The bucket goes in the path (`base/bucket/key`) rather than the host name
    /// (`bucket.host/key`). See [`object_url`] for when virtual-hosted addressing is not
    /// possible even though this is `false`.
    pub path_style: bool,
    /// The region in the credential scope of every signature.
    pub region: String,
}

const ASK_REGION: &str = "Choose a region.";
const BAD_REGION: &str = "Region names use lowercase letters, digits and hyphens, like eu-west-1.";
const ASK_ACCOUNT: &str = "Enter your Cloudflare account ID.";
const BAD_ACCOUNT: &str = "The account ID is made of letters and digits only.";
const ASK_ENDPOINT: &str = "Enter the server address, like https://s3.example.com.";
const BAD_SCHEME: &str = "Use an http:// or https:// address.";
const BAD_CREDENTIALS: &str = "Leave the user name and password out of the address.";
const BAD_PATH: &str = "Enter only the server address, without a bucket or path.";
const BAD_ADDRESS: &str = "That address is not valid.";

/// The signing region of a service whose address does not name one (MinIO, other S3 services).
const DEFAULT_REGION: &str = "us-east-1";

/// Where `preset` answers, from the fields the Add Site sheet shows for it. Only the arguments
/// a preset uses are read and checked; the rest are ignored.
///
/// | preset | uses | addressing |
/// | --- | --- | --- |
/// | Amazon S3 | `region` (`cn-*` regions get the `.com.cn` domain) | virtual-hosted |
/// | Cloudflare R2 | `account_id`; the region is always `auto` | path |
/// | Backblaze B2, Wasabi, DigitalOcean Spaces | `region` | path |
/// | MinIO | `custom`; `region` defaults to `us-east-1` | path |
/// | Other S3 | `custom`, `path_style`; `region` defaults to `us-east-1` | the toggle |
///
/// The error is a short sentence meant for the form field.
pub fn endpoint(
    preset: S3Preset,
    region: &str,
    account_id: &str,
    custom: &str,
    path_style: bool,
) -> Result<Endpoint, String> {
    let (base, path_style, region) = match preset {
        S3Preset::AwsS3 => {
            let region = named_region(region)?;
            let domain = if region.starts_with("cn-") {
                "amazonaws.com.cn"
            } else {
                "amazonaws.com"
            };
            (https_host(&format!("s3.{region}.{domain}"))?, false, region)
        }
        S3Preset::CloudflareR2 => {
            let account = account_id.trim();
            if account.is_empty() {
                return Err(ASK_ACCOUNT.to_owned());
            }
            if !account.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(BAD_ACCOUNT.to_owned());
            }
            let host = format!("{account}.r2.cloudflarestorage.com");
            (https_host(&host)?, true, "auto".to_owned())
        }
        S3Preset::BackblazeB2 => {
            let region = named_region(region)?;
            (
                https_host(&format!("s3.{region}.backblazeb2.com"))?,
                true,
                region,
            )
        }
        S3Preset::Wasabi => {
            let region = named_region(region)?;
            (
                https_host(&format!("s3.{region}.wasabisys.com"))?,
                true,
                region,
            )
        }
        S3Preset::DigitalOceanSpaces => {
            let region = named_region(region)?;
            (
                https_host(&format!("{region}.digitaloceanspaces.com"))?,
                true,
                region,
            )
        }
        S3Preset::Minio => (custom_base(custom)?, true, optional_region(region)?),
        S3Preset::Other => (custom_base(custom)?, path_style, optional_region(region)?),
    };
    Ok(Endpoint {
        base,
        path_style,
        region,
    })
}

/// The URL of a bucket: `https://bucket.host/` when the endpoint is virtual-hosted, otherwise
/// `https://host/bucket`. Use it for bucket-level requests (listing, `?delete`, …). An empty
/// `bucket` gives the endpoint itself: the service root, where the list of buckets lives.
pub fn bucket_url(ep: &Endpoint, bucket: &str) -> Url {
    address(ep, bucket, None)
}

/// The URL of an object. Every key segment is percent-encoded with S3's rules (see
/// [`crate::canonical_uri`]), so the signer sees the path the server will. The key never
/// carries a trailing slash; a folder marker (`name/`) needs one appended to the path. The root
/// key is the bucket itself.
///
/// An empty `bucket` means the endpoint already is one bucket (a custom domain in front of it):
/// the key follows the host directly, `base/key`, whatever the addressing style. The key is
/// never dropped, so a request meant for an object cannot turn into a request for the service
/// root.
///
/// A virtual-hosted endpoint still falls back to path style for a bucket the host name cannot
/// carry: one with a dot over HTTPS (the `*.s3.amazonaws.com` certificate does not match
/// `a.b.s3.amazonaws.com`), with characters a host name does not allow (upper case, `_`,
/// spaces), or when the endpoint is an IP address.
pub fn object_url(ep: &Endpoint, bucket: &str, key: &RemotePath) -> Url {
    address(ep, bucket, Some(key))
}

fn address(ep: &Endpoint, bucket: &str, key: Option<&RemotePath>) -> Url {
    let key_path = key.map(|k| k.to_url_path(false)).unwrap_or_default();
    let mut url = ep.base.clone();
    if bucket.is_empty() {
        if !key_path.is_empty() {
            url.set_path(&format!("/{key_path}"));
        }
        return url;
    }
    if bucket_fits_in_host(ep, bucket) {
        let host = format!("{bucket}.{}", ep.base.host_str().unwrap_or_default());
        if url.set_host(Some(&host)).is_ok() {
            url.set_path(&format!("/{key_path}"));
            return url;
        }
    }
    let mut path = format!("/{}", encode_segment(bucket));
    if !key_path.is_empty() {
        path.push('/');
        path.push_str(&key_path);
    }
    url.set_path(&path);
    url
}

fn bucket_fits_in_host(ep: &Endpoint, bucket: &str) -> bool {
    let https = ep.base.scheme() == "https";
    let letters = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !ep.path_style
        && matches!(ep.base.host(), Some(Host::Domain(_)))
        && bucket.len() <= 63
        && bucket
            .bytes()
            .all(|b| letters(b) || b == b'-' || (b == b'.' && !https))
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.'])
        && !bucket.contains("..")
}

/// A region the user must choose: lower-case letters, digits and hyphens.
fn named_region(text: &str) -> Result<String, String> {
    let region = text.trim();
    if region.is_empty() {
        return Err(ASK_REGION.to_owned());
    }
    if !region
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(BAD_REGION.to_owned());
    }
    Ok(region.to_owned())
}

/// A region the user may leave empty, which then means `us-east-1`.
fn optional_region(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        Ok(DEFAULT_REGION.to_owned())
    } else {
        named_region(text)
    }
}

fn https_host(host: &str) -> Result<Url, String> {
    Url::parse(&format!("https://{host}/")).map_err(|_| BAD_ADDRESS.to_owned())
}

/// The address a user typed for MinIO or another service: `http://` or `https://`, a host and
/// an optional port, nothing else.
fn custom_base(text: &str) -> Result<Url, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ASK_ENDPOINT.to_owned());
    }
    let url = Url::parse(text).map_err(|_| BAD_SCHEME.to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(BAD_SCHEME.to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BAD_CREDENTIALS.to_owned());
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(BAD_PATH.to_owned());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sigv4::{canonical_path, canonical_uri};

    fn made(preset: S3Preset, region: &str, account: &str, custom: &str, path: bool) -> Endpoint {
        endpoint(preset, region, account, custom, path).unwrap()
    }

    fn failed(preset: S3Preset, region: &str, account: &str, custom: &str) -> String {
        endpoint(preset, region, account, custom, true).unwrap_err()
    }

    fn key(text: &str) -> RemotePath {
        RemotePath::parse(text).unwrap()
    }

    fn assert_endpoint(ep: &Endpoint, base: &str, path_style: bool, region: &str) {
        assert_eq!(ep.base.as_str(), base);
        assert_eq!(ep.path_style, path_style, "path_style of {base}");
        assert_eq!(ep.region, region, "region of {base}");
    }

    #[test]
    fn preset_endpoints() {
        use S3Preset::*;
        assert_endpoint(
            &made(AwsS3, "eu-west-1", "", "", false),
            "https://s3.eu-west-1.amazonaws.com/",
            false,
            "eu-west-1",
        );
        assert_endpoint(
            &made(CloudflareR2, "", "abc", "", false),
            "https://abc.r2.cloudflarestorage.com/",
            true,
            "auto",
        );
        assert_endpoint(
            &made(BackblazeB2, "us-west-004", "", "", false),
            "https://s3.us-west-004.backblazeb2.com/",
            true,
            "us-west-004",
        );
        assert_endpoint(
            &made(Wasabi, "us-east-1", "", "", false),
            "https://s3.us-east-1.wasabisys.com/",
            true,
            "us-east-1",
        );
        assert_endpoint(
            &made(DigitalOceanSpaces, "sgp1", "", "", false),
            "https://sgp1.digitaloceanspaces.com/",
            true,
            "sgp1",
        );
        assert_endpoint(
            &made(Minio, "", "", "http://localhost:9000", false),
            "http://localhost:9000/",
            true,
            "us-east-1",
        );
        assert_eq!(
            failed(Other, "", "", "ftp://x"),
            "Use an http:// or https:// address."
        );
    }

    /// Only "Other S3" follows the path-style toggle; every named service has a fixed style.
    #[test]
    fn only_other_follows_the_path_style_toggle() {
        use S3Preset::*;
        for toggle in [false, true] {
            assert!(!made(AwsS3, "us-east-1", "", "", toggle).path_style);
            assert!(made(CloudflareR2, "", "abc", "", toggle).path_style);
            assert!(made(BackblazeB2, "us-west-004", "", "", toggle).path_style);
            assert!(made(Wasabi, "us-east-1", "", "", toggle).path_style);
            assert!(made(DigitalOceanSpaces, "nyc3", "", "", toggle).path_style);
            assert!(made(Minio, "", "", "http://localhost:9000", toggle).path_style);
            assert_eq!(
                made(Other, "", "", "https://s3.example.com", toggle).path_style,
                toggle
            );
        }
    }

    /// Fields that do not belong to a preset are ignored, not validated.
    #[test]
    fn arguments_a_preset_does_not_use_are_ignored() {
        use S3Preset::*;
        let ep = made(AwsS3, "eu-west-1", "not an id!", "ftp://nonsense", true);
        assert_endpoint(
            &ep,
            "https://s3.eu-west-1.amazonaws.com/",
            false,
            "eu-west-1",
        );
        let ep = made(CloudflareR2, "eu-west-1", "abc", "ftp://nonsense", true);
        assert_endpoint(&ep, "https://abc.r2.cloudflarestorage.com/", true, "auto");
        let ep = made(Minio, "", "not an id!", "http://localhost:9000", true);
        assert_eq!(ep.base.as_str(), "http://localhost:9000/");
    }

    #[test]
    fn region_is_required_where_it_picks_the_host() {
        use S3Preset::*;
        for preset in [AwsS3, BackblazeB2, Wasabi, DigitalOceanSpaces] {
            assert_eq!(failed(preset, "", "", ""), "Choose a region.", "{preset:?}");
            assert_eq!(
                failed(preset, "  ", "", ""),
                "Choose a region.",
                "{preset:?}"
            );
            for bad in ["EU-West-1", "eu west", "eu.west-1", "eu-west-1/x", "é"] {
                assert_eq!(
                    failed(preset, bad, "", ""),
                    "Region names use lowercase letters, digits and hyphens, like eu-west-1.",
                    "{preset:?} {bad:?}"
                );
            }
        }
        assert_eq!(
            made(Wasabi, " us-east-1 ", "", "", true).region,
            "us-east-1"
        );
    }

    #[test]
    fn r2_needs_an_account_id_made_of_letters_and_digits() {
        use S3Preset::*;
        assert_eq!(
            failed(CloudflareR2, "", "", ""),
            "Enter your Cloudflare account ID."
        );
        assert_eq!(
            failed(CloudflareR2, "", "  ", ""),
            "Enter your Cloudflare account ID."
        );
        for bad in [
            "abc.r2.cloudflarestorage.com",
            "https://abc",
            "a b",
            "abc/",
            "-abc",
        ] {
            assert_eq!(
                failed(CloudflareR2, "", bad, ""),
                "The account ID is made of letters and digits only.",
                "{bad:?}"
            );
        }
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            made(CloudflareR2, "", &format!(" {id} "), "", true)
                .base
                .host_str(),
            Some(format!("{id}.r2.cloudflarestorage.com").as_str())
        );
    }

    #[test]
    fn custom_endpoints_are_checked_and_normalised() {
        use S3Preset::*;
        for preset in [Minio, Other] {
            assert_eq!(
                failed(preset, "", "", ""),
                "Enter the server address, like https://s3.example.com."
            );
            assert_eq!(
                failed(preset, "", "", "   "),
                "Enter the server address, like https://s3.example.com."
            );
            for not_http in [
                "ftp://x",
                "s3.example.com",
                "localhost:9000",
                "//s3.example.com",
                "http://",
                "https:///",
                "file:///c:/x",
            ] {
                assert_eq!(
                    failed(preset, "", "", not_http),
                    "Use an http:// or https:// address.",
                    "{not_http:?}"
                );
            }
            for with_credentials in ["http://user:pw@localhost:9000", "https://user@h.example"] {
                assert_eq!(
                    failed(preset, "", "", with_credentials),
                    "Leave the user name and password out of the address.",
                    "{with_credentials:?}"
                );
            }
            for with_path in [
                "http://localhost:9000/bucket",
                "https://h.example/a/b/",
                "https://h.example/?x=1",
                "https://h.example/#top",
            ] {
                assert_eq!(
                    failed(preset, "", "", with_path),
                    "Enter only the server address, without a bucket or path.",
                    "{with_path:?}"
                );
            }
        }
        // Whitespace, case, a lone trailing slash and a default port all normalise away.
        assert_eq!(
            made(Other, "", "", "  HTTP://LocalHost:9000/  ", true)
                .base
                .as_str(),
            "http://localhost:9000/"
        );
        assert_eq!(
            made(Other, "", "", "https://S3.Example.com:443", true)
                .base
                .as_str(),
            "https://s3.example.com/"
        );
        assert_eq!(
            made(Other, "", "", "http://192.168.1.5:9000", true)
                .base
                .as_str(),
            "http://192.168.1.5:9000/"
        );
    }

    #[test]
    fn minio_and_other_default_the_region_to_us_east_1() {
        use S3Preset::*;
        assert_eq!(
            made(Minio, "", "", "http://localhost:9000", true).region,
            "us-east-1"
        );
        assert_eq!(
            made(Other, "  ", "", "https://h.example", true).region,
            "us-east-1"
        );
        assert_eq!(
            made(Minio, "eu-1", "", "http://localhost:9000", true).region,
            "eu-1"
        );
        // A real service behind "Other" may use any region name, including R2's "auto".
        assert_eq!(
            made(Other, "auto", "", "https://h.example", true).region,
            "auto"
        );
        assert_eq!(
            failed(Other, "Bad Region", "", "https://h.example"),
            "Region names use lowercase letters, digits and hyphens, like eu-west-1."
        );
    }

    #[test]
    fn china_regions_use_the_cn_domain() {
        let ep = made(S3Preset::AwsS3, "cn-north-1", "", "", false);
        assert_eq!(ep.base.as_str(), "https://s3.cn-north-1.amazonaws.com.cn/");
        let ep = made(S3Preset::AwsS3, "cn-northwest-1", "", "", false);
        assert_eq!(
            ep.base.as_str(),
            "https://s3.cn-northwest-1.amazonaws.com.cn/"
        );
    }

    #[test]
    fn region_lists_are_clean_and_every_entry_makes_an_endpoint() {
        let lists: [(&str, S3Preset, &[&str]); 4] = [
            ("aws", S3Preset::AwsS3, REGIONS_AWS),
            ("wasabi", S3Preset::Wasabi, REGIONS_WASABI),
            ("spaces", S3Preset::DigitalOceanSpaces, REGIONS_SPACES),
            ("b2", S3Preset::BackblazeB2, REGIONS_B2),
        ];
        for (name, preset, regions) in lists {
            assert!(!regions.is_empty(), "{name}");
            let mut seen = std::collections::HashSet::new();
            for region in regions {
                assert!(seen.insert(*region), "{name}: {region} twice");
                let ep = made(preset, region, "", "", false);
                assert_eq!(ep.region, *region);
            }
        }
        for (list, wanted) in [
            (REGIONS_AWS, "us-east-1"),
            (REGIONS_AWS, "eu-west-1"),
            (REGIONS_WASABI, "us-east-1"),
            (REGIONS_SPACES, "sgp1"),
            (REGIONS_B2, "us-west-004"),
        ] {
            assert!(list.contains(&wanted), "{wanted}");
        }
    }

    #[test]
    fn virtual_hosted_urls_put_the_bucket_in_the_host() {
        let ep = made(S3Preset::AwsS3, "eu-west-1", "", "", false);
        assert_eq!(
            bucket_url(&ep, "my-bucket").as_str(),
            "https://my-bucket.s3.eu-west-1.amazonaws.com/"
        );
        assert_eq!(
            object_url(&ep, "my-bucket", &key("a b/ü.txt")).as_str(),
            "https://my-bucket.s3.eu-west-1.amazonaws.com/a%20b/%C3%BC.txt"
        );
        assert_eq!(
            object_url(&ep, "my-bucket", &RemotePath::root()),
            bucket_url(&ep, "my-bucket")
        );
    }

    #[test]
    fn path_style_urls_put_the_bucket_in_the_path() {
        let ep = made(S3Preset::CloudflareR2, "", "abc", "", true);
        assert_eq!(
            bucket_url(&ep, "my-bucket").as_str(),
            "https://abc.r2.cloudflarestorage.com/my-bucket"
        );
        assert_eq!(
            object_url(&ep, "my-bucket", &key("a b/ü.txt")).as_str(),
            "https://abc.r2.cloudflarestorage.com/my-bucket/a%20b/%C3%BC.txt"
        );
        assert_eq!(
            object_url(&ep, "my-bucket", &RemotePath::root()),
            bucket_url(&ep, "my-bucket")
        );
        let minio = made(S3Preset::Minio, "", "", "http://localhost:9000", true);
        assert_eq!(
            object_url(&minio, "photos", &key("2024/cat.jpg")).as_str(),
            "http://localhost:9000/photos/2024/cat.jpg"
        );
    }

    /// No bucket means the endpoint is the whole address (a custom domain that already is one
    /// bucket): the key follows the host, in either addressing style. It is never dropped.
    #[test]
    fn an_empty_bucket_addresses_the_endpoint_itself() {
        for ep in [
            made(S3Preset::AwsS3, "eu-west-1", "", "", false),
            made(S3Preset::CloudflareR2, "", "abc", "", true),
        ] {
            let base = ep.base.as_str();
            assert_eq!(bucket_url(&ep, "").as_str(), base);
            assert_eq!(object_url(&ep, "", &RemotePath::root()).as_str(), base);
            assert_eq!(
                object_url(&ep, "", &key("a b/ü.txt")).as_str(),
                format!("{base}a%20b/%C3%BC.txt")
            );
        }
    }

    /// `Other` with the toggle off is virtual-hosted, on any scheme and port.
    #[test]
    fn other_virtual_hosted_keeps_scheme_and_port() {
        let ep = made(S3Preset::Other, "", "", "http://s3.lan:9000", false);
        assert_eq!(
            object_url(&ep, "photos", &key("cat.jpg")).as_str(),
            "http://photos.s3.lan:9000/cat.jpg"
        );
    }

    /// A bucket the host name cannot carry is reached by path instead of failing: dots break the
    /// HTTPS certificate (`*.s3.amazonaws.com` does not match `a.b.s3...`), upper case and other
    /// characters are not valid in a host name, and an IP address has no sub-domains.
    #[test]
    fn buckets_that_cannot_be_a_host_name_fall_back_to_path_style() {
        let aws = made(S3Preset::AwsS3, "eu-west-1", "", "", false);
        assert_eq!(
            object_url(&aws, "static.example.com", &key("k")).as_str(),
            "https://s3.eu-west-1.amazonaws.com/static.example.com/k"
        );
        assert_eq!(
            bucket_url(&aws, "My_Bucket").as_str(),
            "https://s3.eu-west-1.amazonaws.com/My_Bucket"
        );
        assert_eq!(
            bucket_url(&aws, "my bucket").as_str(),
            "https://s3.eu-west-1.amazonaws.com/my%20bucket"
        );
        assert_eq!(
            bucket_url(&aws, "-bad").as_str(),
            "https://s3.eu-west-1.amazonaws.com/-bad"
        );
        assert_eq!(
            bucket_url(&aws, "").as_str(),
            "https://s3.eu-west-1.amazonaws.com/"
        );
        // Without TLS there is no certificate to mismatch, so dots are fine.
        let lan = made(S3Preset::Other, "", "", "http://s3.lan", false);
        assert_eq!(bucket_url(&lan, "a.b").as_str(), "http://a.b.s3.lan/");
        let ip = made(S3Preset::Other, "", "", "http://127.0.0.1:9000", false);
        assert!(!ip.path_style);
        assert_eq!(
            object_url(&ip, "photos", &key("k")).as_str(),
            "http://127.0.0.1:9000/photos/k"
        );
        let v6 = made(S3Preset::Other, "", "", "http://[::1]:9000", false);
        assert_eq!(
            object_url(&v6, "photos", &key("k")).as_str(),
            "http://[::1]:9000/photos/k"
        );
    }

    #[test]
    fn awkward_keys_never_leak_into_the_query_or_fragment() {
        let ep = made(S3Preset::CloudflareR2, "", "abc", "", true);
        let url = object_url(&ep, "b", &key("100%/a+b/c#d/e?f/g&h=i/j;k/l$m/~n/_o/.p"));
        assert_eq!(
            url.path(),
            "/b/100%25/a%2Bb/c%23d/e%3Ff/g%26h%3Di/j%3Bk/l%24m/~n/_o/.p"
        );
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);
    }

    /// The signer re-derives the canonical path from the URL; it must equal what
    /// `canonical_uri` makes of the raw path, or every awkward key would fail with
    /// `SignatureDoesNotMatch`.
    #[test]
    fn signing_sees_the_same_path_the_url_carries() {
        let keys = [
            "plain.txt",
            "a b/c#d%e?f+g/ü.txt",
            "100%",
            "a+b",
            "x$y&z=w;v,u",
            "(paren)/[brk]/{brace}/'quote'/!bang/*star",
            "~tilde/_under/-dash/.dot",
            "日本語/😀.png",
            "a\\b",
            "50%25",
        ];
        let virtual_ep = made(S3Preset::AwsS3, "us-east-1", "", "", false);
        let path_ep = made(S3Preset::Minio, "", "", "http://localhost:9000", true);
        for text in keys {
            let k = key(text);
            let url = object_url(&virtual_ep, "bkt", &k);
            assert_eq!(
                canonical_path(&url),
                canonical_uri(&format!("/{text}")),
                "{text}"
            );
            let url = object_url(&path_ep, "bkt", &k);
            assert_eq!(
                canonical_path(&url),
                canonical_uri(&format!("/bkt/{text}")),
                "{text}"
            );
        }
    }
}
