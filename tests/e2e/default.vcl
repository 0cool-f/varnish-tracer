vcl 4.1;

# Varnish under test: X-Test-Trusted picks the trusted entry point, "sample" in the query samples in vcl_recv,
# and 5xx or slow responses that were not sampled are traced late.

import std;
import vsthrottle;

include "tracer.vcl";

backend origin {
  .host = "vt-e2e-origin";
  .port = "8080";
}

# Nothing listens there, so every fetch fails
backend broken {
  .host = "127.0.0.1";
  .port = "9";
}

sub vcl_recv {
  if (req.http.X-Test-Trusted) {
    call tracer_recv_trusted;
  } else {
    call tracer_recv;
  }
  unset req.http.X-Test-Trusted;
  if (!req.http.traceparent && req.restarts == 0 && req.esi_level == 0 && req.url ~ "[?&]sample") {
    call tracer_start;
  }
  # The marker is not part of the cache key, so a sampled request can hit an object cached by an unsampled one
  set req.url = regsub(req.url, "[?&]sample$", "");
  set req.backend_hint = origin;
  if (req.url ~ "^/broken") {
    set req.backend_hint = broken;
  }
  if (req.url ~ "^/synth") {
    return (synth(503));
  }
  if (req.url ~ "^/pass") {
    return (pass);
  }
}

sub vcl_backend_response {
  if (bereq.url ~ "^/esi") {
    set beresp.do_esi = true;
  }
}

sub tracer_late {
  if (!req.http.X-Tracer-Span &&
      (resp.status >= 500 || now - req.time > 2s) &&
      !vsthrottle.is_denied("tracer_late", 5, 60s)) {
    call tracer_start_late;
  }
}

sub vcl_deliver {
  if (req.url ~ "^/restart" && req.restarts == 0) {
    return (restart);
  }
  set req.http.X-Tracer-Reason = "e2e";
  call tracer_late;
  call tracer_deliver;
}

sub vcl_synth {
  call tracer_late;
}
