package twitchchannelpointsminer

import (
	"io"
	"log"
	"testing"

	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes/entities"
)

func TestParityPointsAndHistory(t *testing.T) {
	value := parityVectors(t)["points"].(map[string]interface{})
	m := &Miner{logger: &Logger{base: log.New(io.Discard, "", 0)}}
	streamer := &entities.Streamer{ChannelPoints: int(value["initial_balance"].(float64)), PointsInit: true}
	m.handlePubSubGain(streamer, int(value["earned"].(float64)), value["reason"].(string), int(value["balance"].(float64)))
	expected := value["expected"].(map[string]interface{})
	entry := streamer.History[value["reason"].(string)]
	if streamer.ChannelPoints != int(expected["balance"].(float64)) || entry == nil || entry.Count != int(expected["history_count"].(float64)) || entry.Amount != int(expected["history_amount"].(float64)) {
		t.Fatalf("points/history diverged: balance=%d history=%#v", streamer.ChannelPoints, entry)
	}
}
