package classes

import (
	"encoding/json"
	"os"
	"testing"

	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes/entities"
)

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

func parityBool(value interface{}) *bool {
	result := value.(bool)
	return &result
}

func parityInt(value interface{}) *int {
	var result int
	switch value := value.(type) {
	case float64:
		result = int(value)
	case int:
		result = value
	default:
		panic("parity integer must be numeric")
	}
	return &result
}

func TestParityPrediction(t *testing.T) {
	value := parityVectors(t)["prediction"].(map[string]interface{})
	settings := value["settings"].(map[string]interface{})
	streamer := &entities.Streamer{
		ChannelPoints: int(value["balance"].(float64)),
		Settings: entities.StreamerSettings{Bet: entities.BetSettings{
			Strategy:    entities.Strategy(settings["strategy"].(string)),
			Percentage:  parityInt(settings["percentage"]),
			MaxPoints:   parityInt(settings["max_points"]),
			StealthMode: parityBool(settings["stealth_mode"]),
		}},
	}
	event := NewPredictionEvent(streamer, map[string]interface{}{
		"id":       "parity-event",
		"title":    "Parity",
		"status":   "ACTIVE",
		"outcomes": value["outcomes"],
	})
	if event == nil {
		t.Fatal("expected prediction event")
	}
	decision := event.Decide(streamer.ChannelPoints)
	result := value["result"].(map[string]interface{})
	gained, placed, won, resultType, resultString := event.ParseResult(result)
	expected := value["expected"].(map[string]interface{})
	if decision.Choice != int(expected["choice"].(float64)) || decision.OutcomeID != expected["outcome_id"].(string) || decision.Amount != int(expected["amount"].(float64)) {
		t.Fatalf("decision diverged: %#v", decision)
	}
	if gained != int(expected["gained"].(float64)) || placed != int(expected["placed"].(float64)) || won != int(expected["won"].(float64)) || resultType != expected["result_type"].(string) || resultString != expected["result_string"].(string) {
		t.Fatalf("settlement diverged: %d %d %d %s %s", gained, placed, won, resultType, resultString)
	}
}
