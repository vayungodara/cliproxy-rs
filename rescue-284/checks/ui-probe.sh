#!/bin/sh
# Which dashboard does a binary serve? Run in a private network namespace.
ip link set lo up
d=$(mktemp -d); mkdir "$d/auth"
printf 'config-version: 8\nserver:\n  host: "127.0.0.1"\n  port: 8317\naccess:\n  api-keys:\n    - "k"\nmanagement:\n  secret-key: "m"\noauth:\n  auth-dir: "%s/auth"\n' "$d" > "$d/config.yaml"
(cd "$d" && exec "$1" --config "$d/config.yaml" --local-model) > "$d/proxy.log" 2>&1 &
p=$!
trap 'kill $p 2>/dev/null; rm -rf "$d"' EXIT
i=0; until curl -fs -m 2 -o /dev/null -H "Authorization: Bearer k" http://127.0.0.1:8317/v1/models; do i=$((i+1)); [ $i -lt 50 ] || exit 1; sleep 0.1; done
curl -s -m 5 --compressed http://127.0.0.1:8317/management.html > "$d/m.html"
js=$(grep -o 'assets/index-[A-Za-z0-9_-]*\.js' "$d/m.html" | head -1)
echo "html bytes: $(wc -c < "$d/m.html")  js: $js"
curl -s -m 5 --compressed "http://127.0.0.1:8317/$js" > "$d/app.js"
echo "js bytes: $(wc -c < "$d/app.js")"
for s in ANTHROPIC_DEFAULT_HAIKU_MODEL CLIPROXY_CLIENT_KEY CLIPROXY_API_KEY 'Name at the provider' 'Needs a model'; do
  echo "$s: $(grep -c -F "$s" "$d/app.js")"
done
