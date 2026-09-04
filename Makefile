# The binary embeds the viewer (web/dist, built by Vite), so the web build comes first.
#   make            the release binary, viewer included
#   make web        the viewer only (Node 24)
#   make test       cargo test, with the viewer built
#   make run ARGS='--no-tls --listen 127.0.0.1:8080 --exec foot'
#   make docker     the container image
#   make clean

WEB_SRC := $(shell find web/src web/index.html web/vite.config.js web/package.json -type f)

.PHONY: all build web test run docker clean

all: build

build: web
	cargo build --release --locked

web: web/dist/index.html

web/dist/index.html: web/node_modules/.package-lock.json $(WEB_SRC)
	cd web && npm run build

web/node_modules/.package-lock.json: web/package-lock.json
	cd web && npm ci --no-audit --no-fund

test: web
	cargo test --workspace

run: web
	cargo run --release -- $(ARGS)

docker:
	docker build -t browser-wayland .

clean:
	rm -rf web/dist target
