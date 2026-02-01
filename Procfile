
src-server dir=src ready=http:8999: caddy file-server --listen localhost:8999 2>&1 | \
    caddylogs
binary-server after=src-server ready=http:9000: caddy file-server --listen localhost:9000 2>&1 | \
    caddylogs
