# The binary embeds the viewer (web/dist, built by Vite), so the web build comes first.
#   make            the release binary, viewer included
#   make web        the viewer only (Node 24)
#   make test       cargo test, with the viewer built
#   make run ARGS='--no-tls --listen 127.0.0.1:8080 --exec foot'
#   make docker     the container image
#   make docker-run the image, built if needed, on port 8443 (ARGS go to browser-wayland)
#   make clean

.PHONY: all build web test run docker docker-run clean FORCE

all: build

build: web
	cargo build --release --locked

BW_VISUALISER ?= 1
export BW_VISUALISER

# All source-archive inputs deliberately invalidate the viewer; directory times cover deleted files.
WEB_SRC := $(shell find web/src web/checks crates docs skills packaging .github) web/index.html web/vite.config.js web/package.json web/package-lock.json Cargo.toml Cargo.lock LICENSE README.md Makefile Dockerfile .dockerignore
DIST := web/dist/index.html web/dist/app.js web/dist/app.css web/dist/THIRD_PARTY.txt

web: $(DIST)

$(DIST) &: web/node_modules/.package-lock.json web/node_modules/.bw-visualiser $(WEB_SRC)
	cd web && npm run build

# Only a changed feature value invalidates the output.
web/node_modules/.bw-visualiser: FORCE | web/node_modules/.package-lock.json
	@test "$$(cat $@ 2>/dev/null)" = '$(BW_VISUALISER)' || printf '%s\n' '$(BW_VISUALISER)' > $@

FORCE:

web/node_modules/.package-lock.json: web/package-lock.json
	cd web && npm ci --no-audit --no-fund

test: web
	cargo test --workspace --locked

run: web
	cargo run --release --locked -- $(ARGS)

docker:
	docker build --build-arg BW_VISUALISER=$(BW_VISUALISER) -t browser-wayland .

# the render node's group, for hosts where it isn't world-accessible
docker-run: docker
	docker run --rm --device /dev/dri --group-add $$(stat -c %g /dev/dri/renderD128) --shm-size 1g \
		-p 8443:8443 -p 8443:8443/udp -v bw-data:/home/bw/.config/browser-wayland browser-wayland $(ARGS)

clean:
	rm -rf web/dist target
