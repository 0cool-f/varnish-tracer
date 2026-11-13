# Official Varnish image plus the blobdigest vmod that tracer.vcl imports, at the commit and checksum the production image pins
ARG VARNISH_IMAGE=docker.io/library/varnish:8.0.2-alpine
FROM ${VARNISH_IMAGE}

USER root
RUN set -e; \
    apk add --no-cache curl automake autoconf libtool make py3-docutils; \
    SKIP_CHECK=true install-vmod https://gitlab.com/uplex/varnish/libvmod-blobdigest/-/archive/3b507de6c9e240c5a770ae40716302ec7014405c/libvmod-blobdigest-3b507de6c9e240c5a770ae40716302ec7014405c.tar.gz 2c4be7a84683d02087843453ecc6297bef2419a3702f5864b8a2de149accf11c6e0ba59dd59a6475fc5177a44399573261a6718274b538218aa118cf37eb580c; \
    apk del --no-network curl automake autoconf libtool make py3-docutils
USER varnish
