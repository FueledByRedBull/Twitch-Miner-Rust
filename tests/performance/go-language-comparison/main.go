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

func benchmarkBalance(index int) int {
	return 123456 + (index & 7)
}

func operationChecksum(decision classes.PredictionDecision) int64 {
	return int64(decision.Amount) + int64(decision.Choice) + int64(len(decision.OutcomeID))
}

func updateSemanticChecksum(hash uint64, decision classes.PredictionDecision) uint64 {
	const prime uint64 = 1099511628211
	mix := func(value byte) {
		hash ^= uint64(value)
		hash *= prime
	}
	choice := uint64(decision.Choice)
	for shift := 0; shift < 64; shift += 8 {
		mix(byte(choice >> shift))
	}
	amount := uint64(int64(decision.Amount))
	for shift := 0; shift < 64; shift += 8 {
		mix(byte(amount >> shift))
	}
	for index := 0; index < len(decision.OutcomeID); index++ {
		mix(decision.OutcomeID[index])
	}
	return hash
}

func semanticSequenceChecksum(event *classes.PredictionEvent, iterations int) string {
	const offset uint64 = 14695981039346656037
	hash := uint64(offset)
	for index := 0; index < iterations; index++ {
		hash = updateSemanticChecksum(hash, event.Decide(benchmarkBalance(index)))
	}
	return fmt.Sprintf("%016x", hash)
}

func main() {
	iterations := boundedEnv("TM_LANGUAGE_BENCHMARK_ITERATIONS", 2000000, 100000, 100000000)
	runs := boundedEnv("TM_LANGUAGE_BENCHMARK_RUNS", 5, 3, 25)
	event := benchmarkEvent()
	if event == nil {
		panic("prediction fixture was rejected")
	}
	lastDecision := classes.PredictionDecision{}
	for index := 0; index < 10000; index++ {
		lastDecision = event.Decide(benchmarkBalance(index))
		runtime.KeepAlive(lastDecision)
	}

	throughput := make([]float64, 0, runs)
	var checksum int64
	for run := 0; run < runs; run++ {
		started := time.Now()
		for index := 0; index < iterations; index++ {
			lastDecision = event.Decide(benchmarkBalance(index))
			checksum += operationChecksum(lastDecision)
			runtime.KeepAlive(lastDecision)
		}
		throughput = append(throughput, float64(iterations)/time.Since(started).Seconds())
	}
	semanticChecksum := semanticSequenceChecksum(event, iterations)
	runtime.KeepAlive(event)
	report := map[string]interface{}{
		"schema":             3,
		"implementation":     "go",
		"revision":           revision,
		"workload":           "complete-production-prediction-decision",
		"iterations_per_run": iterations,
		"runs":               runs,
		"operations_per_second": map[string]float64{
			"median": percentile(throughput, 50),
			"p95":    percentile(throughput, 95),
		},
		"checksum":          checksum,
		"semantic_checksum": semanticChecksum,
		"decision_output": map[string]interface{}{
			"choice":     lastDecision.Choice,
			"outcome_id": lastDecision.OutcomeID,
			"amount":     lastDecision.Amount,
		},
		"measurement": map[string]interface{}{
			"build_profile":         "go-default-optimizer-stripped",
			"percentage_math":       "float64-truncating",
			"outcome_id":            "shallow-string-header",
			"output_consumption":    "complete-decision-runtime-keepalive",
			"semantic_verification": "all-decisions-separate-pass",
		},
	}
	encoded, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		panic(err)
	}
	fmt.Println(string(encoded))
}
