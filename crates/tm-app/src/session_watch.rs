use std::time::Duration;

use tm_auth::TwitchAuthClient;
use tm_observability::Event as DiscordEvent;

use crate::bootstrap::{saved_session_requires_reauthorization, DEFAULT_USER_AGENT};
use crate::observability::AppObservability;

pub(crate) const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Finishes when Twitch definitively rejects the session. Supervision treats
/// that as a task failure, so the process exits and its restart reauthorizes;
/// without this a revoked token leaves every worker failing forever.
pub(crate) fn spawn_session_watch_loop(
    mut stop: tokio::sync::watch::Receiver<bool>,
    auth_client: TwitchAuthClient,
    auth_token: String,
    device_id: String,
    username: String,
    observability: AppObservability,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                stop_result = stop.changed() => {
                    if stop_result.is_err() || *stop.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(interval) => {
                    let result = auth_client
                        .validate_login_details(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
                        .await;
                    if result.as_ref().err().is_some_and(saved_session_requires_reauthorization) {
                        tracing::error!(
                            operation = "auth",
                            error_class = "session-rejected",
                            "Twitch rejected the saved session; exiting so a restart can reauthorize"
                        );
                        observability.spawn_event(
                            DiscordEvent::SessionLost,
                            String::from("Twitch rejected the saved session; a restart must reauthorize"),
                        );
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tm_auth::AuthEndpoints;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_status(status: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}/validate"), server)
    }

    fn watch(
        validate_url: String,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let auth_client = TwitchAuthClient::with_client_and_endpoints(
            reqwest::Client::new(),
            AuthEndpoints {
                device_code_url: String::new(),
                token_url: String::new(),
                validate_url,
            },
        );
        spawn_session_watch_loop(
            stop,
            auth_client,
            String::from("token"),
            String::from("device"),
            String::from("tester"),
            AppObservability::new(
                None,
                tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
                crate::observability::AppObservabilitySettings::default(),
            ),
            Duration::from_millis(1),
        )
    }

    #[tokio::test]
    async fn rejected_session_finishes_the_task() {
        let (url, server) = serve_status("401 Unauthorized").await;
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        tokio::time::timeout(Duration::from_secs(5), watch(url, stop_rx))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transient_failure_keeps_watching_until_stopped() {
        let (url, server) = serve_status("503 Service Unavailable").await;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = watch(url, stop_rx);
        server.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
