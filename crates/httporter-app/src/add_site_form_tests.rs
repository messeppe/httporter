// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task 21: every kind of Add Site form (`validate`), the S3 address preview, the secret's
//! redaction, and the fields Edit Site starts from.

use chrono::Utc;
use httporter_core::{ProviderKind, S3Site, SiteId};

use super::*;

fn s3(preset: S3Preset) -> SiteForm {
    SiteForm::S3 {
        preset,
        region: String::new(),
        account_id: String::new(),
        endpoint: String::new(),
        path_style: false,
        bucket: String::new(),
        key_id: String::new(),
        secret: String::new(),
    }
}

fn filled(preset: S3Preset, region: &str, account_id: &str, endpoint: &str) -> SiteForm {
    SiteForm::S3 {
        preset,
        region: region.into(),
        account_id: account_id.into(),
        endpoint: endpoint.into(),
        path_style: false,
        bucket: "media".into(),
        key_id: "AKIDEXAMPLE".into(),
        secret: "s3cret-key".into(),
    }
}

fn problems(form: &SiteForm, editing: bool) -> Vec<(&'static str, String)> {
    validate(form, editing).err().unwrap_or_default()
}

fn owned(list: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    list.iter().map(|(f, m)| (*f, (*m).to_owned())).collect()
}

#[test]
fn validate_each_form_kind_and_edit_keeps_secret() {
    // Bunny with a zone password: every problem at once, in field order.
    let empty = SiteForm::bunny("", -1, "");
    assert_eq!(
        problems(&empty, false),
        owned(&[
            (field::ZONE, "Enter the storage zone name."),
            (field::REGION, "Choose a region."),
            (field::PASSWORD, "Enter the zone's password."),
        ])
    );
    // Editing: an empty password keeps the saved one.
    assert_eq!(
        problems(&empty, true),
        owned(&[
            (field::ZONE, "Enter the storage zone name."),
            (field::REGION, "Choose a region."),
        ])
    );
    assert_eq!(
        problems(&SiteForm::bunny("my zone", 9, "pw"), false),
        owned(&[
            (field::ZONE, "Zone names can't contain spaces."),
            (field::REGION, "Choose a region."),
        ])
    );
    assert_eq!(
        validate(&SiteForm::bunny(" testzone ", 4, "pw"), false),
        Ok(())
    );
    assert_eq!(validate(&SiteForm::bunny("testzone", 4, ""), true), Ok(()));

    // The account key.
    let key = |key: &str| SiteForm::BunnyAccount { key: key.into() };
    assert_eq!(
        problems(&key("  "), false),
        owned(&[(field::ACCOUNT_KEY, "Enter your Bunny account key.")])
    );
    assert_eq!(validate(&key(""), true), Ok(()));
    assert_eq!(validate(&key("k-1"), false), Ok(()));

    // S3: the address fields of the preset, the bucket, the access key ID and the secret.
    assert_eq!(
        problems(&s3(S3Preset::AwsS3), false),
        owned(&[
            (field::REGION, "Choose a region."),
            (field::BUCKET, "Enter the bucket name."),
            (field::KEY_ID, "Enter the access key ID."),
            (field::SECRET, "Enter the secret access key."),
        ])
    );
    // Editing keeps an empty secret, but the access key ID is still needed.
    assert_eq!(
        problems(&s3(S3Preset::Wasabi), true),
        owned(&[
            (field::REGION, "Choose a region."),
            (field::BUCKET, "Enter the bucket name."),
            (field::KEY_ID, "Enter the access key ID."),
        ])
    );
    assert_eq!(
        problems(&s3(S3Preset::CloudflareR2), true)[0],
        (
            field::ACCOUNT_ID,
            "Enter your Cloudflare account ID.".to_owned()
        )
    );
    assert_eq!(
        problems(&filled(S3Preset::CloudflareR2, "", "abc-123", ""), false),
        owned(&[(
            field::ACCOUNT_ID,
            "The account ID is made of letters and digits only."
        )])
    );
    assert_eq!(
        problems(&s3(S3Preset::Minio), true)[0],
        (
            field::ENDPOINT,
            "Enter the server address, like https://s3.example.com.".to_owned()
        )
    );
    assert_eq!(
        problems(&filled(S3Preset::Other, "", "", "ftp://x"), false),
        owned(&[(field::ENDPOINT, "Use an http:// or https:// address.")])
    );
    // Other S3 with a good address and a bad region blames the region.
    assert_eq!(
        problems(
            &filled(S3Preset::Other, "EU West", "", "https://s3.example.com"),
            false
        ),
        owned(&[(
            field::REGION,
            "Region names use lowercase letters, digits and hyphens, like eu-west-1."
        )])
    );
    for form in [
        filled(S3Preset::AwsS3, "eu-west-1", "", ""),
        filled(S3Preset::CloudflareR2, "", "abc123", ""),
        filled(S3Preset::BackblazeB2, "us-west-004", "", ""),
        filled(S3Preset::Wasabi, "eu-central-1", "", ""),
        filled(S3Preset::DigitalOceanSpaces, "nyc3", "", ""),
        filled(S3Preset::Minio, "", "", "http://localhost:9000"),
        filled(S3Preset::Other, "", "", "https://s3.example.com"),
    ] {
        assert_eq!(validate(&form, false), Ok(()), "{form:?}");
    }
}

#[test]
fn s3_endpoint_previews_per_preset() {
    use S3Preset::*;
    let preview = s3_endpoint_preview;
    // Amazon S3 puts the bucket in the host name.
    assert_eq!(
        preview(AwsS3, "eu-west-1", "", "", "my-bucket", false),
        "https://my-bucket.s3.eu-west-1.amazonaws.com/"
    );
    // A bucket with a dot can't be in an HTTPS host name.
    assert_eq!(
        preview(AwsS3, "eu-west-1", "", "", "my.bucket", false),
        "https://s3.eu-west-1.amazonaws.com/my.bucket"
    );
    // The other services put it in the path.
    assert_eq!(
        preview(CloudflareR2, "", "abc123", "", "media", false),
        "https://abc123.r2.cloudflarestorage.com/media"
    );
    assert_eq!(
        preview(BackblazeB2, "us-west-004", "", "", "media", false),
        "https://s3.us-west-004.backblazeb2.com/media"
    );
    assert_eq!(
        preview(Wasabi, "eu-central-1", "", "", "media", false),
        "https://s3.eu-central-1.wasabisys.com/media"
    );
    assert_eq!(
        preview(DigitalOceanSpaces, "nyc3", "", "", "media", false),
        "https://nyc3.digitaloceanspaces.com/media"
    );
    assert_eq!(
        preview(Minio, "", "", "http://localhost:9000", "media", false),
        "http://localhost:9000/media"
    );
    // Other S3 follows its path-style box.
    assert_eq!(
        preview(Other, "", "", "https://s3.example.com", "media", false),
        "https://media.s3.example.com/"
    );
    assert_eq!(
        preview(Other, "", "", "https://s3.example.com", "media", true),
        "https://s3.example.com/media"
    );
    // The bucket is trimmed; without one the service's own address shows.
    assert_eq!(
        preview(Wasabi, "eu-central-1", "", "", " media ", false),
        "https://s3.eu-central-1.wasabisys.com/media"
    );
    assert_eq!(
        preview(AwsS3, "eu-west-1", "", "", "", false),
        "https://s3.eu-west-1.amazonaws.com/"
    );
    // Nothing while the fields can't name an address.
    assert_eq!(preview(AwsS3, "", "", "", "media", false), "");
    assert_eq!(preview(CloudflareR2, "", "", "", "media", false), "");
    assert_eq!(preview(Minio, "", "", "localhost:9000", "media", false), "");
    assert_eq!(preview(Other, "", "", "", "media", true), "");
}

#[test]
fn the_sheets_s3_draft_becomes_a_form_with_its_secret() {
    let draft = SiteDraft {
        provider: 2,
        name: "Media".into(),
        account_id: "abc123".into(),
        bucket: "media".into(),
        key_id: "AKID".into(),
        ..SiteDraft::default()
    };
    assert_eq!(
        SiteForm::from_s3_draft(&draft, "s3cret"),
        Some(SiteForm::S3 {
            preset: S3Preset::CloudflareR2,
            region: String::new(),
            account_id: "abc123".into(),
            endpoint: String::new(),
            path_style: false,
            bucket: "media".into(),
            key_id: "AKID".into(),
            secret: "s3cret".into(),
        })
    );
    let bunny = SiteDraft {
        provider: 0,
        ..draft
    };
    assert_eq!(SiteForm::from_s3_draft(&bunny, "s3cret"), None);

    // The grid's tiles and the presets map both ways.
    assert_eq!(preset_for_tile(0), None);
    assert_eq!(preset_for_tile(8), None);
    assert_eq!(preset_for_tile(-1), None);
    for tile in 1..=7 {
        let preset = preset_for_tile(tile).unwrap();
        assert_eq!(tile_for(preset), tile, "{preset:?}");
    }
    assert_eq!(preset_for_tile(1), Some(S3Preset::AwsS3));
    assert_eq!(preset_for_tile(7), Some(S3Preset::Other));

    // An empty secret field takes the one Import found; a typed one stays.
    let empty = SiteForm::bunny("z", 0, "");
    assert!(empty.secret_is_empty());
    assert_eq!(
        empty.or_secret("imported"),
        SiteForm::bunny("z", 0, "imported")
    );
    assert_eq!(
        SiteForm::bunny("z", 0, "typed").or_secret("imported"),
        SiteForm::bunny("z", 0, "typed")
    );
    let key = SiteForm::BunnyAccount { key: String::new() }.or_secret("k");
    assert_eq!(key, SiteForm::BunnyAccount { key: "k".into() });
    let secret = s3(S3Preset::AwsS3).or_secret("sk");
    assert!(!secret.secret_is_empty());
}

#[test]
fn site_form_debug_never_shows_secrets() {
    for form in [
        SiteForm::bunny("zone-a", 4, "pw-hunter2"),
        SiteForm::BunnyAccount {
            key: "key-hunter2".into(),
        },
        filled(S3Preset::AwsS3, "eu-west-1", "", "").or_secret("unused"),
        SiteForm::S3 {
            preset: S3Preset::AwsS3,
            region: "eu-west-1".into(),
            account_id: String::new(),
            endpoint: String::new(),
            path_style: false,
            bucket: "media".into(),
            key_id: "AKIDEXAMPLE".into(),
            secret: "sk-hunter2".into(),
        },
    ] {
        for text in [format!("{form:?}"), format!("{form:#?}")] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("<hidden>"), "{text}");
        }
    }
    // The fields that are not secret are shown.
    let shown = format!("{:?}", filled(S3Preset::AwsS3, "eu-west-1", "", ""));
    assert!(
        shown.contains("AKIDEXAMPLE") && shown.contains("media"),
        "{shown}"
    );
}

#[test]
fn bunny_regions_are_found_by_code_or_city() {
    assert_eq!(bunny_region_index("de"), 0);
    assert_eq!(bunny_region_index("SG"), 4);
    assert_eq!(bunny_region_index(" Singapore "), 4);
    assert_eq!(bunny_region_index("São Paulo"), 6);
    assert_eq!(bunny_region_index("syd"), 8);
    assert_eq!(bunny_region_index("mars"), -1);
    assert_eq!(bunny_region_index(""), -1);
}

fn record(provider: ProviderKind, container: &str, region: &str, host: &str) -> SiteRecord {
    SiteRecord {
        id: SiteId::new(),
        name: "My site".into(),
        provider,
        container: container.into(),
        region: region.into(),
        host: host.into(),
        s3: None,
        created_at: Utc::now(),
    }
}

fn s3_record(
    preset: S3Preset,
    region: &str,
    host: &str,
    endpoint: &str,
    path_style: bool,
) -> SiteRecord {
    SiteRecord {
        s3: Some(S3Site {
            preset,
            endpoint: endpoint.into(),
            path_style,
        }),
        ..record(ProviderKind::S3, "media", region, host)
    }
}

#[test]
fn edit_site_starts_from_the_saved_fields_for_every_preset() {
    let bunny = draft_for(
        &record(
            ProviderKind::Bunny,
            "testzone",
            "sg",
            "sg.storage.bunnycdn.com",
        ),
        "",
    );
    assert_eq!(
        (
            bunny.provider,
            bunny.name.as_str(),
            bunny.zone.as_str(),
            bunny.region_index
        ),
        (0, "My site", "testzone", 4)
    );

    let draft = |site: &SiteRecord| draft_for(site, "AKID");
    let aws = draft(&s3_record(
        S3Preset::AwsS3,
        "eu-west-1",
        "s3.eu-west-1.amazonaws.com",
        "https://s3.eu-west-1.amazonaws.com",
        false,
    ));
    assert_eq!(
        (
            aws.provider,
            aws.region.as_str(),
            aws.bucket.as_str(),
            aws.key_id.as_str()
        ),
        (1, "eu-west-1", "media", "AKID")
    );
    let r2 = draft(&s3_record(
        S3Preset::CloudflareR2,
        "auto",
        "abc123.r2.cloudflarestorage.com",
        "https://abc123.r2.cloudflarestorage.com",
        true,
    ));
    assert_eq!(
        (r2.provider, r2.account_id.as_str(), r2.region.as_str()),
        (2, "abc123", "")
    );
    for (preset, tile, region) in [
        (S3Preset::BackblazeB2, 3, "us-west-004"),
        (S3Preset::Wasabi, 4, "eu-central-1"),
        (S3Preset::DigitalOceanSpaces, 5, "nyc3"),
    ] {
        let d = draft(&s3_record(preset, region, "h", "https://h", true));
        assert_eq!(
            (d.provider, d.region.as_str()),
            (tile, region),
            "{preset:?}"
        );
    }
    let minio = draft(&s3_record(
        S3Preset::Minio,
        "us-east-1",
        "localhost:9000",
        "http://localhost:9000",
        true,
    ));
    assert_eq!(
        (
            minio.provider,
            minio.endpoint.as_str(),
            minio.region.as_str()
        ),
        (6, "http://localhost:9000", "")
    );
    let other = draft(&s3_record(
        S3Preset::Other,
        "eu-1",
        "s3.example.com",
        "https://s3.example.com",
        true,
    ));
    assert_eq!(
        (
            other.provider,
            other.endpoint.as_str(),
            other.region.as_str(),
            other.path_style
        ),
        (7, "https://s3.example.com", "eu-1", true)
    );

    // Each draft, reported back by the sheet with an empty secret, is a valid edit naming the
    // same address.
    let form = SiteForm::from_s3_draft(&aws, "").unwrap();
    assert_eq!(validate(&form, true), Ok(()));
    assert_eq!(
        preview_of(&aws),
        "https://media.s3.eu-west-1.amazonaws.com/"
    );
    assert_eq!(
        preview_of(&r2),
        "https://abc123.r2.cloudflarestorage.com/media"
    );
    assert_eq!(preview_of(&other), "https://s3.example.com/media");
    assert_eq!(preview_of(&bunny), "", "Bunny has no S3 preview");
}

#[test]
fn an_s3_failure_blames_its_field() {
    assert_eq!(
        field_blamed(copy::S3_ACCESS_KEY_UNKNOWN),
        Some(field::KEY_ID)
    );
    assert_eq!(field_blamed(copy::S3_SECRET_WRONG), Some(field::SECRET));
    assert_eq!(field_blamed(copy::S3_REGION_WRONG), Some(field::REGION));
    assert_eq!(field_blamed(copy::CLOCK_OFF), None);
}

#[test]
fn an_s3_host_keeps_its_port() {
    let url = |text: &str| Url::parse(text).unwrap();
    assert_eq!(s3_host(&url("https://s3.example.com/")), "s3.example.com");
    assert_eq!(s3_host(&url("http://localhost:9000/")), "localhost:9000");
}
