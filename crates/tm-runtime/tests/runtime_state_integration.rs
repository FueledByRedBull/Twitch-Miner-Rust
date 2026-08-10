use tm_config::ConfigFile;
use tm_domain::{MinerEvent, OffsetDateTime};
use tm_runtime::{RuntimeEffect, RuntimeError, RuntimeState};

fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

#[tokio::test]
async fn runtime_handle_serializes_event_notification_and_shutdown() {
    let config = ConfigFile {
        streamers: vec!["alpha".into()],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = "100".into();
    let runtime = tm_runtime::spawn_runtime_state(state);
    let mut changes = runtime.subscribe_state_changes();

    let effects = runtime
        .apply_event(
            MinerEvent::ClaimAvailable {
                channel_id: "100".into(),
                claim_id: "claim-1".into(),
            },
            ts(10),
        )
        .await
        .unwrap();
    assert_eq!(
        effects,
        vec![RuntimeEffect::ClaimBonus {
            channel_id: "100".into(),
            claim_id: "claim-1".into(),
        }]
    );
    changes.changed().await.unwrap();
    assert_eq!(*changes.borrow(), 1);

    let snapshot = runtime.state_snapshot().await.unwrap();
    assert_eq!(snapshot.streamers[0].channel_id, "100");

    let summary = runtime.shutdown(false, ts(60)).await.unwrap();
    assert_eq!(summary.duration, "00:01:00.000000");
    assert!(matches!(
        runtime.state_snapshot().await,
        Err(RuntimeError::RuntimeClosed {
            command: "StateSnapshot"
        })
    ));
}
