// Run from a temporary directory inside the pinned CLIProxyAPI reference module.
// Expected values call real Go redaction and use gin_logger.go's exact formatter.
package main

import (
	"encoding/json"
	"fmt"
	"net/http/httptest"
	"os"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/logging"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	log "github.com/sirupsen/logrus"
	logtest "github.com/sirupsen/logrus/hooks/test"
)

func main() {
	queries := []map[string]string{}
	for _, query := range []string{
		"", "x=abc&key=0123456789&x=%20", "key=ab&key=abcd&key=abcde&key=abcdefgh&key=abcdefghi",
		"%6bey=+abcdefghijk+&auth_token=a%2Bb%2Fc%3Dd123456789&z=%", "bad%=clear&secret%=abcdefghi",
		"KEY%5B%5D=123456789&key&token=&password=keep&auth=keep", "a=1&&apikey=123456789&",
		"token=%FF%FF%FF%FF%FF%FF%FF%FF%FF&key=%ZZ123456789", "key=abc;xyz&Api_Key=123456789",
		"AP%C4%B0KEY=123456789",
	} {
		queries = append(queries, map[string]string{"in": query, "out": util.MaskSensitiveQuery(query)})
	}
	lines := []map[string]any{}
	for _, nanos := range []int64{0, 999999, 1000000, 999999999, 1000000000, 1234567890, 23559000123, 60000000000, 60000000001, 61999999999, 3601000000000} {
		latency := time.Duration(nanos)
		if latency > time.Minute {
			latency = latency.Truncate(time.Second)
		} else {
			latency = latency.Truncate(time.Millisecond)
		}
		lines = append(lines, map[string]any{"nanos": nanos, "status": 401, "client": "2001:db8::abcd", "method": "POST", "path": "/v1/responses?key=abcd...ghij", "out": fmt.Sprintf("%3d | %13v | %15s | %-7s \"%s\"", 401, latency, "2001:db8::abcd", "POST", "/v1/responses?key=abcd...ghij")})
	}
	gin.SetMode(gin.TestMode)
	hook := logtest.NewLocal(log.StandardLogger())
	health := []map[string]any{}
	for _, method := range []string{"GET", "HEAD", "POST"} {
		for _, status := range []int{200, 299, 300, 400, 499, 500, 503} {
			hook.Reset()
			engine := gin.New()
			engine.Use(logging.GinLogrusLogger())
			engine.Use(func(c *gin.Context) { c.AbortWithStatus(status) })
			engine.Handle(method, "/healthz", func(c *gin.Context) {})
			engine.ServeHTTP(httptest.NewRecorder(), httptest.NewRequest(method, "/healthz", nil))
			level := ""
			if entry := hook.LastEntry(); entry != nil {
				level = entry.Level.String()
			}
			health = append(health, map[string]any{"method": method, "status": status, "level": level})
		}
	}
	if err := json.NewEncoder(os.Stdout).Encode(map[string]any{"query": queries, "lines": lines, "health": health}); err != nil {
		panic(err)
	}
}
