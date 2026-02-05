echo! "README.md": echo "Hello"; sleep 2

src-server after="echo" dir=src ready=http:8999:
  echo "Starting Caddy on ./src"
  caddy file-server --listen localhost:8999 2>&1 | \
      caddylogs

binary-server after=echo dir=target ready=http:9000:
  echo "Starting Caddy on ./target"
  caddy file-server --listen localhost:9000 2>&1 | \
      caddylogs
