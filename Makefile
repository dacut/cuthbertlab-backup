# Build and deploy the GitHub backup.
#
#   make test      run the unit tests
#   make test-emulator
#                  run the tests that need AWS APIs, against a local emulator
#   make moto-start, make moto-stop
#                  run the emulator on its own, e.g. to point the CLI at it
#   make build     cross-compile both Lambda packages (needs cargo-lambda)
#   make init      initialize Terraform against the configured state backend
#   make plan      build, then show what Terraform would change
#   make deploy    build, then apply
#   make run       start a backup run now
#   make status    show the most recent runs

TF      := terraform -chdir=terraform
BACKEND ?= kanga.s3.tfbackend

# Moto, a local emulator of the AWS APIs, installed in a virtualenv of its own.
MOTO_VERSION ?= 5.2.5
MOTO_PORT    ?= 5599
MOTO_DIR     := target/moto
MOTO_SERVER  := $(MOTO_DIR)/venv/bin/moto_server
MOTO_PID     := $(MOTO_DIR)/moto.pid
MOTO_LOG     := $(MOTO_DIR)/moto.log
MOTO_URL     := http://127.0.0.1:$(MOTO_PORT)
MOTO_READY   := curl -fsS -o /dev/null $(MOTO_URL)/moto-api/ 2>/dev/null
# Everything a process needs to use the emulator and nothing that could lead it
# to a real account: no profile, no config files, dummy credentials.
MOTO_ENV     := env -u AWS_PROFILE -u AWS_SESSION_TOKEN -u GITHUB_TOKEN \
	AWS_CONFIG_FILE=/dev/null AWS_SHARED_CREDENTIALS_FILE=/dev/null AWS_EC2_METADATA_DISABLED=true \
	AWS_ACCESS_KEY_ID=testing AWS_SECRET_ACCESS_KEY=testing AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 \
	AWS_ENDPOINT_URL=$(MOTO_URL) S3_FORCE_PATH_STYLE=true

.PHONY: build test test-emulator moto-start moto-stop lint fmt init plan deploy run status

build:
	cargo lambda build --release --arm64 --output-format zip
	@ls -lh target/lambda/*/bootstrap.zip

test:
	cargo test

# Runs the tests marked #[ignore] that create buckets and parameters. Starts
# the emulator unless it is already running, and stops it again if it did.
test-emulator: $(MOTO_SERVER)
	@if $(MOTO_READY); then started=; else $(MAKE) --no-print-directory moto-start || exit 1; started=1; fi; \
	$(MOTO_ENV) cargo test -- --ignored emulator_; status=$$?; \
	if [ -n "$$started" ]; then $(MAKE) --no-print-directory moto-stop; fi; \
	exit $$status

$(MOTO_SERVER):
	python3 -m venv $(MOTO_DIR)/venv
	$(MOTO_DIR)/venv/bin/pip install --quiet 'moto[server]==$(MOTO_VERSION)'

# The emulator keeps everything in memory: stopping it discards all its data.
# To use it by hand, give a command the environment in MOTO_ENV above, e.g.
#   AWS_ENDPOINT_URL=http://127.0.0.1:5599 AWS_ACCESS_KEY_ID=testing \
#     AWS_SECRET_ACCESS_KEY=testing AWS_REGION=us-east-1 aws s3 ls
moto-start: $(MOTO_SERVER)
	@if $(MOTO_READY); then echo "something is already listening at $(MOTO_URL)"; exit 1; fi
	@nohup $(MOTO_SERVER) -H 127.0.0.1 -p $(MOTO_PORT) > $(MOTO_LOG) 2>&1 & echo $$! > $(MOTO_PID)
	@for attempt in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do \
		if $(MOTO_READY); then echo "moto $(MOTO_VERSION) is listening at $(MOTO_URL) (log: $(MOTO_LOG))"; exit 0; fi; \
		if ! kill -0 "$$(cat $(MOTO_PID))" 2>/dev/null; then break; fi; \
		sleep 0.5; \
	done; \
	echo "moto did not start:"; cat $(MOTO_LOG); exit 1

# Only ever signals a process that is still a moto_server, in case the recorded
# process ID has since been reused by something else.
moto-stop:
	@pid="$$(cat $(MOTO_PID) 2>/dev/null)"; \
	if [ -n "$$pid" ] && ps -p "$$pid" -o command= | grep -q moto_server; then \
		kill "$$pid"; \
		for attempt in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do kill -0 "$$pid" 2>/dev/null || break; sleep 0.25; done; \
		echo "moto stopped"; \
	else echo "moto is not running (or was not started by make moto-start)"; fi; \
	: > $(MOTO_PID)

lint:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	$(TF) fmt -check

fmt:
	cargo fmt --all
	$(TF) fmt

init:
	$(TF) init -backend-config=$(BACKEND)

plan: build
	$(TF) plan

deploy: build
	$(TF) apply

run:
	aws stepfunctions start-execution \
		--state-machine-arn "$$($(TF) output -raw state_machine_arn)" \
		--query executionArn --output text

status:
	aws stepfunctions list-executions \
		--state-machine-arn "$$($(TF) output -raw state_machine_arn)" \
		--max-results 10 \
		--query 'executions[].[startDate,status,name]' --output table
