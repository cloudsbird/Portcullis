//! Detection against what a coding agent actually sends: logs, configs, package pins,
//! hashes, `.env` files. Two lists, both binding:
//!
//! * `NOISE` must come through untouched. Each line was redacted before, and a model
//!   that sees `<<PHONE_1>>` instead of a timestamp, or `<<IP_1>>` instead of
//!   `127.0.0.1`, cannot reason about logs or config. That is a real cost, not a
//!   cosmetic one.
//! * `MUST_HIDE` must be caught, with the expected label. Precision is never bought
//!   with these.

use portcullis::{Gateway, Store, Vault};

/// Redact `text` with no taught terms; return `(label, original text)` per span.
fn spans(text: &str) -> Vec<(String, String)> {
    let mut gw = Gateway::new(Store::default());
    let mut vault = Vault::new();
    let out = gw.process_with(&mut vault, &[text.to_string()]);
    // Walk the redacted text and pull each placeholder's value back out of the vault.
    let re = regex::Regex::new(r"<<([A-Z]+)_\d+>>").unwrap();
    re.find_iter(&out[0])
        .map(|m| {
            let label = re.captures(m.as_str()).unwrap()[1].to_string();
            (label, vault.restore(m.as_str()))
        })
        .collect()
}

const NOISE: &[&str] = &[
    "2026-11-14 03:44:49,019 INFO run_agent: OpenAI client closed",
    "2026-11-14T03:44:49Z started; released 2026-10-02.",
    "due 14-11-2026, built 20261101.360.1",
    "listening on 127.0.0.1:8080 and 0.0.0.0:9119, netmask 255.255.255.0",
    "installed react@18.3.1 and @types/node@20.11.5; pip install httpx==0.28.1",
    "git clone git@github.com:cloudsbird/Portcullis.git",
    "image docker.io/library/rust@sha256:4e1a7b3c9d0f",
    "chrome 131.0.6778.85, v1.2.3.4, schema 1.2.3.4.5",
    "pid 1234567, unix 1731556800, ms 1731556800000, order 4155550132",
    "id 550e8400-e29b-41d4-a716-446655440000",
    "commit 0f698483e157bcd6e86a4fdc1599220617e1a353",
    "  PID  %CPU %MEM    VSZ\n 1234  0.0  0.1 123456",
    "max_tokens: 4096, prompt_tokens=1234, tokenizer: cl100k_base",
    "password=os.environ['DB_PASSWORD']; token: null",
    "curl -H \"Authorization: Bearer $(cat ~/.token)\" https://api.example.com",
    "ask-me-anything-about-this-12345678901234567890",
    "std::cout << x << std::endl; cat <<'EOF'",
    "see 1.........9 and +1",
];

#[test]
fn agent_noise_is_left_alone() {
    let mut failures = Vec::new();
    for line in NOISE {
        let got = spans(line);
        if !got.is_empty() {
            failures.push(format!("{line:?} -> {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "over-redacted:\n{}",
        failures.join("\n")
    );
}

const MUST_HIDE: &[(&str, &str, &str)] = &[
    // (text, label, the exact value that must be hidden)
    ("mail jane.doe@acme.io today", "EMAIL", "jane.doe@acme.io"),
    ("cc daniel.pratt@northwind-logistics.com.", "EMAIL", "daniel.pratt@northwind-logistics.com"),
    ("call +1 (415) 555-0132", "PHONE", "+1 (415) 555-0132"),
    ("or +44 20 7946 0958 now", "PHONE", "+44 20 7946 0958"),
    ("office 415-555-0132", "PHONE", "415-555-0132"),
    ("dotted 415.555.0132", "PHONE", "415.555.0132"),
    ("e164 +14155550132", "PHONE", "+14155550132"),
    ("local 555-0132", "PHONE", "555-0132"),
    ("host 192.168.10.4 up", "IP", "192.168.10.4"),
    ("peer 10.0.1.3:443", "IP", "10.0.1.3"),
    ("card 4111 1111 1111 1111", "CARD", "4111 1111 1111 1111"),
    ("card 4111111111111111", "CARD", "4111111111111111"),
    ("card 5555-5555-5555-4444", "CARD", "5555-5555-5555-4444"),
    ("OPENAI_API_KEY=sk-proj-abc123DEF456ghi789JKL012mno345", "SECRET", "sk-proj-abc123DEF456ghi789JKL012mno345"),
    ("key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789", "SECRET", "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"),
    ("key sk-or-v1-0123456789abcdef0123456789abcdef", "SECRET", "sk-or-v1-0123456789abcdef0123456789abcdef"),
    ("aws AKIAIOSFODNN7EXAMPLE", "SECRET", "AKIAIOSFODNN7EXAMPLE"),
    ("gh ghp_1234567890abcdefghijABCDEFGHIJ123456", "SECRET", "ghp_1234567890abcdefghijABCDEFGHIJ123456"),
    ("slack xoxb-1234567890-abcdefghij", "SECRET", "xoxb-1234567890-abcdefghij"),
    ("google AIzaSyA1234567890abcdefghijklmnopqrstuv", "SECRET", "AIzaSyA1234567890abcdefghijklmnopqrstuv"),
    ("gitlab glpat-abcdefghij0123456789", "SECRET", "glpat-abcdefghij0123456789"),
    (
        "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "SECRET",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    ),
    ("db postgres://admin:hunter2@db.internal:5432/app", "SECRET", "hunter2"),
    ("Authorization: Bearer abcdef1234567890ABCDEF", "SECRET", "abcdef1234567890ABCDEF"),
    ("DB_PASSWORD=Sup3rS3cret!", "SECRET", "Sup3rS3cret!"),
    ("{\"api_key\": \"x9f8e7d6c5b4a3\"}", "SECRET", "x9f8e7d6c5b4a3"),
    ("client_secret: 9b1c2d3e4f5a6b7c", "SECRET", "9b1c2d3e4f5a6b7c"),
];

#[test]
fn real_pii_and_secrets_are_hidden() {
    let mut failures = Vec::new();
    for (text, label, value) in MUST_HIDE {
        let got = spans(text);
        let hit = got.iter().any(|(l, v)| l == label && v == value);
        if !hit {
            failures.push(format!("{text:?}: want {label} {value:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "missed:\n{}", failures.join("\n"));
}

#[test]
fn a_private_key_block_is_hidden_whole() {
    let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ\nAAAAAAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5\n-----END OPENSSH PRIVATE KEY-----";
    let got = spans(&format!("key:\n{pem}\ndone"));
    assert_eq!(got, vec![("SECRET".to_string(), pem.to_string())]);
}

/// The same key inside a JSON tool result (newlines escaped) is hidden too.
#[test]
fn a_private_key_in_a_json_tool_result_is_hidden() {
    let bs = '\\';
    let tool = format!(
        "{{\"output\": \"-----BEGIN RSA PRIVATE KEY-----{bs}nMIIEowIBAAKCAQEA7bq{bs}nQWERTY12{bs}n-----END RSA PRIVATE KEY-----\"}}"
    );
    let mut gw = Gateway::new(Store::default());
    let out = gw.process_with(&mut Vault::new(), &[tool]);
    assert_eq!(out[0], "{\"output\": \"<<SECRET_1>>\"}");
}

/// A URL's userinfo is a credential, not an email address.
#[test]
fn url_credentials_are_a_secret_not_an_email() {
    let got = spans("postgres://admin:hunter2@db.internal:5432/app");
    assert!(got.iter().all(|(l, _)| l != "EMAIL"), "{got:?}");
}

/// Secrets never go to the teach-suggestions list, which an admin endpoint returns.
#[test]
fn secrets_are_not_suggested_for_teaching() {
    let mut gw = Gateway::new(Store::default());
    gw.process_with(
        &mut Vault::new(),
        &["key sk-proj-abc123DEF456ghi789JKL012mno345 from jane@acme.io".to_string()],
    );
    let suggestions = gw.suggestions();
    assert!(
        suggestions.iter().all(|s| !s.starts_with("sk-")),
        "{suggestions:?}"
    );
    assert!(suggestions.iter().any(|s| s == "jane@acme.io"));
}
