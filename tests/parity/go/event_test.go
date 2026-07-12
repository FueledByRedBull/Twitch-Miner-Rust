package classes

import (
	"encoding/json"
	"testing"

	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes/entities"
	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/constants"
)

type parityEventLogger struct{}

func (parityEventLogger) Printf(string, ...interface{}) {}
func (parityEventLogger) Errorf(string, ...interface{}) {}
func (parityEventLogger) EmojiPrintf(string, string, ...interface{}) {}
func (parityEventLogger) Eventf(constants.Event, string, ...interface{}) {}
func (parityEventLogger) EmojiEventf(string, constants.Event, string, ...interface{}) {}
func (parityEventLogger) ErrorEventf(constants.Event, string, ...interface{}) {}
func (parityEventLogger) Debugf(string, ...interface{}) {}
func (parityEventLogger) DebugEnabled() bool { return false }

func TestParityPubSubPointsEvent(t *testing.T) {
	value := parityVectors(t)["pubsub"].(map[string]interface{})
	expected := value["expected"].(map[string]interface{})
	streamer := &entities.Streamer{ChannelID: expected["channel_id"].(string)}
	var earned, balance int
	var reason string
	client := &PubSubClient{
		logger:      parityEventLogger{},
		streamerMap: map[string]*entities.Streamer{streamer.ChannelID: streamer},
		onGain: func(_ *entities.Streamer, gotEarned int, gotReason string, gotBalance int) {
			earned, reason, balance = gotEarned, gotReason, gotBalance
		},
	}
	raw, err := json.Marshal(value["raw"])
	if err != nil {
		t.Fatal(err)
	}
	if err := client.handleMessage(raw, nil); err != nil {
		t.Fatal(err)
	}
	if earned != int(expected["earned"].(float64)) || reason != expected["reason"].(string) || balance != int(expected["balance"].(float64)) {
		t.Fatalf("event diverged: earned=%d reason=%s balance=%d", earned, reason, balance)
	}
}
