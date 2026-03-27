use std::time::Duration;

use tm_integration_tests::{base_runtime_state, ts};
use tm_pubsub::{PlaybackType, PubSubEvent};

#[tokio::test(start_paused = true)]
async fn paused_runtime_can_drive_time_based_integration_sequences() {
    let runtime = tm_runtime::spawn_runtime_state(base_runtime_state());
    let delayed_runtime = runtime.clone();

    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        delayed_runtime
            .apply_pubsub_event(
                PubSubEvent::Playback {
                    channel_id: String::from("123"),
                    kind: PlaybackType::StreamUp,
                },
                ts(30),
            )
            .await
            .unwrap();
    });

    tokio::time::advance(Duration::from_secs(30)).await;
    task.await.unwrap();

    let snapshot = runtime.state_snapshot().await.unwrap();
    assert_eq!(snapshot.desired_chat_logins(), vec!["alpha"]);
    assert!(snapshot.watch_target_logins(ts(59)).is_empty());
    assert_eq!(snapshot.watch_target_logins(ts(61)), vec!["alpha"]);
}
