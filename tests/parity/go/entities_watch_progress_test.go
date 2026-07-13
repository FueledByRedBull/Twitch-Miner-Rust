package entities

import (
	"testing"
	"time"
)

func TestParityWatchProgress(t *testing.T) {
	stream := NewStream()
	stream.lastMinuteUpdate = time.Now().Add(-90 * time.Second)
	stream.UpdateMinuteWatched()
	if stream.MinuteWatched < 1.4 || stream.MinuteWatched > 1.6 {
		t.Fatalf("continuous watch progress out of range: %f", stream.MinuteWatched)
	}

	stream.MinuteWatched = 1
	stream.lastMinuteUpdate = time.Now().Add(-121 * time.Second)
	stream.UpdateMinuteWatched()
	if stream.MinuteWatched != 0 {
		t.Fatalf("watch progress should reset after a gap, got %f", stream.MinuteWatched)
	}
}
