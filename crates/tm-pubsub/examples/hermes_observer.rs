//! Replay the contained Hermes observation adapter against a JSONL capture.
//!
//! Usage:
//! `cargo run -p tm-pubsub --example hermes_observer -- capture.jsonl subscription-id=topic`
//!
//! The adapter is deliberately offline and observation-only. It does not open
//! a socket, authenticate, subscribe, reconnect, or execute runtime effects.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, BufRead, BufReader};

use tm_domain::MinerEvent;
use tm_pubsub::{
    HermesObservation, HermesObserver, HERMES_MAX_FRAME_BYTES, HERMES_MAX_REPLAY_FRAMES,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("Hermes observer failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let capture_path = arguments
        .next()
        .ok_or("usage: hermes_observer <capture.jsonl> [subscription-id=topic ...]")?;
    let mappings = arguments
        .map(|mapping| {
            let (subscription_id, topic) = mapping
                .split_once('=')
                .ok_or("subscription mapping must use subscription-id=topic")?;
            if subscription_id.trim().is_empty() || topic.trim().is_empty() {
                return Err("subscription mapping cannot be empty");
            }
            Ok((subscription_id.to_string(), topic.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut observer = HermesObserver::new(mappings)?;
    let file = File::open(capture_path)?;
    let mut reader = BufReader::new(file);
    let mut frame = Vec::new();
    let mut frames = 0_usize;
    let mut welcomes = 0_usize;
    let mut ignored = 0_usize;
    let mut event_counts = BTreeMap::<&'static str, usize>::new();

    while read_bounded_line(&mut reader, &mut frame)? {
        trim_line_end(&mut frame);
        if frame.is_empty() {
            continue;
        }
        frames = frames
            .checked_add(1)
            .ok_or("Hermes replay frame count overflow")?;
        if frames > HERMES_MAX_REPLAY_FRAMES {
            return Err("Hermes replay contains too many frames".into());
        }
        let raw = std::str::from_utf8(&frame)?;
        match observer.observe_frame(raw, &[])? {
            HermesObservation::Welcome => welcomes += 1,
            HermesObservation::Event(event) => {
                *event_counts.entry(event_kind(&event)).or_default() += 1;
            }
            HermesObservation::IgnoredUnknownSubscription | HermesObservation::IgnoredNoEvent => {
                ignored += 1;
            }
        }
    }

    println!("frames={frames}");
    println!("welcomes={welcomes}");
    println!("ignored={ignored}");
    for (kind, count) in event_counts {
        println!("event.{kind}={count}");
    }
    Ok(())
}

fn read_bounded_line<R: BufRead>(reader: &mut R, output: &mut Vec<u8>) -> io::Result<bool> {
    output.clear();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(!output.is_empty());
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |index| index + 1);
        if output.len().saturating_add(take) > HERMES_MAX_FRAME_BYTES + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Hermes replay frame exceeds the configured bound",
            ));
        }
        output.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn trim_line_end(frame: &mut Vec<u8>) {
    if frame.last() == Some(&b'\n') {
        frame.pop();
    }
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
}

fn event_kind(event: &MinerEvent) -> &'static str {
    match event {
        MinerEvent::PointsEarned { .. } => "points-earned",
        MinerEvent::ClaimAvailable { .. } => "claim-available",
        MinerEvent::Playback { .. } => "playback",
        MinerEvent::Raid { .. } => "raid",
        MinerEvent::Moment { .. } => "moment",
        MinerEvent::PredictionChannel { .. } => "prediction-channel",
        MinerEvent::PredictionUser { .. } => "prediction-user",
        MinerEvent::CommunityGoal { .. } => "community-goal",
    }
}
