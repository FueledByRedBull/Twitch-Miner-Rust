use super::*;

#[test]
fn resolved_history_does_not_consume_unresolved_capacity_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let journal = PredictionPlacementJournal::open(directory.path())?;
    let now = unix_now_seconds();
    let request = PredictionPlacementRequest {
        account_id: "account",
        channel_id: "channel",
        event_id: "",
        choice: Some(0),
        outcome_id: "outcome",
        amount: 10,
        reserved_at_unix_seconds: now,
    };
    for index in 0..160 {
        let event = format!("resolved-{index}");
        assert!(journal.reserve(&PredictionPlacementRequest {
            event_id: &event,
            ..request
        })?);
        if index % 2 == 0 {
            journal.confirm("account", "channel", &event)?;
        } else {
            journal.reject("account", "channel", &event)?;
        }
    }
    let journal = PredictionPlacementJournal::open(directory.path())?;
    assert!(!journal.reserve(&PredictionPlacementRequest {
        event_id: "resolved-0",
        ..request
    })?);
    for index in 0..MAX_PENDING_PLACEMENTS {
        let event = format!("unresolved-{index}");
        assert!(journal.reserve(&PredictionPlacementRequest {
            event_id: &event,
            ..request
        })?);
        journal.mark_unknown("account", "channel", &event)?;
    }
    let journal = PredictionPlacementJournal::open(directory.path())?;
    assert!(journal
        .reserve(&PredictionPlacementRequest {
            event_id: "blocked",
            ..request
        })
        .is_err());
    journal.confirm("account", "channel", "unresolved-0")?;
    assert!(journal.reserve(&PredictionPlacementRequest {
        event_id: "new",
        ..request
    })?);
    let journal = PredictionPlacementJournal::open(directory.path())?;
    assert_eq!(
        journal
            .lookup("account", "channel", "unresolved-1")?
            .map(|r| r.status),
        Some(PredictionPlacementStatus::Unknown)
    );
    assert!(!journal.reserve(&PredictionPlacementRequest {
        event_id: "resolved-0",
        ..request
    })?);
    // Storage remains bounded, without discarding replay protection to make room.
    journal.confirm("account", "channel", "new")?;
    let oversized = "x".repeat(usize::try_from(MAX_JOURNAL_BYTES)?);
    assert!(journal
        .reserve(&PredictionPlacementRequest {
            event_id: &oversized,
            ..request
        })
        .is_err());
    assert!(journal.lookup("account", "channel", &oversized)?.is_none());
    assert!(
        PredictionPlacementJournal::open(directory.path())?.contains(
            "account",
            "channel",
            "resolved-0"
        )?
    );
    Ok(())
}

#[test]
fn normal_records_reach_byte_limit_without_losing_replay_protection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let journal = PredictionPlacementJournal::open(directory.path())?;
    let request = PredictionPlacementRequest {
        account_id: "123456789",
        channel_id: "987654321",
        event_id: "",
        choice: Some(1),
        outcome_id: "12345678-1234-1234-1234-123456789abc",
        amount: 50_000,
        reserved_at_unix_seconds: unix_now_seconds(),
    };
    let started = std::time::Instant::now();
    let mut retained = 0;
    for index in 0..2_000 {
        let event = format!("00000000-0000-0000-0000-{index:012}");
        match journal.reserve(&PredictionPlacementRequest {
            event_id: &event,
            ..request
        }) {
            Ok(true) => {
                journal.confirm(request.account_id, request.channel_id, &event)?;
                retained += 1;
            }
            Ok(false) => panic!("unique event must not be a replay"),
            Err(error) => {
                assert!(error.to_string().contains("byte capacity"));
                break;
            }
        }
    }
    let capacity = journal.capacity()?;
    assert!((500..1_000).contains(&retained));
    assert_eq!(capacity.retained_count, retained);
    assert_eq!(capacity.unresolved_count, 0);
    assert!(capacity.capacity_blocked);
    assert_eq!(
        capacity.bytes as u64,
        fs::metadata(directory.path().join(JOURNAL_FILE_NAME))?.len()
    );
    println!(
        "normal-record capacity: {retained} records, {} bytes, {:.3}s including durable writes",
        capacity.bytes,
        started.elapsed().as_secs_f64()
    );
    let reopened = PredictionPlacementJournal::open(directory.path())?;
    assert_eq!(reopened.capacity()?.retained_count, retained);
    assert!(!reopened.reserve(&PredictionPlacementRequest {
        event_id: "00000000-0000-0000-0000-000000000000",
        ..request
    })?);
    // Expiry frees only resolved history, and admission resumes.
    assert!(reopened.reserve(&PredictionPlacementRequest {
        event_id: "new-after-expiry",
        reserved_at_unix_seconds: request.reserved_at_unix_seconds
            + TERMINAL_TOMBSTONE_TTL_SECONDS
            + 60,
        ..request
    })?);
    assert!(!reopened.capacity()?.capacity_blocked);
    Ok(())
}

#[test]
fn admission_leaves_room_for_terminal_updates() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let journal = PredictionPlacementJournal::open(directory.path())?;
    let request = PredictionPlacementRequest {
        account_id: "account",
        channel_id: "channel",
        event_id: "event",
        choice: Some(0),
        outcome_id: "outcome",
        amount: 10,
        reserved_at_unix_seconds: unix_now_seconds(),
    };
    // A pending record alone fits, but its confirmation would exceed the limit.
    let mut probe = BTreeMap::new();
    probe.insert(
        journal_key("account", "channel", "event"),
        PendingPlacement {
            account_id: "account".into(),
            channel_id: "channel".into(),
            event_id: "event".into(),
            choice: Some(0),
            outcome_id: "outcome".into(),
            amount: 10,
            reserved_at_unix_seconds: request.reserved_at_unix_seconds,
            resolved_at_unix_seconds: None,
            status: PlacementStatus::Pending,
        },
    );
    let padding =
        "x".repeat(usize::try_from(MAX_JOURNAL_BYTES)? - snapshot_bytes(&probe)?.len() - 6);
    let outcome = format!("outcome{padding}");
    assert!(journal
        .reserve(&PredictionPlacementRequest {
            outcome_id: &outcome,
            ..request
        })
        .is_err());
    assert_eq!(journal.capacity()?.unresolved_count, 0);
    assert!(journal.capacity()?.capacity_blocked);
    assert!(!directory.path().join(JOURNAL_FILE_NAME).exists());

    // At the admitted boundary, even the largest serialized timestamp fits.
    let outcome = &outcome[..outcome.len() - 18];
    assert!(journal.reserve(&PredictionPlacementRequest {
        outcome_id: outcome,
        ..request
    })?);
    {
        let state = journal.lock()?;
        let mut largest = state.pending.clone();
        for entry in largest.values_mut() {
            entry.status = PlacementStatus::Confirmed;
            entry.resolved_at_unix_seconds = Some(i64::MIN);
        }
        assert_eq!(
            snapshot_bytes(&largest)?.len() + 1,
            admission_bytes(&state.pending)?
        );
        assert!(admission_bytes(&state.pending)? <= usize::try_from(MAX_JOURNAL_BYTES)?);
    }
    journal.mark_unknown("account", "channel", "event")?;
    let reopened = PredictionPlacementJournal::open(directory.path())?;
    reopened.confirm("account", "channel", "event")?;
    assert_eq!(
        PredictionPlacementJournal::open(directory.path())?
            .capacity()?
            .retained_count,
        1
    );
    Ok(())
}
