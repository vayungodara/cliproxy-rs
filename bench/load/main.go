// Closed-loop HTTP load generator for docs/BENCHMARKS.md. Standard library only.
//
// -c workers each send the request, read the whole response, and repeat until -d has
// passed. A response counts as ok only with status 200 and a body containing -expect,
// so a proxy that answers fast but wrong does not score. Prints one JSON line.
package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"time"
)

type headers []string

func (h *headers) String() string     { return strings.Join(*h, ", ") }
func (h *headers) Set(v string) error { *h = append(*h, v); return nil }

func main() {
	url := flag.String("url", "", "request URL")
	bodyFile := flag.String("body", "", "file with the POST body")
	expect := flag.String("expect", "", "substring every ok response body must contain")
	workers := flag.Int("c", 32, "concurrent workers")
	duration := flag.Duration("d", 30*time.Second, "test duration")
	var hdrs headers
	flag.Var(&hdrs, "H", "request header 'Name: value' (repeatable)")
	flag.Parse()

	body, err := os.ReadFile(*bodyFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	client := &http.Client{Transport: &http.Transport{
		MaxIdleConns:        *workers,
		MaxIdleConnsPerHost: *workers,
		DisableCompression:  true,
	}}

	type result struct {
		lat  []time.Duration
		ok   int
		bad  int
		code map[int]int
	}
	results := make([]result, *workers)
	deadline := time.Now().Add(*duration)
	start := time.Now()
	var wg sync.WaitGroup
	for w := 0; w < *workers; w++ {
		wg.Add(1)
		go func(r *result) {
			defer wg.Done()
			r.code = map[int]int{}
			for time.Now().Before(deadline) {
				req, _ := http.NewRequest("POST", *url, bytes.NewReader(body))
				req.Header.Set("Content-Type", "application/json")
				for _, h := range hdrs {
					name, value, _ := strings.Cut(h, ":")
					req.Header.Set(strings.TrimSpace(name), strings.TrimSpace(value))
				}
				t0 := time.Now()
				resp, err := client.Do(req)
				if err != nil {
					r.bad++
					r.code[0]++
					continue
				}
				data, err := io.ReadAll(resp.Body)
				resp.Body.Close()
				r.code[resp.StatusCode]++
				if err != nil || resp.StatusCode != 200 || !bytes.Contains(data, []byte(*expect)) {
					r.bad++
					continue
				}
				r.ok++
				r.lat = append(r.lat, time.Since(t0))
			}
		}(&results[w])
	}
	wg.Wait()
	elapsed := time.Since(start)

	var lat []time.Duration
	ok, bad := 0, 0
	codes := map[string]int{}
	for _, r := range results {
		lat = append(lat, r.lat...)
		ok += r.ok
		bad += r.bad
		for c, n := range r.code {
			codes[fmt.Sprint(c)] += n
		}
	}
	sort.Slice(lat, func(i, j int) bool { return lat[i] < lat[j] })
	pct := func(p float64) float64 {
		if len(lat) == 0 {
			return 0
		}
		return float64(lat[int(p*float64(len(lat)-1))].Microseconds()) / 1000
	}
	json.NewEncoder(os.Stdout).Encode(map[string]any{
		"ok": ok, "bad": bad, "codes": codes, "seconds": elapsed.Seconds(),
		"rps": float64(ok) / elapsed.Seconds(),
		"p50_ms": pct(0.50), "p90_ms": pct(0.90), "p99_ms": pct(0.99), "max_ms": pct(1),
	})
}
