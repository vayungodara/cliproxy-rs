#!/usr/bin/env python3
"""Sends demo requests through the proxy for <seconds>, with a load that grows over the
run, alternating Responses and Messages requests. Prints one line per request and a
count of client statuses at the end. Usage: traffic.py <proxy-port> <client-key> <seconds> <requests>"""
import collections, json, random, sys, time, urllib.error, urllib.request

port, key, seconds, total = int(sys.argv[1]), sys.argv[2], float(sys.argv[3]), int(sys.argv[4])
random.seed(246)
models = ["deepseek"] * 5 + ["glm"] * 5 + ["kimi"] * 3 + ["qwen"] * 3
# Request times: denser toward the end of the run.
times = sorted(seconds * random.random() ** 0.6 for _ in range(total))
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
seen = collections.Counter()
start = time.time()
for i, at in enumerate(times):
    time.sleep(max(0, start + at - time.time()))
    model = random.choice(models)
    if i % 2:
        url, headers = f"http://127.0.0.1:{port}/v1/messages", {"x-api-key": key, "anthropic-version": "2023-06-01"}
        body = {"model": model, "max_tokens": 64, "stream": i % 4 == 3, "messages": [{"role": "user", "content": "Say hello"}]}
    else:
        url, headers = f"http://127.0.0.1:{port}/v1/responses", {"Authorization": f"Bearer {key}"}
        body = {"model": model, "stream": i % 4 == 2, "input": "Say hello"}
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={**headers, "Content-Type": "application/json"})
    try:
        with opener.open(req, timeout=60) as r:
            status = r.status
            r.read()
    except urllib.error.HTTPError as e:
        status = e.code
    except Exception as e:  # noqa: BLE001
        status = type(e).__name__
    seen[status] += 1
    print(f"{time.time() - start:8.1f} {url.rsplit('/', 1)[1]:9} {model:8} -> {status}", flush=True)
print("client statuses:", dict(seen), flush=True)
