package main

import (
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"time"

	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes"
	"TwitchChannelPointsMiner/TwitchChannelPointsMiner/classes/entities"
)

var revision = "development"
var checksum int

func integerSetting(value int) *int {
	return &value
}

func boolSetting(value bool) *bool {
	return &value
}

func benchmarkEvent() *classes.PredictionEvent {
	streamer := &entities.Streamer{
		ChannelPoints: 123456,
		Settings: entities.StreamerSettings{
			Bet: entities.BetSettings{
				Strategy:    entities.StrategyMostVoted,
				Percentage:  integerSetting(7),
				StealthMode: boolSetting(false),
			},
		},
	}
	return classes.NewPredictionEvent(streamer, map[string]interface{}{
		"id":                        "language-comparison",
		"title":                     "Sanitized prediction",
		"status":                    "ACTIVE",
		"created_at":                "1970-01-01T00:00:00Z",
		"prediction_window_seconds": 120,
		"outcomes": []interface{}{
			map[string]interface{}{
				"id":           "alpha",
				"title":        "Alpha",
				"total_users":  641,
				"total_points": 7000000,
				"top_predictors": []interface{}{
					map[string]interface{}{"points": 25000},
				},
			},
			map[string]interface{}{
				"id":           "bravo",
				"title":        "Bravo",
				"total_users":  455,
				"total_points": 4000000,
				"top_predictors": []interface{}{
					map[string]interface{}{"points": 18000},
				},
			},
		},
	})
}

func boundedEnv(name string, fallback, minimum, maximum int) int {
	value, err := strconv.Atoi(os.Getenv(name))
	if err != nil {
		return fallback
	}
	if value < minimum {
		return minimum
	}
	if value > maximum {
		return maximum
	}
	return value
}

func percentile(samples []float64, percent int) float64 {
	ordered := append([]float64(nil), samples...)
	for index := 1; index < len(ordered); index++ {
		for current := index; current > 0 && ordered[current] < ordered[current-1]; current-- {
			ordered[current], ordered[current-1] = ordered[current-1], ordered[current]
		}
	}
	index := ((len(ordered)*percent + 99) / 100) - 1
	if index < 0 {
		return 0
	}
	return ordered[index]
}

func main() {
	iterations := boundedEnv("TM_LANGUAGE_BENCHMARK_ITERATIONS", 2000000, 10000, 100000000)
	runs := boundedEnv("TM_LANGUAGE_BENCHMARK_RUNS", 5, 3, 25)
	event := benchmarkEvent()
	if event == nil {
		panic("prediction fixture was rejected")
	}
	for index := 0; index < 10000; index++ {
		checksum += event.Decide(123456).Amount
	}

	throughput := make([]float64, 0, runs)
	checksum = 0
	for run := 0; run < runs; run++ {
		started := time.Now()
		for index := 0; index < iterations; index++ {
			checksum += event.Decide(123456).Amount
		}
		throughput = append(throughput, float64(iterations)/time.Since(started).Seconds())
	}
	runtime.KeepAlive(event)
	report := map[string]interface{}{
		"schema":             1,
		"implementation":     "go",
		"revision":           revision,
		"workload":           "production-prediction-decision",
		"iterations_per_run": iterations,
		"runs":               runs,
		"operations_per_second": map[string]float64{
			"median": percentile(throughput, 50),
			"p95":    percentile(throughput, 95),
		},
		"checksum": checksum,
	}
	encoded, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		panic(err)
	}
	fmt.Println(string(encoded))
}
