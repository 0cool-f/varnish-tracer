vcl 4.1;

# Backend of the end-to-end test: every response is synthetic, chosen by the path.

import std;
import vtc;

backend none none;

sub vcl_recv {
  if (req.url ~ "^/slow") {
    vtc.sleep(2500ms);
  }
  if (req.url ~ "^/status/5") {
    return (synth(503));
  }
  return (synth(200));
}

sub vcl_synth {
  set resp.http.Content-Type = "text/html";
  set resp.http.Cache-Control = "max-age=60";
  if (req.url ~ "^/esi") {
    set resp.body = {"<p>top</p><esi:include src="/frag1"/><esi:include src="/frag2"/>"};
  } else {
    set resp.body = "origin " + req.url;
  }
  return (deliver);
}
