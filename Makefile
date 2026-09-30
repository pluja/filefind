# Filefind — common development commands. Run `make` to see them all.
#
# The app is built inside the GNOME SDK (Flatpak), so the host needs no GTK
# development packages: only flatpak, and cargo for the core library tests.

APP_ID     := io.github.filefind.Filefind
MANIFEST   := $(APP_ID).yml
SDK        := org.gnome.Sdk//50
LINGUAS    := $(shell cat po/LINGUAS)
DEV_HOME   := $(CURDIR)/.dev
DEMO_HOME  := $(CURDIR)/target/demo
LOCALEDIR  := $(CURDIR)/target/locale

# Runs a command inside the GNOME SDK with the Rust extension.
SDK_RUN = flatpak run --share=network --share=ipc --socket=wayland --socket=fallback-x11 \
	--device=dri --socket=session-bus \
	--filesystem=$(CURDIR) --filesystem=home:ro --filesystem=/media:ro \
	--filesystem=/run/media:ro --filesystem=/mnt:ro --filesystem=$(HOME)/.cargo \
	--env=PATH=/usr/lib/sdk/rust-stable/bin:/usr/bin:/bin \
	--env=CARGO_TARGET_DIR=$(CURDIR)/target/sdk \
	--env=FILEFIND_LOCALEDIR=$(LOCALEDIR) \
	--env=RUST_LOG=$(or $(RUST_LOG),warn) \
	$(if $(DEV_LANG),--env=LANGUAGE=$(DEV_LANG) --env=LANG=$(DEV_LANG).UTF-8) \
	$(1) --command=$(2) $(SDK)

.DEFAULT_GOAL := help
.PHONY: help dev dev-es demo run build test check fmt locale pot po flatpak bundle \
	cargo-sources screenshot uninstall clean setup

help: ## Show this help
	@grep -hE '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "}; {printf "  \033[36mmake %-14s\033[0m %s\n", $$1, $$2}'

setup: ## Install the Flatpak SDK, Rust extension and flatpak-builder (user install)
	flatpak install --user -y flathub $(SDK) org.freedesktop.Sdk.Extension.rust-stable//25.08 org.flatpak.Builder

dev: locale ## Build a debug version and run it (separate data in .dev/, your folders read-only)
	$(call SDK_RUN,,cargo) build -p filefind
	$(call SDK_RUN,--env=XDG_DATA_HOME=$(DEV_HOME)/data --env=XDG_CONFIG_HOME=$(DEV_HOME)/config,$(CURDIR)/target/sdk/debug/filefind)

dev-es: ## Like `make dev`, in Spanish
	$(MAKE) dev DEV_LANG=es

demo: locale ## Run the debug build against a small demo library
	@$(MAKE) --no-print-directory target/demo/.ready
	$(call SDK_RUN,,cargo) build -p filefind
	$(call SDK_RUN,--env=XDG_DATA_HOME=$(DEMO_HOME)/data --env=XDG_CONFIG_HOME=$(DEMO_HOME)/config,$(CURDIR)/target/sdk/debug/filefind)

run: ## Run the installed Flatpak
	flatpak run $(APP_ID)

build: ## Build an optimized binary (target/sdk/release/filefind)
	$(call SDK_RUN,,cargo) build --release -p filefind

test: ## Run the core test suite (extraction, indexing, search)
	cargo test -p filefind-core

check: ## Lint everything with clippy
	$(call SDK_RUN,,cargo) clippy --workspace --all-targets

fmt: ## Format the code
	cargo fmt --all

locale: $(foreach l,$(LINGUAS),$(LOCALEDIR)/$(l)/LC_MESSAGES/filefind.mo) ## Compile translations for development

$(LOCALEDIR)/%/LC_MESSAGES/filefind.mo: po/%.po
	@mkdir -p $(dir $@)
	msgfmt --check -o $@ $<

pot: ## Extract translatable strings into po/filefind.pot
	xgettext --from-code=UTF-8 --language=C --keyword= --keyword=tr --keyword=ntr:1,2 \
		--add-comments=TRANSLATORS --package-name=filefind \
		--files-from=po/POTFILES -o po/filefind.pot

po: pot ## Update the .po files with new strings
	for l in $(LINGUAS); do msgmerge --update --backup=none po/$$l.po po/filefind.pot; done

flatpak: ## Build the Flatpak and install it for the current user
	flatpak run org.flatpak.Builder --user --force-clean --repo=repo --install build-dir $(MANIFEST)

bundle: flatpak ## Create filefind.flatpak, a single file you can share and install
	flatpak build-bundle repo filefind.flatpak $(APP_ID)
	@echo "Install with: flatpak install --user filefind.flatpak"

cargo-sources: ## Regenerate offline Cargo sources for the Flatpak (after changing dependencies)
	uv run --quiet --with aiohttp --with tomlkit python3 build-aux/flatpak-cargo-generator.py \
		Cargo.lock -o build-aux/cargo-sources.json

screenshot: locale ## Render the demo window to target/demo/screenshot.png (QUERY=..., DEV_LANG=es)
	@$(MAKE) --no-print-directory target/demo/.ready
	$(call SDK_RUN,,cargo) build -p filefind
	$(call SDK_RUN,--env=XDG_DATA_HOME=$(DEMO_HOME)/data --env=XDG_CONFIG_HOME=$(DEMO_HOME)/config \
		--env=FILEFIND_SNAPSHOT=$(DEMO_HOME)/screenshot.png --env=FILEFIND_QUERY="$(QUERY)",$(CURDIR)/target/sdk/debug/filefind)
	@echo "Saved $(DEMO_HOME)/screenshot.png"

target/demo/.ready:
	mkdir -p $(DEMO_HOME)/Documents/Work $(DEMO_HOME)/Documents/Taxes $(DEMO_HOME)/Documents/Recipes $(DEMO_HOME)/config/filefind
	cp core/tests/fixtures/sample.pdf "$(DEMO_HOME)/Documents/Work/Quarterly Report.pdf"
	cp core/tests/fixtures/sample.docx "$(DEMO_HOME)/Documents/Work/Board Minutes.docx"
	cp core/tests/fixtures/sample.odt "$(DEMO_HOME)/Documents/Work/Budget Draft.odt"
	cp core/tests/fixtures/sheet.xlsx "$(DEMO_HOME)/Documents/Taxes/Expenses 2025.xlsx"
	cp core/tests/fixtures/sample.doc "$(DEMO_HOME)/Documents/Taxes/Old Letter.doc"
	printf "Grandma's marmalade\n\nIngredients: 1kg Seville oranges, 2kg sugar, 1 lemon.\nSimmer the oranges for two hours, then add the sugar and boil until set.\n" > "$(DEMO_HOME)/Documents/Recipes/Marmalade.md"
	printf "Photosynthesis notes\n\nLight reactions happen in the thylakoid membrane. The Calvin cycle fixes carbon in the stroma.\n" > "$(DEMO_HOME)/Documents/Work/biology notes.txt"
	echo '{"folders":["$(DEMO_HOME)/Documents"]}' > $(DEMO_HOME)/config/filefind/library.json
	touch $@

uninstall: ## Remove the installed Flatpak
	flatpak uninstall --user -y $(APP_ID)

clean: ## Remove build output (keeps .dev data)
	rm -rf target build-dir repo .flatpak-builder filefind.flatpak
