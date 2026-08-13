package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/zeromicro/go-zero/core/breaker"
	"github.com/zeromicro/go-zero/core/logx"
	"github.com/zeromicro/go-zero/core/service"
	"github.com/zeromicro/go-zero/rest"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/protobuf/types/known/wrapperspb"
)

type config struct {
	Version               int `json:"version"`
	WarmupIterations      int `json:"warmup_iterations"`
	MeasuredIterations    int `json:"measured_iterations"`
	Concurrency           int `json:"concurrency"`
	PayloadBytes          int `json:"payload_bytes"`
	BreakerFailurePercent int `json:"breaker_failure_percent"`
	DiscoveryEndpoints    int `json:"discovery_endpoints"`
	QueueCapacity         int `json:"queue_capacity"`
	QueueMessages         int `json:"queue_messages"`
	QueueConsumerDelayUS  int `json:"queue_consumer_delay_us"`
	OverloadConcurrency   int `json:"overload_concurrency"`
}

type measurement struct {
	Name                string            `json:"name"`
	Operations          int               `json:"operations"`
	ElapsedMS           float64           `json:"elapsed_ms"`
	OperationsPerSecond float64           `json:"operations_per_second"`
	P50US               float64           `json:"p50_us"`
	P95US               float64           `json:"p95_us"`
	P99US               float64           `json:"p99_us"`
	Allocations         uint64            `json:"allocations"`
	AllocatedBytes      uint64            `json:"allocated_bytes"`
	PeakRSSKiB          uint64            `json:"peak_rss_kib"`
	Counters            map[string]uint64 `json:"counters"`
}

type report struct {
	SchemaVersion    int           `json:"schema_version"`
	Framework        string        `json:"framework"`
	FrameworkVersion string        `json:"framework_version"`
	UnixTimestamp    int64         `json:"unix_timestamp"`
	GitRevision      string        `json:"git_revision"`
	GoVersion        string        `json:"go_version"`
	Target           string        `json:"target"`
	Config           config        `json:"config"`
	Workloads        []measurement `json:"workloads"`
}

func main() {
	path := "benchmarks/config/v1.toml"
	if len(os.Args) > 1 {
		path = os.Args[1]
	}
	cfg, err := readConfig(path)
	if err != nil {
		panic(err)
	}
	workloads := []measurement{restTransport(cfg), grpcTransport(cfg), breakerFailure(cfg), overloadRecovery(cfg), discoverySnapshot(cfg), queueSaturation(cfg)}
	r := report{1, "go-zero", "v1.10.3", time.Now().Unix(), env("RUST_ZERO_GIT_REVISION"), runtime.Version(), runtime.GOARCH + "-" + runtime.GOOS, cfg, workloads}
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	if err := enc.Encode(r); err != nil {
		panic(err)
	}
}

func env(key string) string {
	if value := os.Getenv(key); value != "" {
		return value
	}
	return "unknown"
}

func readConfig(path string) (config, error) {
	f, err := os.Open(path)
	if err != nil {
		return config{}, err
	}
	defer f.Close()
	values := map[string]int{}
	s := bufio.NewScanner(f)
	for s.Scan() {
		line := strings.TrimSpace(strings.SplitN(s.Text(), "#", 2)[0])
		if line == "" {
			continue
		}
		parts := strings.SplitN(line, "=", 2)
		if len(parts) != 2 {
			return config{}, fmt.Errorf("invalid config line %q", line)
		}
		value, err := strconv.Atoi(strings.TrimSpace(parts[1]))
		if err != nil {
			return config{}, err
		}
		values[strings.TrimSpace(parts[0])] = value
	}
	if err := s.Err(); err != nil {
		return config{}, err
	}
	c := config{values["version"], values["warmup_iterations"], values["measured_iterations"], values["concurrency"], values["payload_bytes"], values["breaker_failure_percent"], values["discovery_endpoints"], values["queue_capacity"], values["queue_messages"], values["queue_consumer_delay_us"], values["overload_concurrency"]}
	if c.Version != 1 || c.MeasuredIterations < 1 || c.Concurrency < 1 || c.DiscoveryEndpoints < 1 || c.QueueCapacity < 1 || c.QueueMessages < 1 || c.OverloadConcurrency < 1 || c.BreakerFailurePercent < 0 || c.BreakerFailurePercent > 100 {
		return config{}, errors.New("invalid v1 benchmark configuration")
	}
	return c, nil
}

func measure(name string, operations int, fn func() ([]time.Duration, map[string]uint64)) measurement {
	runtime.GC()
	var before, after runtime.MemStats
	runtime.ReadMemStats(&before)
	started := time.Now()
	latencies, counters := fn()
	elapsed := time.Since(started)
	runtime.ReadMemStats(&after)
	sort.Slice(latencies, func(i, j int) bool { return latencies[i] < latencies[j] })
	return measurement{name, operations, float64(elapsed) / float64(time.Millisecond), float64(operations) / elapsed.Seconds(), percentile(latencies, 50), percentile(latencies, 95), percentile(latencies, 99), after.Mallocs - before.Mallocs, after.TotalAlloc - before.TotalAlloc, peakRSS(), counters}
}

func percentile(values []time.Duration, p int) float64 {
	if len(values) == 0 {
		return 0
	}
	return float64(values[(len(values)-1)*p/100]) / float64(time.Microsecond)
}
func peakRSS() uint64 {
	var usage syscall.Rusage
	if syscall.Getrusage(syscall.RUSAGE_SELF, &usage) != nil {
		return 0
	}
	value := uint64(usage.Maxrss)
	if runtime.GOOS == "darwin" {
		value /= 1024
	}
	return value
}

func restTransport(c config) measurement {
	conf := rest.RestConf{ServiceConf: service.ServiceConf{Name: "benchmark", Mode: service.TestMode, Log: logx.LogConf{Mode: "console"}}, Host: "127.0.0.1", Port: 1, MaxBytes: 1 << 20, Timeout: 3000}
	server, err := rest.NewServer(conf)
	if err != nil {
		panic(err)
	}
	server.AddRoute(rest.Route{Method: http.MethodPost, Path: "/echo", Handler: func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			panic(err)
		}
		_, _ = w.Write(body)
	}})
	handler, err := rest.NewServerless(server)
	if err != nil {
		panic(err)
	}
	testServer := httptest.NewUnstartedServer(http.HandlerFunc(handler.Serve))
	testServer.Listener, err = net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	testServer.Start()
	defer testServer.Close()
	client := testServer.Client()
	payload := bytes.Repeat([]byte{'x'}, c.PayloadBytes)
	request := func() {
		response, err := client.Post(testServer.URL+"/echo", "application/octet-stream", bytes.NewReader(payload))
		if err != nil {
			panic(err)
		}
		body, err := io.ReadAll(response.Body)
		_ = response.Body.Close()
		if err != nil || len(body) != len(payload) {
			panic("REST echo failed")
		}
	}
	for i := 0; i < c.WarmupIterations; i++ {
		request()
	}
	return concurrentMeasure("rest_transport", c.MeasuredIterations, c.Concurrency, request)
}

type echoService interface{}

func echoHandler(_ interface{}, ctx context.Context, decode func(interface{}) error, _ grpc.UnaryServerInterceptor) (interface{}, error) {
	in := new(wrapperspb.BytesValue)
	if err := decode(in); err != nil {
		return nil, err
	}
	return in, nil
}

func grpcTransport(c config) measurement {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	server := grpc.NewServer()
	server.RegisterService(&grpc.ServiceDesc{ServiceName: "benchmark.Echo", HandlerType: (*echoService)(nil), Methods: []grpc.MethodDesc{{MethodName: "Echo", Handler: echoHandler}}}, struct{}{})
	go func() {
		if err := server.Serve(listener); err != nil {
			panic(err)
		}
	}()
	defer server.Stop()
	conn, err := grpc.NewClient(listener.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		panic(err)
	}
	defer conn.Close()
	payload := &wrapperspb.BytesValue{Value: bytes.Repeat([]byte{'x'}, c.PayloadBytes)}
	request := func() {
		output := new(wrapperspb.BytesValue)
		if err := conn.Invoke(context.Background(), "/benchmark.Echo/Echo", payload, output); err != nil || len(output.Value) != c.PayloadBytes {
			panic("gRPC echo failed")
		}
	}
	for i := 0; i < c.WarmupIterations; i++ {
		request()
	}
	return concurrentMeasure("grpc_transport", c.MeasuredIterations, c.Concurrency, request)
}

func concurrentMeasure(name string, operations, concurrency int, request func()) measurement {
	return measure(name, operations, func() ([]time.Duration, map[string]uint64) {
		latencies := make(chan time.Duration, operations)
		var wg sync.WaitGroup
		for worker := 0; worker < concurrency; worker++ {
			wg.Add(1)
			go func(offset int) {
				defer wg.Done()
				for i := offset; i < operations; i += concurrency {
					started := time.Now()
					request()
					latencies <- time.Since(started)
				}
			}(worker)
		}
		wg.Wait()
		close(latencies)
		result := make([]time.Duration, 0, operations)
		for latency := range latencies {
			result = append(result, latency)
		}
		return result, map[string]uint64{"completed": uint64(len(result))}
	})
}

func breakerFailure(c config) measurement {
	b := breaker.NewBreaker(breaker.WithName("benchmark"))
	failure := errors.New("upstream failure")
	return measure("circuit_breaker_partial_failure", c.MeasuredIterations, func() ([]time.Duration, map[string]uint64) {
		latencies := make([]time.Duration, 0, c.MeasuredIterations)
		var failed, rejected uint64
		for i := 0; i < c.MeasuredIterations; i++ {
			started := time.Now()
			upstreamFailure := i%100 < c.BreakerFailurePercent
			err := b.Do(func() error {
				if upstreamFailure {
					return failure
				}
				return nil
			})
			latencies = append(latencies, time.Since(started))
			if errors.Is(err, breaker.ErrServiceUnavailable) {
				rejected++
			} else if err != nil {
				failed++
			}
		}
		return latencies, map[string]uint64{"upstream_failures": failed, "rejected": rejected}
	})
}

func overloadRecovery(c config) measurement {
	return measure("overload_recovery", c.MeasuredIterations, func() ([]time.Duration, map[string]uint64) {
		permits := make(chan struct{}, c.OverloadConcurrency)
		for i := 0; i < c.OverloadConcurrency; i++ {
			permits <- struct{}{}
		}
		latencies := make([]time.Duration, 0, c.MeasuredIterations)
		var rejected uint64
		for i := 0; i < c.MeasuredIterations/2; i++ {
			started := time.Now()
			select {
			case permits <- struct{}{}:
				<-permits
			default:
				rejected++
			}
			latencies = append(latencies, time.Since(started))
		}
		<-permits
		started := time.Now()
		recovered := uint64(0)
		select {
		case permits <- struct{}{}:
			recovered = 1
		default:
		}
		recovery := uint64(time.Since(started) / time.Microsecond)
		for i := c.MeasuredIterations / 2; i < c.MeasuredIterations; i++ {
			started := time.Now()
			select {
			case permits <- struct{}{}:
				<-permits
			default:
			}
			latencies = append(latencies, time.Since(started))
		}
		return latencies, map[string]uint64{"rejected": rejected, "recovered": recovered, "recovery_us": recovery}
	})
}

func discoverySnapshot(c config) measurement {
	endpoints := make([]string, c.DiscoveryEndpoints)
	for i := range endpoints {
		endpoints[i] = fmt.Sprintf("http://127.0.0.1:%d", 10000+i)
	}
	operations := c.MeasuredIterations
	if operations > 100 {
		operations = 100
	}
	return measure("large_discovery_snapshot", operations, func() ([]time.Duration, map[string]uint64) {
		latencies := make([]time.Duration, 0, operations)
		var snapshot []string
		for i := 0; i < operations; i++ {
			started := time.Now()
			snapshot = append([]string(nil), endpoints...)
			latencies = append(latencies, time.Since(started))
		}
		return latencies, map[string]uint64{"snapshot_endpoints": uint64(len(snapshot))}
	})
}

func queueSaturation(c config) measurement {
	return measure("queue_saturation", c.QueueMessages, func() ([]time.Duration, map[string]uint64) {
		queue := make(chan int, c.QueueCapacity)
		var wg sync.WaitGroup
		wg.Add(1)
		go func() {
			defer wg.Done()
			for range queue {
				time.Sleep(time.Duration(c.QueueConsumerDelayUS) * time.Microsecond)
			}
		}()
		latencies := make([]time.Duration, 0, c.QueueMessages)
		for i := 0; i < c.QueueMessages; i++ {
			started := time.Now()
			queue <- i
			latencies = append(latencies, time.Since(started))
		}
		close(queue)
		wg.Wait()
		return latencies, map[string]uint64{"processed": uint64(c.QueueMessages), "capacity": uint64(c.QueueCapacity)}
	})
}
