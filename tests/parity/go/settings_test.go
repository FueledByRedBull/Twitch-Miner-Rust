package main

import (
	"encoding/json"
	"os"
	"testing"
)

func parityVectors(t *testing.T) map[string]interface{} {
	t.Helper()
	path := os.Getenv("TM_PARITY_VECTOR")
	if path == "" {
		t.Fatal("TM_PARITY_VECTOR is required")
	}
	data, err := os.ReadFile(path)
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
	result := int(value.(float64))
	return &result
}

func parityFloat(value interface{}) *float64 {
	result := value.(float64)
	return &result
}

func TestParitySettings(t *testing.T) {
	value := parityVectors(t)["settings"].(map[string]interface{})
	bet := value["bet"].(map[string]interface{})
	cfg := config{
		BettingMakePredictions: value["betting_make_predictions"].(bool),
		FollowRaid:             value["follow_raid"].(bool),
		ClaimDrops:             value["claim_drops"].(bool),
		ClaimMoments:           value["claim_moments"].(bool),
		CommunityGoals:         value["community_goals"].(bool),
		IRCMode:                value["chat_presence"].(string),
		Bet: betConfig{
			Strategy:           bet["strategy"].(string),
			Percentage:         parityInt(bet["percentage"]),
			PercentageGap:      parityInt(bet["percentage_gap"]),
			MaxPoints:          parityInt(bet["max_points"]),
			MinimumPoints:      parityInt(bet["minimum_points"]),
			StealthMode:        parityBool(bet["stealth_mode"]),
			DeductStakeOnPlace: parityBool(bet["deduct_stake_on_place"]),
			DelayMode:          bet["delay_mode"].(string),
			Delay:              parityFloat(bet["delay"]),
		},
	}
	settings := buildBaseStreamerSettings(cfg)
	expected := value["expected"].(map[string]interface{})
	if settings.MakePredictions != expected["make_predictions"].(bool) || settings.FollowRaid != expected["follow_raid"].(bool) || settings.ClaimDrops != expected["claim_drops"].(bool) || settings.ClaimMoments != expected["claim_moments"].(bool) || settings.WatchStreak != expected["watch_streak"].(bool) || settings.CommunityGoals != expected["community_goals"].(bool) || string(settings.IRCMode) != expected["chat_presence"].(string) {
		t.Fatalf("streamer settings diverged: %#v", settings)
	}
	if string(settings.Bet.Strategy) != expected["strategy"].(string) || *settings.Bet.Percentage != int(expected["percentage"].(float64)) || *settings.Bet.PercentageGap != int(expected["percentage_gap"].(float64)) || *settings.Bet.MaxPoints != int(expected["max_points"].(float64)) || *settings.Bet.MinimumPoints != int(expected["minimum_points"].(float64)) || *settings.Bet.StealthMode != expected["stealth_mode"].(bool) || *settings.Bet.DeductStakeOnPlace != expected["deduct_stake_on_place"].(bool) || string(settings.Bet.DelayMode) != expected["delay_mode"].(string) || *settings.Bet.Delay != expected["delay"].(float64) {
		t.Fatalf("bet settings diverged: %#v", settings.Bet)
	}
}
