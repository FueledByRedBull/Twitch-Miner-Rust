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
