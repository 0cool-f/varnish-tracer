#!/usr/bin/env bash
# End-to-end test: a real Varnish with vcl/tracer.vcl, the tracer in --dry-run and real requests, checked on the printed spans.
# Needs docker (or DOCKER=podman), curl and jq; the argument is a static varnish-tracer binary that runs in the Varnish image.
# VARNISH_IMAGE is the official image the test image is built from; the test image is kept for the next run.
set -euo pipefail

binary=$(realpath "${1:?usage: run.sh <static varnish-tracer binary>}")
docker=${DOCKER:-docker}
base=${VARNISH_IMAGE:-docker.io/library/varnish:8.0.2-alpine}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
p=vt-e2e
image=localhost/$p-varnish:latest
work=$(mktemp -d)
failures=0

cleanup() {
  if [ "$failures" -ne 0 ] || [ "${KEEP_LOGS:-}" = 1 ]; then
    echo "--- tracer stderr" >&2
    $docker logs "$p-tracer" 2>&1 >/dev/null | tail -n 50 >&2 || true
  fi
  $docker rm -f "$p-varnish" "$p-tracer" "$p-origin" >/dev/null 2>&1 || true
  $docker volume rm -f "$p-workdir" >/dev/null 2>&1 || true
  $docker network rm "$p" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

openssl rand -hex 16 > "$work/tracer.key"
chmod 0644 "$work/tracer.key"

start_varnish() {
  $docker run -d --name "$p-varnish" --network "$p" -p 127.0.0.1::8080 -e VARNISH_HTTP_PORT=8080 \
    -v "$p-workdir:/var/lib/varnish" \
    -v "$here/default.vcl:/etc/varnish/default.vcl:ro" \
    -v "$root/vcl/tracer.vcl:/etc/varnish/tracer.vcl:ro" \
    -v "$work/tracer.key:/etc/varnish/tracer.key:ro" \
    "$image" >/dev/null
  port=$($docker port "$p-varnish" 8080/tcp | head -n 1 | sed 's/.*://')
  for _ in $(seq 100); do
    curl -fs -o /dev/null "http://127.0.0.1:$port/ready" && return 0
    sleep 0.2
  done
  echo "varnish did not start" >&2
  $docker logs "$p-varnish" >&2
  exit 1
}

get() {
  curl -s -o /dev/null "$@" "http://127.0.0.1:$port$path"
}

spans() {
  $docker logs "$p-tracer" 2>/dev/null | grep '^{' || true
}

# Sends sampled requests until one is traced: the tracer and its varnishncsa are attached and every earlier line is printed
barrier() {
  local name=$1
  for i in $(seq 50); do
    path="/$name-$i?sample" get
    for _ in $(seq 5); do
      spans | jq -e --arg p "/$name-$i" 'select(.attributes["url.path"] == $p)' >/dev/null && return 0
      sleep 0.2
    done
  done
  echo "FAIL barrier $name never traced" >&2
  failures=$((failures + 1))
  return 1
}

# All spans of the traces whose SERVER span has the path, as one array
trace() {
  spans | jq -s --arg p "$1" '
    (map(select(.kind == "SERVER" and .attributes["url.path"] == $p) | .trace_id) | unique) as $ids
    | map(select(.trace_id as $t | $ids | index($t)))'
}

expect() {
  local what=$1 path=$2 filter=$3
  local defs='def server: map(select(.kind == "SERVER")); def client: map(select(.kind == "CLIENT")); def attr($k): .attributes[$k];'
  if trace "$path" | jq -e "$defs $filter" >/dev/null; then
    echo "ok   $what"
  else
    echo "FAIL $what"
    trace "$path" | jq -c '.[]'
    failures=$((failures + 1))
  fi
}

metric() {
  curl -fs "http://127.0.0.1:$metrics_port/metrics" | awk -v m="$1" '$1 == m { print $2 }'
}

$docker build -q --build-arg "VARNISH_IMAGE=$base" -t "$image" "$here" >/dev/null
$docker network create "$p" >/dev/null
$docker run -d --name "$p-origin" --network "$p" -e VARNISH_HTTP_PORT=8080 -v "$here/origin.vcl:/etc/varnish/default.vcl:ro" "$image" >/dev/null
start_varnish
$docker run -d --name "$p-tracer" --network "$p" -p 127.0.0.1::9132 \
  -v "$p-workdir:/var/lib/varnish:ro" \
  -v "$work/tracer.key:/etc/varnish/tracer.key:ro" \
  -v "$binary:/usr/local/bin/varnish-tracer:ro" \
  -e VARNISH_TRACER_DRY_RUN=true \
  -e VARNISH_TRACER_METRICS_LISTEN=0.0.0.0:9132 \
  -e VARNISH_TRACER_RED_METRICS=true \
  "$image" varnish-tracer >/dev/null
metrics_port=$($docker port "$p-tracer" 9132/tcp | head -n 1 | sed 's/.*://')
barrier warmup
# The request varnishncsa of --red-metrics attaches on its own; a counted request shows it is reading
for _ in $(seq 50); do
  path=/red-warmup get
  [ "$(curl -fs "http://127.0.0.1:$metrics_port/metrics" | awk '/^varnish_tracer_requests_total/ { s += $2 } END { print s + 0 }')" -gt 0 ] && break
  sleep 0.2
done
red_synth_before=$(metric 'varnish_tracer_requests_total{handling="synth",code="5xx"}')

path=/miss?sample get
path=/hit get
path=/hit?sample get
path=/pass?sample get
path=/esi?sample get
path=/restart?sample get
path=/broken?sample get
path=/plain get
path=/untrusted get -H 'traceparent: 00-11111111111111111111111111111111-2222222222222222-01'
path=/trusted get -H 'X-Test-Trusted: 1' -H 'traceparent: 00-33333333333333333333333333333333-4444444444444444-01'
path=/trusted-unsampled get -H 'X-Test-Trusted: 1' -H 'traceparent: 00-55555555555555555555555555555555-6666666666666666-00'
path=/esibad get
path=/synth get
path=/status/503 get
path=/slow get
for i in 1 2 3 4 5; do
  path=/synth/$i get
done
barrier checks

expect "miss: SERVER span and a CLIENT child for the fetch" /miss '
  (server | length == 1) and (client | length == 1)
  and (server[0] | attr("varnish.handling") == "miss" and attr("http.response.status_code") == 200
       and .parent_span_id == "" and .status == null and attr("varnish.cache_reason") == "e2e"
       and attr("varnish.late_sampling") == null)
  and (client[0].parent_span_id == server[0].span_id)
  and (client[0] | attr("varnish.backend") != null and attr("http.response.status_code") == 200)'
expect "hit: no CLIENT span, hits counted" /hit '
  (server | length == 1) and (client | length == 0)
  and (server[0] | attr("varnish.handling") == "hit" and attr("varnish.hits") >= 1 and attr("varnish.ttl") > 0)'
expect "pass: SERVER and CLIENT spans" /pass '
  (server | length == 1) and (client | length == 1) and server[0].attributes["varnish.handling"] == "pass"'
expect "esi: one SERVER span, one CLIENT span for the top fetch only" /esi '
  (server | length == 1) and (client | length == 1) and (client[0] | attr("url.path") == "/esi")'
expect "restart: one SERVER span for the last pass, parent of the fetch of the first pass" /restart '
  (server | length == 1) and (client | length == 1)
  and server[0].attributes["varnish.handling"] == "hit" and client[0].parent_span_id == server[0].span_id'
expect "broken backend: SERVER and CLIENT spans are errors" /broken '
  (server | length == 1) and (client | length == 1)
  and (server[0] | attr("http.response.status_code") == 503 and .status.code == "ERROR")
  and (client[0] | attr("http.response.status_code") == 503 and .status.code == "ERROR" and attr("varnish.backend") == null)'
expect "trusted upstream: its trace id and parent are kept" /trusted '
  (server | length == 1) and (client | length == 1)
  and server[0].trace_id == "33333333333333333333333333333333"
  and server[0].parent_span_id == "4444444444444444"'
expect "late synth 503: late span, no CLIENT span" /synth '
  (server | length == 1) and (client | length == 0)
  and (server[0] | attr("varnish.late_sampling") == true and attr("varnish.handling") == "synth"
       and attr("http.response.status_code") == 503)'
expect "late backend 503: late span, no CLIENT span" /status/503 '
  (server | length == 1) and (client | length == 0)
  and (server[0] | attr("varnish.late_sampling") == true and attr("http.response.status_code") == 503)'
expect "late slow response: late span with the slow time to first byte" /slow '
  (server | length == 1) and (server[0] | attr("varnish.late_sampling") == true and attr("varnish.time_to_first_byte") > 2)'

check_absent() {
  local what=$1 filter=$2
  if [ "$(spans | jq -s "map(select($filter)) | length")" = 0 ]; then
    echo "ok   $what"
  else
    echo "FAIL $what"
    spans | jq -c "select($filter)"
    failures=$((failures + 1))
  fi
}
check_absent "unsampled requests have no span" '.attributes["url.path"] | IN("/plain", "/trusted-unsampled", "/red-warmup")'
check_absent "a client traceparent is neither trusted nor sampled" '.attributes["url.path"] == "/untrusted" or .trace_id == "11111111111111111111111111111111"'
check_absent "ESI fragments have no span of their own, a failed one included" '.attributes["url.path"] | IN("/frag1", "/frag2", "/esibad", "/status/503-frag")'

late_flood=$(spans | jq -s '[.[] | select(.attributes["url.path"] | startswith("/synth/"))] | length')
# The limit is 5: /synth, /status/503 and /slow take three tokens, the failed ESI fragment none
if [ "$late_flood" = 2 ]; then
  echo "ok   late sampling stops at the vsthrottle limit"
else
  echo "FAIL late sampling stops at the vsthrottle limit: $late_flood spans for /synth/N, expected 2"
  failures=$((failures + 1))
fi

red_synth_after=$(metric 'varnish_tracer_requests_total{handling="synth",code="5xx"}')
if [ $((red_synth_after - red_synth_before)) = 6 ] &&
  [ "$(metric varnish_tracer_requests_malformed_total)" = 0 ] &&
  [ "$(metric 'varnish_tracer_request_ttfb_seconds_bucket{handling="miss",le="+Inf"}')" -ge 1 ]; then
  echo "ok   request metrics count every synthetic 5xx"
else
  echo "FAIL request metrics: synth 5xx went from $red_synth_before to $red_synth_after, expected +6"
  curl -fs "http://127.0.0.1:$metrics_port/metrics" | grep -v '^#' | grep -v ' 0$'
  failures=$((failures + 1))
fi

# A new Varnish container on the same working directory, as a changed image or definition makes the module do
$docker rm -f "$p-varnish" >/dev/null
start_varnish
barrier recreated && echo "ok   spans resume after the Varnish container is recreated"
if [ "$($docker inspect -f '{{.State.Running}} {{.RestartCount}}' "$p-tracer")" = "true 0" ]; then
  echo "ok   the tracer kept running across the recreation"
else
  echo "FAIL the tracer kept running across the recreation: $($docker inspect -f '{{.State.Status}} exit {{.State.ExitCode}}' "$p-tracer")"
  failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all checks passed"
