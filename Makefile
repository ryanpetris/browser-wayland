# The binary embeds the viewer (web/dist, built by Vite), so the web build comes first.
#   make            the release binary, viewer included
#   make web        the viewer only (Node 24)
#   make test       cargo test, with the viewer built
#   make run ARGS='--no-tls --listen 127.0.0.1:8080 --exec foot'
#   make docker     the container image
#   make clean

# directories included: a deleted source changes its directory's time
WEB_SRC := $(shell find web/src web/index.html web/vite.config.js web/package.json)
DIST := web/dist/index.html web/dist/app.js web/dist/app.css

.PHONY: all build web test run docker clean

all: build

build: web
	cargo build --release --locked

web: $(DIST)

# one build for the three files (grouped target), so a missing one brings back all of them
$(DIST) &: web/node_modules/.package-lock.json $(WEB_SRC)
	cd web && npm run build

web/node_modules/.package-lock.json: web/package-lock.json
	cd web && npm ci --no-audit --no-fund

test: web
	cargo test --workspace --locked

run: web
	cargo run --release --locked -- $(ARGS)

docker:
	docker build -t browser-wayland .

clean:
	rm -rf web/dist target
