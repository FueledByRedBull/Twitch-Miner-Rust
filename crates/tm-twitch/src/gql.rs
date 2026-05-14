use std::collections::BTreeMap;

use crate::types::{GqlPersistedOperation, GqlRequest};
use crate::{CLIENT_ID, GQL_URL};

#[must_use]
pub fn gql_headers(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".into(), format!("OAuth {auth_token}")),
        ("Client-Id".into(), CLIENT_ID.into()),
        ("Client-Session-Id".into(), client_session.into()),
        ("Client-Version".into(), client_version.into()),
        ("User-Agent".into(), user_agent.into()),
        ("X-Device-Id".into(), device_id.into()),
        ("Content-Type".into(), "application/json".into()),
    ])
}

pub fn gql_request(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
    operation: &GqlPersistedOperation,
) -> Result<GqlRequest, serde_json::Error> {
    Ok(GqlRequest {
        url: GQL_URL.to_string(),
        headers: gql_headers(
            auth_token,
            client_session,
            client_version,
            user_agent,
            device_id,
        ),
        body: serde_json::to_string(operation)?,
    })
}

pub fn gql_batch_request(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
    operations: &[GqlPersistedOperation],
) -> Result<GqlRequest, serde_json::Error> {
    Ok(GqlRequest {
        url: GQL_URL.to_string(),
        headers: gql_headers(
            auth_token,
            client_session,
            client_version,
            user_agent,
            device_id,
        ),
        body: serde_json::to_string(operations)?,
    })
}
