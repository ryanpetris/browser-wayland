# The binary embeds the viewer (web/dist, built by Vite), so the web build comes first.
#   make            the release binary, viewer included
#   make web        the viewer only (Node 24)
#   make test       cargo test, with the viewer built
#   make run ARGS='--no-tls --listen 127.0.0.1:8080 --exec foot'
#   make docker     the container image
#   make docker-run the image, built if needed, on port 8443 (ARGS go to browser-wayland)
#   make clean

.PHONY: all build web test run docker docker-run clean

all: build

build: web
	cargo build --release --locked

WEB_SRC := $(shell find web/src) web/index.html web/vite.config.js web/package.json web/package-lock.json LICENSE
DIST := web/dist/index.html web/dist/app.js web/dist/app.css web/dist/THIRD_PARTY.txt

web: $(DIST)

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

# the render node's group, for hosts where it isn't world-accessible
docker-run: docker
	docker run --rm --device /dev/dri --group-add $$(stat -c %g /dev/dri/renderD128) --shm-size 1g \
		-p 8443:8443 -p 8443:8443/udp -v bw-data:/home/bw/.config/browser-wayland browser-wayland $(ARGS)

clean:
	rm -rf web/dist target
