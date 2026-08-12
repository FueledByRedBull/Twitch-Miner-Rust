use std::time::Duration;

use serde_json::json;
use tm_twitch::{CLIENT_ID, GQL_URL, PERSISTED_OPERATION_CONTRACTS};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("apq probe: {error}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), String> {
    let filter = parse_operation_filter()?;
    let contracts: Vec<_> = PERSISTED_OPERATION_CONTRACTS
        .iter()
        .filter(|contract| {
            filter
                .as_deref()
                .is_none_or(|name| name == contract.operation_name)
        })
        .collect();

    if contracts.is_empty() {
        return Err(filter.map_or_else(
            || "no persisted operations are inventoried".to_string(),
            |name| format!("unknown persisted operation: {name}"),
        ));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "could not build HTTP client".to_string())?;
    let mut counts = [0_usize; 3];

    for contract in contracts {
        let payload = probe_payload(contract);
        let status = match client
            .post(GQL_URL)
            .header("Client-Id", CLIENT_ID)
            .json(&payload)
            .send()
            .await
        {
            Ok(response) if response.status().as_u16() == 200 => {
                let body = response.bytes().await.map_err(|_| ()).unwrap_or_default();
                classify_response(Some(200), &body)
            }
            Ok(response) => classify_response(Some(response.status().as_u16()), &[]),
            Err(_) => classify_response(None, &[]),
        };
        counts[status_index(status)] += 1;
        let mode = if contract.read_only {
            "READ"
        } else {
            "MUTATION-HASH"
        };
        println!("{} [{mode}]: {status}", contract.operation_name);
    }

    println!(
        "summary: REGISTERED={} BROKEN={} INCONCLUSIVE={}",
        counts[0], counts[1], counts[2]
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeStatus {
    Registered,
    Broken,
    Inconclusive,
}

impl std::fmt::Display for ProbeStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Registered => "REGISTERED",
            Self::Broken => "BROKEN",
            Self::Inconclusive => "INCONCLUSIVE",
        })
    }
}

fn classify_response(status: Option<u16>, body: &[u8]) -> ProbeStatus {
    if status != Some(200) {
        return ProbeStatus::Inconclusive;
    }

    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ProbeStatus::Inconclusive;
    };
    let Some(object) = payload.as_object() else {
        return ProbeStatus::Inconclusive;
    };
    let Some(errors_value) = object.get("errors") else {
        return if object.contains_key("data") {
            ProbeStatus::Registered
        } else {
            ProbeStatus::Inconclusive
        };
    };
    let Some(errors) = errors_value.as_array() else {
        return ProbeStatus::Inconclusive;
    };

    let mut has_error_detail = false;
    for error in errors {
        let values = [
            error.get("message").and_then(serde_json::Value::as_str),
            error.get("code").and_then(serde_json::Value::as_str),
            error
                .get("extensions")
                .and_then(|extensions| extensions.get("code"))
                .and_then(serde_json::Value::as_str),
        ];
        for value in values.into_iter().flatten().map(str::to_ascii_lowercase) {
            has_error_detail = true;
            if value.contains("persistedquerynotfound")
                || value.contains("persisted_query_not_found")
            {
                return ProbeStatus::Broken;
            }
            if value.contains("persistedquerynotsupported")
                || value.contains("persisted_query_not_supported")
            {
                return ProbeStatus::Inconclusive;
            }
        }
    }
    if object.contains_key("data") || has_error_detail {
        ProbeStatus::Registered
    } else {
        ProbeStatus::Inconclusive
    }
}

fn probe_payload(contract: &tm_twitch::PersistedOperationContract) -> serde_json::Value {
    json!({
        "operationName": contract.operation_name,
        "extensions": {
            "persistedQuery": {
                "version": 1,
                "sha256Hash": contract.sha256_hash,
            }
        }
    })
}

fn parse_operation_filter() -> Result<Option<String>, String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(None),
        [flag] if flag == "--help" || flag == "-h" => {
            println!("Usage: cargo run -p tm-twitch --example apq_probe -- [--operation NAME]");
            std::process::exit(0);
        }
        [flag, name] if flag == "--operation" && !name.is_empty() => Ok(Some(name.clone())),
        _ => Err("usage: apq_probe [--operation NAME]".to_string()),
    }
}

const fn status_index(status: ProbeStatus) -> usize {
    match status {
        ProbeStatus::Registered => 0,
        ProbeStatus::Broken => 1,
        ProbeStatus::Inconclusive => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_response, probe_payload, ProbeStatus, PERSISTED_OPERATION_CONTRACTS};

    #[test]
    fn payload_is_hash_only_for_every_contract() {
        for contract in PERSISTED_OPERATION_CONTRACTS {
            let payload = probe_payload(contract);
            assert!(payload.get("query").is_none());
            assert!(payload.get("variables").is_none());
            assert_eq!(payload.as_object().map(|object| object.len()), Some(2));
        }
    }

    #[test]
    fn classifier_is_conservative_and_body_free() {
        assert_eq!(
            classify_response(Some(200), br#"{"data":{"ok":true}}"#),
            ProbeStatus::Registered
        );
        assert_eq!(
            classify_response(
                Some(200),
                br#"{"errors":[{"message":"PersistedQueryNotFound"}]}"#
            ),
            ProbeStatus::Broken
        );
        assert_eq!(
            classify_response(
                Some(200),
                br#"{"errors":[{"extensions":{"code":"PERSISTED_QUERY_NOT_SUPPORTED"}}]}"#
            ),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(200), br"not-json"),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(200), br#"{"errors":[{}]}"#),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(200), br#"{}"#),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(200), br#"[]"#),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(200), br#"{"errors":{}}"#),
            ProbeStatus::Inconclusive
        );
        assert_eq!(
            classify_response(Some(503), br"{}"),
            ProbeStatus::Inconclusive
        );
        assert_eq!(classify_response(None, &[]), ProbeStatus::Inconclusive);
    }
}
