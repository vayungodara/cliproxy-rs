package main

// Live interop helper, overlaid as cmd/zzmdns/main.go at 6fecc6e (see README.md).
// It runs only Go's discovery code: no proxy server, no outbound HTTP.
//
//	zzmdns advertise <iface> <port> <service-name>   advertise until SIGINT/SIGTERM
//	zzmdns browse <iface> <seconds>                  Go's `discover --json`

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/cmd"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/discovery"
)

func main() {
	switch os.Args[1] {
	case "advertise":
		port, _ := strconv.Atoi(os.Args[3])
		cfg, err := config.ParseConfigBytes([]byte(fmt.Sprintf(
			"discovery:\n  enabled: true\n  service-name: %q\n  interfaces:\n    include: [%s]\n", os.Args[4], os.Args[2])))
		if err != nil {
			panic(err)
		}
		spec, err := discovery.BuildServiceSpec(cfg, port, false)
		if err != nil {
			panic(err)
		}
		adv := discovery.NewZeroconfAdvertiser()
		if err := adv.Start(context.Background(), spec); err != nil {
			panic(err)
		}
		fmt.Printf("advertising %s.%s on %v\n", spec.InstanceName, spec.ServiceType, spec.AdvertisedIPs)
		ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
		<-ctx.Done()
		stop()
		_ = adv.Stop()
	case "browse":
		seconds, _ := strconv.Atoi(os.Args[3])
		os.Exit(cmd.DoDiscoverWithOptions(cmd.DiscoverOptions{
			Timeout: time.Duration(seconds) * time.Second, JSONOutput: true, Include: []string{os.Args[2]},
		}))
	}
}
