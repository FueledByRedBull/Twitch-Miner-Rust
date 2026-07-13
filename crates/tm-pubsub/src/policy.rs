#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictionSource {
    PubSubCompatibility,
    EventSubBroadcaster,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportSourcePolicy {
    pub prediction_source: PredictionSource,
    pub pubsub_presence: bool,
}

impl TransportSourcePolicy {
    #[must_use]
    pub const fn viewer_compatibility() -> Self {
        Self {
            prediction_source: PredictionSource::PubSubCompatibility,
            pubsub_presence: false,
        }
    }

    #[must_use]
    pub const fn broadcaster_eventsub() -> Self {
        Self {
            prediction_source: PredictionSource::EventSubBroadcaster,
            pubsub_presence: false,
        }
    }

    #[must_use]
    pub const fn legacy_pubsub() -> Self {
        Self {
            prediction_source: PredictionSource::PubSubCompatibility,
            pubsub_presence: true,
        }
    }
}

impl Default for TransportSourcePolicy {
    fn default() -> Self {
        Self::viewer_compatibility()
    }
}
