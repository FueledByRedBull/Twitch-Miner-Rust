use super::{NetworkPolicy, ProxyConfig, ProxyParseError, ProxyScheme};

const PASSWORD: &str = "hunter2-secret";

fn parse(input: &str) -> Result<ProxyConfig, ProxyParseError> {
    ProxyConfig::parse(input)
}

#[test]
fn accepts_socks5h_and_http_with_a_host_and_port() {
    let socks = parse("socks5h://proxy.example:1080").unwrap();
    assert_eq!(socks.scheme(), ProxyScheme::Socks5h);
    assert_eq!(socks.host(), "proxy.example");
    assert_eq!(socks.port(), 1080);
    assert!(socks.credentials().is_none());

    let http = parse("http://10.0.0.5:3128/").unwrap();
    assert_eq!(http.scheme(), ProxyScheme::Http);
    assert_eq!(http.host(), "10.0.0.5");
    assert_eq!(http.port(), 3128);

    let default_port_written_out = parse("http://proxy.example:80").unwrap();
    assert_eq!(default_port_written_out.port(), 80);

    let upper_case = parse("  SOCKS5H://Proxy.Example:9050 ").unwrap();
    assert_eq!(upper_case.scheme(), ProxyScheme::Socks5h);
}

#[test]
fn accepts_ipv6_hosts_in_brackets() {
    let proxy = parse("socks5h://[::1]:9050").unwrap();
    assert_eq!(proxy.host(), "::1");
    assert_eq!(proxy.port(), 9050);
}

#[test]
fn reads_and_decodes_credentials() {
    let proxy = parse("socks5h://alice:p%40ss%3Aword@proxy.example:1080").unwrap();
    let credentials = proxy.credentials().unwrap();
    assert_eq!(credentials.username, "alice");
    assert_eq!(credentials.password, "p@ss:word");

    let user_only = parse("http://alice@proxy.example:3128").unwrap();
    let credentials = user_only.credentials().unwrap();
    assert_eq!(credentials.username, "alice");
    assert_eq!(credentials.password, "");
}

#[test]
fn socks5_is_rejected_because_it_resolves_names_locally() {
    let error = parse("socks5://proxy.example:1080").unwrap_err();
    assert_eq!(error, ProxyParseError::LocalDns);
    let message = error.to_string();
    assert!(message.contains("socks5h://"), "{message}");
    assert!(message.contains("leak"), "{message}");
}

#[test]
fn other_forms_are_rejected() {
    let cases = [
        ("https://proxy.example:443", ProxyParseError::Https),
        (
            "socks4://proxy.example:1080",
            ProxyParseError::UnsupportedScheme,
        ),
        (
            "socks4a://proxy.example:1080",
            ProxyParseError::UnsupportedScheme,
        ),
        ("ftp://proxy.example:21", ProxyParseError::UnsupportedScheme),
        ("proxy.example:1080", ProxyParseError::Malformed),
        ("socks5h://proxy.example", ProxyParseError::MissingPort),
        ("socks5h://[::1]", ProxyParseError::MissingPort),
        ("http://proxy.example:", ProxyParseError::Malformed),
        ("socks5h://:1080", ProxyParseError::MissingHost),
        ("socks5h://user:pw@:1080", ProxyParseError::MissingHost),
        ("socks5h://proxy.example:0", ProxyParseError::Malformed),
        ("socks5h://proxy.example:70000", ProxyParseError::Malformed),
        ("socks5h://proxy.example:port", ProxyParseError::Malformed),
        ("socks5h://pro xy.example:1080", ProxyParseError::Malformed),
        ("socks5h://[nonsense]:1080", ProxyParseError::Malformed),
        (
            "socks5h://:pw@proxy.example:1080",
            ProxyParseError::Malformed,
        ),
        (
            "socks5h://proxy.example:1080/path",
            ProxyParseError::ExtraComponents,
        ),
        (
            "socks5h://proxy.example:1080?x=1",
            ProxyParseError::ExtraComponents,
        ),
        (
            "socks5h://proxy.example:1080#frag",
            ProxyParseError::ExtraComponents,
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(parse(input).unwrap_err(), expected, "{input}");
    }
}

#[test]
fn neither_errors_nor_debug_output_contain_the_url_or_password() {
    let inputs = [
        format!("socks5://alice:{PASSWORD}@proxy.example:1080"),
        format!("https://alice:{PASSWORD}@proxy.example:443"),
        format!("socks5h://alice:{PASSWORD}@proxy.example"),
        format!("socks5h://alice:{PASSWORD}@proxy.example:1080/path"),
        format!("socks5h://alice:{PASSWORD}@pro xy:1080"),
        format!("gopher://alice:{PASSWORD}@proxy.example:70"),
        format!("socks5h://alice:{PASSWORD}@:1080"),
    ];
    for input in &inputs {
        let error = parse(input).unwrap_err();
        for output in [error.to_string(), format!("{error:?}")] {
            assert!(!output.contains(PASSWORD), "{output}");
            assert!(!output.contains("alice"), "{output}");
            assert!(!output.contains("proxy.example"), "{output}");
        }
    }

    let proxy = parse(&format!("socks5h://alice:{PASSWORD}@proxy.example:1080")).unwrap();
    let policy = NetworkPolicy::through(proxy.clone());
    for output in [
        format!("{proxy:?}"),
        format!("{proxy:#?}"),
        format!("{policy:?}"),
        format!("{policy:#?}"),
    ] {
        assert!(!output.contains(PASSWORD), "{output}");
        assert!(!output.contains("alice"), "{output}");
        assert!(!output.contains("proxy.example"), "{output}");
        assert!(!output.contains("1080"), "{output}");
    }
}

#[test]
fn an_empty_setting_means_no_proxy() {
    assert_eq!(
        NetworkPolicy::from_setting("").unwrap(),
        NetworkPolicy::direct()
    );
    assert_eq!(
        NetworkPolicy::from_setting("   ").unwrap(),
        NetworkPolicy::direct()
    );
    assert_eq!(NetworkPolicy::direct().proxy_scheme(), None);

    let policy = NetworkPolicy::from_setting("http://proxy.example:3128").unwrap();
    assert_eq!(policy.proxy_scheme(), Some(ProxyScheme::Http));
    assert!(NetworkPolicy::from_setting("socks5://proxy.example:1080").is_err());
}

#[test]
fn proxies_compare_by_endpoint_and_credentials() {
    let first = parse("socks5h://alice:one@proxy.example:1080").unwrap();
    assert_eq!(
        first,
        parse("socks5h://alice:one@proxy.example:1080").unwrap()
    );
    assert_ne!(
        first,
        parse("socks5h://alice:two@proxy.example:1080").unwrap()
    );
    assert_ne!(
        first,
        parse("socks5h://alice:one@proxy.example:1081").unwrap()
    );
}
