//! Tests of Import: the `BUNNY_*` lines of a `.env` file, the AWS CLI profiles, that neither
//! ever shows a secret, and the Add Site fields each fills.

use super::*;

#[test]
fn parse_env_reads_the_scripts_variables() {
    let text = "\u{feff}# deploy settings\r\n\
                export BUNNY_ZONE=\"exampleshop\"\r\n\
                BUNNY_KEY = 'pw-1234-abcd' # the zone password\r\n\
                BUNNY_REGION=sg   # Singapore\r\n\
                BUNNY_LOCAL_DIR=C:\\site\r\n\
                BUNNY_PREFIX=wp\r\n";
    let found = parse_env(text).unwrap();
    assert_eq!(
        found,
        EnvImport {
            zone: "exampleshop".into(),
            password: "pw-1234-abcd".into(),
            region: "sg".into(),
            local_dir: Some(PathBuf::from("C:\\site")),
            prefix: "wp".into(),
        }
    );

    // The last of two lines wins; a quote inside a quoted value survives; a # with no space
    // before it is part of the value.
    let found = parse_env("BUNNY_ZONE=a\nBUNNY_ZONE=b\nBUNNY_KEY=\"p\"w\"\n").unwrap();
    assert_eq!(
        (found.zone.as_str(), found.password.as_str()),
        ("b", "p\"w")
    );
    let found = parse_env("BUNNY_ZONE=z\nBUNNY_KEY=p#w # comment\n").unwrap();
    assert_eq!(found.password, "p#w");
    // No region line: none.
    assert_eq!(parse_env("BUNNY_ZONE=z\nBUNNY_KEY=k").unwrap().region, "");
    // A word that only starts with "export" is a name, not the keyword.
    let found = parse_env("exportBUNNY=1\nBUNNY_ZONE=z\nBUNNY_KEY=k\n").unwrap();
    assert_eq!(found.zone, "z");

    // What is missing is said, the key first; never a value from the file.
    assert_eq!(
        parse_env("BUNNY_ZONE=z\n"),
        Err("The file has no BUNNY_KEY line.".to_owned())
    );
    assert_eq!(
        parse_env(""),
        Err("The file has no BUNNY_KEY line.".to_owned())
    );
    assert_eq!(
        parse_env("BUNNY_KEY=secret-value\n"),
        Err("The file has no BUNNY_ZONE line.".to_owned())
    );
    assert_eq!(
        parse_env("BUNNY_ZONE=z\nBUNNY_KEY=\"\"\n"),
        Err("The BUNNY_KEY line is empty.".to_owned())
    );
    assert_eq!(
        parse_env("BUNNY_ZONE=\nBUNNY_KEY=k\n"),
        Err("The BUNNY_ZONE line is empty.".to_owned())
    );
    // Lines that are not assignments are skipped.
    let found = parse_env("not a line\n=x\nBAD NAME=1\nBUNNY_ZONE=z\nBUNNY_KEY=k\n").unwrap();
    assert_eq!(found.zone, "z");
}

#[test]
fn parse_aws_profiles_merges_config_regions() {
    let credentials = "\u{feff}[default]\r\n\
                       aws_access_key_id = AKIDDEFAULT\r\n\
                       aws_secret_access_key = secret-default\r\n\
                       \r\n\
                       ; a comment\r\n\
                       [work]\r\n\
                       AWS_ACCESS_KEY_ID=AKIDWORK\r\n\
                       aws_secret_access_key=secret-work\r\n\
                       aws_session_token=token\r\n\
                       \r\n\
                       [half]\r\n\
                       aws_access_key_id=AKIDHALF\r\n\
                       \r\n\
                       [no-region]\r\n\
                       aws_access_key_id=AKIDNR\r\n\
                       aws_secret_access_key=secret-nr\r\n";
    let config = "[default]\n\
                  region = us-east-1\n\
                  [profile work]\n\
                  region=eu-west-1\n\
                  output=json\n\
                  [work]\n\
                  region=ap-south-1\n\
                  [profile sso-only]\n\
                  region=eu-central-1\n\
                  sso_start_url=https://example.awsapps.com/start\n";
    let profiles = parse_aws_profiles(credentials, config);
    let shown: Vec<_> = profiles
        .iter()
        .map(|p| {
            (
                p.name.as_str(),
                p.key_id.as_str(),
                p.secret.as_str(),
                p.region.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                "default",
                "AKIDDEFAULT",
                "secret-default",
                Some("us-east-1")
            ),
            // `[profile work]` in config is `[work]` in credentials; config's bare `[work]` is
            // not the AWS CLI's way and is ignored.
            ("work", "AKIDWORK", "secret-work", Some("eu-west-1")),
            ("no-region", "AKIDNR", "secret-nr", None),
        ]
    );
    // Missing files read as empty.
    assert!(parse_aws_profiles("", config).is_empty());
    assert_eq!(parse_aws_profiles(credentials, "").len(), 3);
    assert_eq!(parse_aws_profiles(credentials, "")[0].region, None);
}

#[test]
fn import_debug_never_shows_secrets() {
    let env = EnvImport {
        zone: "exampleshop".into(),
        password: "pw-hunter2".into(),
        region: "sg".into(),
        local_dir: None,
        prefix: String::new(),
    };
    let profile = AwsProfile {
        name: "work".into(),
        key_id: "AKIDWORK".into(),
        secret: "sk-hunter2".into(),
        region: Some("eu-west-1".into()),
    };
    for text in [
        format!("{env:?}"),
        format!("{env:#?}"),
        format!("{profile:?}"),
        format!("{profile:#?}"),
    ] {
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<hidden>"), "{text}");
    }
    assert!(format!("{env:?}").contains("exampleshop"));
    assert!(format!("{profile:?}").contains("AKIDWORK"));
    // The errors never quote the file either.
    let error = parse_env("BUNNY_KEY=hunter2\n").unwrap_err();
    assert!(!error.contains("hunter2"), "{error}");
}

#[test]
fn import_prefills_the_add_site_fields_without_the_secret() {
    let bunny = env_draft(&EnvImport {
        zone: "exampleshop".into(),
        password: "pw".into(),
        region: "Singapore".into(),
        local_dir: None,
        prefix: String::new(),
    });
    assert_eq!(
        (bunny.provider, bunny.zone.as_str(), bunny.region_index),
        (0, "exampleshop", 4)
    );
    assert_eq!(bunny.key_id, "");
    let unknown = env_draft(&EnvImport {
        zone: "z".into(),
        password: "pw".into(),
        region: String::new(),
        local_dir: None,
        prefix: String::new(),
    });
    assert_eq!(unknown.region_index, -1);

    let aws = aws_draft(&AwsProfile {
        name: "work".into(),
        key_id: "AKIDWORK".into(),
        secret: "sk".into(),
        region: Some("eu-west-1".into()),
    });
    assert_eq!(
        (aws.provider, aws.region.as_str(), aws.key_id.as_str()),
        (1, "eu-west-1", "AKIDWORK")
    );
    assert_eq!((aws.bucket.as_str(), aws.region_index), ("", -1));
    let no_region = aws_draft(&AwsProfile {
        name: "x".into(),
        key_id: "K".into(),
        secret: "s".into(),
        region: None,
    });
    assert_eq!(no_region.region, "");
}

#[test]
fn a_picked_env_file_is_read_and_a_missing_one_is_said_in_words() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deploy.env");
    std::fs::write(&path, "BUNNY_ZONE=z\nBUNNY_KEY=k\nBUNNY_REGION=de\n").unwrap();
    assert_eq!(read_env_file(&path).unwrap().region, "de");
    let gone = read_env_file(&dir.path().join("gone.env")).unwrap_err();
    assert!(gone.starts_with("Couldn't read the file: "), "{gone}");
}
