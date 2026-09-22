# Operating aibrain-kernel.
#
#   make dev              postgres + aibrain-core + the UI, in this terminal
#   make stop             stop the two native processes; the database stays up
#   make docker-up        start the containers (PROFILE=full adds the watcher)
#   make docker-down      stop and remove the containers; the data volume stays
#   make docker-redeploy  rebuild the watcher image and recreate the containers
#   make test             the whole suite, scripts/test.sh
#
# `docker compose` resolves to podman-compose on this machine; override with
# COMPOSE=... if yours is elsewhere.

COMPOSE ?= docker compose
PROFILE ?=
COMPOSE_FLAGS := $(if $(PROFILE),--profile $(PROFILE),)

.DEFAULT_GOAL := help
.PHONY: help dev stop docker-up docker-down docker-redeploy test

help:
	@sed -n '3,8p' $(MAKEFILE_LIST) | sed 's/^#   //'

dev:
	./dev.sh

stop:
	@pkill -f 'aibrain-core serve' && echo "stopped aibrain-core" || echo "aibrain-core was not running"
	@pkill -f 'python3? -m aibrain' && echo "stopped aibrain (ui)" || echo "aibrain (ui) was not running"

docker-up:
	$(COMPOSE) $(COMPOSE_FLAGS) up -d

docker-down:
	$(COMPOSE) --profile full down

docker-redeploy:
	$(COMPOSE) --profile full build --pull watcher
	$(COMPOSE) --profile full up -d --force-recreate

test:
	./scripts/test.sh
