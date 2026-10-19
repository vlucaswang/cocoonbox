.PHONY: all api agent deskhost macos app image gate help

MACOS_TEST_ARGS ?=

all: gate api agent ## Gates that run on Linux: clean gate, api, agent

api: ## api: fmt check, lint, test, build
	$(MAKE) -C api fmt-check lint test build

agent: ## agent: fmt check, clippy -D warnings, test (CB_NO_BROWSER=1 skips the Chrome tests)
	cd agent && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked

deskhost: ## deskhost: fmt check, clippy -D warnings, test (macOS or Windows)
	cd deskhost && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked

macos: ## macOS app: build the app and run the swift-testing suite
	cd macos && bash -c 'source scripts/toolchain.sh && swift build --product CocoonBox' && scripts/test.sh $(MACOS_TEST_ARGS)

app: ## macOS app bundle dist/CocoonBox.app for CB_ENDPOINT, ad hoc signed
	cd macos && scripts/bundle.sh

image: ## Syntax-check the image scripts, test cut.sh's dry run (Linux bash) and the package replay (docker)
	for f in image/*.sh image/android/*.sh image/android/rootfs/usr/local/lib/cocoonbox/* image/desk/*.sh image/test/*.sh; do bash -n "$$f" || exit 1; done
	python3 -c "import ast, sys; ast.parse(open(sys.argv[1]).read())" image/cut-proxy.py
	bash image/test/cut-dry-run.sh
	bash image/test/packages.sh

gate: ## Clean gate over the tracked files and the dependencies
	scripts/clean-gate repo
	scripts/clean-gate deps

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'
