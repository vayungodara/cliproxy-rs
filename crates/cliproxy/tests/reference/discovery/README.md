# Discovery, CLI and .env goldens

The `../../fixtures/discovery_*.json` files hold outputs of CLIProxyAPI at `6fecc6e`
(and its pinned joho/godotenv v1.5.1), produced by running the real Go code:

- `discovery_go.json`: `internal/discovery` helpers (TXT records, instance names,
  subtypes, service types, interface rules, browse entry conversion and merging,
  `BuildServiceSpec` errors), from `discovery_fixture_test.go`.
- `discovery_cmd_go.json`: the exact stdout, stderr and exit code of
  `runDiscoverWithOptions` for fixed browse results, terminal sanitizing and the
  config interface filters, from `cmd_fixture_test.go`.
- `discovery_main_go.json`: `argvEnablesBoolFlag`, `resolveManagementBaseURL`,
  `modelCatalogUpdaterPlan` and `godotenv.Unmarshal` cases, from
  `main_fixture_test.go`.

The test files are overlaid into the reference packages with `go test -overlay`, so
they can call unexported functions while the reference checkout stays unmodified.
They open no sockets. Regenerate with:

```sh
./gen.sh /absolute/path/to/CLIProxyAPI   # checkout at 6fecc6e, Go 1.26
```

## Live mDNS interop (manual)

`discovery_go_packets.json` holds probe, announcement, answer and goodbye packets sent
by Go's zeroconf advertiser, captured on an isolated dummy interface. To repeat the
two-way check (Linux, root for the dummy interface; no traffic leaves the host):

```sh
sudo ip link add ethcpa0 type dummy
sudo ip link set ethcpa0 multicast on
sudo ip addr add 10.77.0.1/24 dev ethcpa0
sudo ip link set ethcpa0 up
reference=/absolute/path/to/CLIProxyAPI
echo "{\"Replace\": {\"$reference/cmd/zzmdns/main.go\": \"$PWD/mdns_main.go\"}}" > /tmp/overlay.json
(cd "$reference" && go build -overlay /tmp/overlay.json -o /tmp/zzmdns ./cmd/zzmdns)
WRITABLE_PATH=/tmp/go-state /tmp/zzmdns advertise ethcpa0 18317 "Go Box" &
/tmp/zzmdns browse ethcpa0 3 > go.json
cliproxy discover -json -include ethcpa0 -timeout 3 > rs.json
diff go.json rs.json   # identical
```

Then run `cliproxy` with `discovery.enabled: true` and `interfaces.include: [ethcpa0]`
and browse it with both commands again; stopping it with SIGTERM sends the goodbye.
