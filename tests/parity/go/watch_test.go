package twitchchannelpointsminer

import (
	"encoding/json"
	"os"
	"testing"

	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes/entities"
)

func TestParityWatchSelection(t *testing.T) {
	value := parityVectors(t)["watch"].(map[string]interface{})
	priorityNames := []string{}
	for _, item := range value["priority_names"].([]interface{}) {
		priorityNames = append(priorityNames, item.(string))
	}
	gamePriority := []string{}
	for _, item := range value["game_priority"].([]interface{}) {
		gamePriority = append(gamePriority, item.(string))
	}
	gameExclude := []string{}
	for _, item := range value["game_exclude"].([]interface{}) {
		gameExclude = append(gameExclude, item.(string))
	}
	m := NewMiner("tester", "", false, false, LoggerSettings{}, entities.StreamerSettings{WatchStreak: false}, nil, priorityNames, nil, gamePriority, gameExclude, false, false, false)
	streamers := make([]*entities.Streamer, 0)
	for _, raw := range value["streamers"].([]interface{}) {
		item := raw.(map[string]interface{})
		stream := entities.NewStream()
		stream.Game = map[string]interface{}{"displayName": item["game"].(string)}
		streamers = append(streamers, &entities.Streamer{
			Username:      item["username"].(string),
			ChannelID:     item["channel_id"].(string),
			ChannelPoints: int(item["channel_points"].(float64)),
			Settings:      entities.StreamerSettings{WatchStreak: false},
			IsOnline:      item["online"].(bool),
			Stream:        stream,
		})
	}
	selected := m.pickStreamersToWatch(streamers)
	expected := value["expected"].([]interface{})
	if len(selected) != len(expected) {
		t.Fatalf("selected %d streamers, want %d", len(selected), len(expected))
	}
	for index, streamer := range selected {
		want := streamers[int(expected[index].(float64))]
		if streamer != want {
			t.Fatalf("selection %d was %q, want %q", index, streamer.Username, want.Username)
		}
	}
}

func parityVectors(t *testing.T) map[string]interface{} {
	t.Helper()
	data, err := os.ReadFile(os.Getenv("TM_PARITY_VECTOR"))
	if err != nil {
		t.Fatal(err)
	}
	var value map[string]interface{}
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatal(err)
	}
	return value
}
