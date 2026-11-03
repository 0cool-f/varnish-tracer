vcl 4.1;

# SPDX-License-Identifier: BSD-2-Clause
#
# Copyright (c) 2026 Fabio Di Giorgio
#
# Redistribution and use in source and binary forms, with or without
# modification, are permitted provided that the following conditions are met:
#
# 1. Redistributions of source code must retain the above copyright notice,
#    this list of conditions and the following disclaimer.
# 2. Redistributions in binary form must reproduce the above copyright notice,
#    this list of conditions and the following disclaimer in the documentation
#    and/or other materials provided with the distribution.
#
# THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
# AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
# IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
# ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
# LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
# CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
# SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
# INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
# CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
# ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
# POSSIBILITY OF SUCH DAMAGE.

# W3C trace context for varnish-tracer: every sampled request gets its own span id, which is forwarded to the backend
# in traceparent and logged through std.log for varnishncsa to read. The include must come before the including VCL's own subs, so that the built-in subs below run first.
#
# Every log record is prefixed with the content of /etc/varnish/tracer.key (32 lowercase hex characters), so client data cannot forge one;
# tracing is disabled while that file is missing or invalid. std.fileread caches the key per call site, so a new key needs a VCL reload.

import std;
import blob;
import blobdigest;

# Removes every trace header a client could send, so it can neither force sampling nor choose ids.
sub tracer_scrub {
  unset req.http.traceparent;
  unset req.http.tracestate;
  unset req.http.X-Tracer-Id;
  unset req.http.X-Tracer-Parent;
  unset req.http.X-Tracer-Span;
  unset req.http.X-Tracer-Reason;
}

# Makes an ESI fragment a child of the Varnish span of the top request.
sub tracer_esi {
  call tracer_scrub;
  if (req_top.http.traceparent) {
    set req.http.traceparent = req_top.http.traceparent;
  }
}

# Logs the span of the first pass again, since a restart is a separate transaction for varnishncsa.
sub tracer_restart {
  if (req.http.X-Tracer-Span && regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") ~ "^[0-9a-f]{32}$") {
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_tp:" + req.http.traceparent);
    if (req.http.X-Tracer-Parent) {
      std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_parent:" + req.http.X-Tracer-Parent);
    }
  }
}

# Entry point for requests from untrusted clients: drops all trace headers.
# The including VCL then calls tracer_start on its own sampling decision.
sub tracer_recv {
  if (req.esi_level > 0) {
    call tracer_esi;
    return;
  }
  if (req.restarts > 0) {
    call tracer_restart;
    return;
  }
  call tracer_scrub;
}

# Entry point for requests from a trusted upstream, for networks the operator controls only: validates the
# upstream traceparent and starts a span when the upstream sampled the request; X-Tracer-* headers are still dropped.
sub tracer_recv_trusted {
  if (req.esi_level > 0) {
    call tracer_esi;
    return;
  }
  if (req.restarts > 0) {
    call tracer_restart;
    return;
  }
  unset req.http.X-Tracer-Id;
  unset req.http.X-Tracer-Parent;
  unset req.http.X-Tracer-Span;
  unset req.http.X-Tracer-Reason;
  # Assigning a header to itself collapses duplicates into one
  set req.http.traceparent = req.http.traceparent;
  if (req.http.traceparent &&
      (req.http.traceparent !~ "^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$" ||
       req.http.traceparent ~ "^00-0{32}-" ||
       req.http.traceparent ~ "-0{16}-[0-9a-f]{2}$")) {
    unset req.http.traceparent;
    unset req.http.tracestate;
  }
  if (req.http.traceparent ~ "[13579bdf]$") {
    call tracer_start;
  }
}

# Starts the Varnish span, once per request: keeps the incoming trace id or creates one, and replaces the parent id with a fresh span id.
# Does nothing while the key is invalid.
sub tracer_start {
  if (req.http.X-Tracer-Span) {
    return;
  }
  if (regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") !~ "^[0-9a-f]{32}$") {
    return;
  }
  set req.http.X-Tracer-Id = blob.encode(HEX, blob = blobdigest.hash(SHA256,
    blob.decode(encoded = server.identity + ":" + req.xid + ":" + std.random(0, 4294967295) + ":" + std.random(0, 4294967295) + ":" + std.real(time = now, fallback = 0.0))));
  set req.http.X-Tracer-Span = regsub(req.http.X-Tracer-Id, "^.{32}(.{16}).*$", "\1");
  if (req.http.traceparent) {
    set req.http.X-Tracer-Parent = regsub(req.http.traceparent, "^00-[0-9a-f]{32}-([0-9a-f]{16})-.*$", "\1");
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_parent:" + req.http.X-Tracer-Parent);
    set req.http.traceparent = regsub(req.http.traceparent, "^(00-[0-9a-f]{32}-).*$", "\1") + req.http.X-Tracer-Span + "-01";
  } else {
    unset req.http.tracestate;
    set req.http.traceparent = regsub(req.http.X-Tracer-Id, "^(.{32})(.{16}).*$", "00-\1-\2-01");
  }
  unset req.http.X-Tracer-Id;
  std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_tp:" + req.http.traceparent);
}

# For vcl_deliver of the including VCL: logs the cache attributes of a traced request.
# X-Tracer-Reason, when the including VCL has set it, is logged as the cache reason.
sub tracer_deliver {
  if (req.http.X-Tracer-Span && regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") ~ "^[0-9a-f]{32}$") {
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_hits:" + obj.hits);
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_ttl:" + obj.ttl);
    if (req.http.X-Tracer-Reason) {
      std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_reason:" + req.http.X-Tracer-Reason);
    }
  }
}

# A traced fetch gets its own span id for the backend request, logged with the Varnish span as its parent and
# sent to the backend in traceparent; the internal headers never reach the backend.
sub vcl_backend_fetch {
  if (bereq.http.X-Tracer-Span &&
      regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") ~ "^[0-9a-f]{32}$" &&
      bereq.http.traceparent ~ "^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$") {
    set bereq.http.X-Tracer-Id = blob.encode(HEX, blob = blobdigest.hash(SHA256,
      blob.decode(encoded = server.identity + ":" + bereq.xid + ":" + std.random(0, 4294967295) + ":" + std.random(0, 4294967295) + ":" + std.real(time = now, fallback = 0.0))));
    set bereq.http.traceparent = regsub(bereq.http.traceparent, "^(00-[0-9a-f]{32}-).*$", "\1") + regsub(bereq.http.X-Tracer-Id, "^.{32}(.{16}).*$", "\1") + "-01";
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_be_parent:" + bereq.http.X-Tracer-Span);
    std.log(regsub(std.fileread("/etc/varnish/tracer.key"), "\s+$", "") + "_be_tp:" + bereq.http.traceparent);
  }
  unset bereq.http.X-Tracer-Parent;
  unset bereq.http.X-Tracer-Span;
  unset bereq.http.X-Tracer-Id;
  unset bereq.http.X-Tracer-Reason;
}

sub vcl_pipe {
  unset bereq.http.X-Tracer-Parent;
  unset bereq.http.X-Tracer-Span;
  unset bereq.http.X-Tracer-Id;
  unset bereq.http.X-Tracer-Reason;
}

# One request's trace ids must never be replayed from the cache to other users.
sub vcl_backend_response {
  if (!bereq.uncacheable) {
    unset beresp.http.traceparent;
    unset beresp.http.tracestate;
    unset beresp.http.traceresponse;
    if (beresp.http.Server-Timing ~ "(?i)(^|,)\s*dt") {
      std.collect(beresp.http.Server-Timing);
      set beresp.http.Server-Timing = regsub(regsuball(beresp.http.Server-Timing, "(?i)(^|,)\s*dt[a-z]*(;[^,]*)?", ""), "^\s*,\s*", "");
      if (beresp.http.Server-Timing !~ "\S") {
        unset beresp.http.Server-Timing;
      }
    }
  }
}
