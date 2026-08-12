.POSIX:
.PHONY: all doc do-doc compile do-compile test do-test test-oneshot \
	benchmark-ewoc-dashboard do-benchmark-ewoc-dashboard openapi do-openapi \
	integration do-integration lint do-lint load clean

NIX := $(shell command -v nix 2>/dev/null)
ENV_MAKE = $(MAKE) --no-print-directory
ifeq ($(GNOSIS_ENV_WRAPPED),)
ifneq ($(NIX),)
ENV_MAKE = nix develop --no-write-lock-file path:. --command env GNOSIS_ENV_WRAPPED=1 $(MAKE) --no-print-directory
endif
endif

EMACS ?= emacs
ORG := docs/gnosis.org
TEXI := docs/gnosis.texi
INFO := docs/gnosis.info
LISP_DIR := lisp
TEST_DIR := tests
TOOL_DIR := tools
EL_FILES := gnosis-completion.el gnosis-core.el gnosis-core-assets.el gnosis-edit.el gnosis-nodes.el gnosis-dashboard.el gnosis.el gnosis-review.el
LISP_FILES := $(addprefix $(LISP_DIR)/,$(EL_FILES))
TOOL_FILES := $(TOOL_DIR)/gnosis-benchmark-ewoc-dashboard.el \
	$(TOOL_DIR)/gnosis-openapi-generator.el
TEST_FILES := tests/gnosis-test-core.el \
	tests/gnosis-test-core-assets.el \
	tests/gnosis-test-openapi.el \
	tests/gnosis-test-edit.el \
	tests/gnosis-test-nodes.el \
	tests/gnosis-test-entry-smoke.el \
	tests/gnosis-test-dashboard.el \
	tests/gnosis-test-ewoc-dashboard.el \
	tests/gnosis-test-review.el \
	tests/gnosis-test-boundary.el \
	tests/gnosis-package-smoke.el \
	tests/gnosis-test-core-integration.el
TEST_FEATURES := $(basename $(notdir $(TEST_FILES)))
TEST ?=
ERT_SELECTOR ?= t
export ERT_SELECTOR
SELECTED_TEST_FILES = $(if $(TEST),$(TEST),$(TEST_FILES))
SELECTED_TEST_FEATURES = $(basename $(notdir $(SELECTED_TEST_FILES)))

all: doc

doc:
	@$(ENV_MAKE) do-doc

do-doc: $(ORG)
	$(EMACS) --batch -Q \
		--eval "(require 'org)" \
		--eval "(with-current-buffer (find-file \"$(ORG)\") (org-texinfo-export-to-info))" \
		--kill

compile:
	@$(ENV_MAKE) do-compile

do-compile:
	rm -f $(LISP_DIR)/*.elc
	$(EMACS) --batch -q \
		--eval "(setq byte-compile-error-on-warn t byte-compile-warnings t)" \
		-L $(LISP_DIR) -f batch-byte-compile $(LISP_FILES)

test:
	@$(ENV_MAKE) do-test

do-test:
	@build=$$(mktemp -d "$${TMPDIR:-/tmp}/gnosis-tests.XXXXXX"); \
	trap 'rm -rf "$$build"' EXIT HUP INT TERM; \
	cp $(LISP_FILES) $(TOOL_FILES) $(SELECTED_TEST_FILES) "$$build"/; \
	$(EMACS) --batch -q \
		--eval "(setq byte-compile-error-on-warn t byte-compile-warnings t)" \
		-L "$$build" -f batch-byte-compile "$$build"/*.el; \
	$(EMACS) --batch -q -L "$$build" \
		--eval "(mapc #'require '($(SELECTED_TEST_FEATURES)))" \
		--eval "(ert-run-tests-batch-and-exit (read (getenv \"ERT_SELECTOR\")))"

test-oneshot: test

benchmark-ewoc-dashboard:
	@$(ENV_MAKE) do-benchmark-ewoc-dashboard

do-benchmark-ewoc-dashboard:
	$(EMACS) --batch -q -L $(LISP_DIR) \
		--eval "(setq load-prefer-newer t)" \
		-l $(TOOL_DIR)/gnosis-benchmark-ewoc-dashboard.el

openapi:
	@$(ENV_MAKE) do-openapi

do-openapi:
	@GNOSIS_OPENAPI_INPUT="$(OPENAPI_FILE)" \
	GNOSIS_OPENAPI_OUTPUT="$(OPENAPI_OUTPUT)" \
	$(EMACS) --batch -Q -L $(TOOL_DIR) \
		-l gnosis-openapi-generator \
		-f gnosis-openapi-generate-from-environment

integration:
	@$(ENV_MAKE) do-integration

do-integration:
	@tools/run-temp-integration.sh

lint:
	@$(ENV_MAKE) do-lint

do-lint:
	@lisp_version=$$(sed -n 's/^;; Version: //p' lisp/gnosis.el); \
	news_version=$$(sed -n 's/^\* //p' NEWS.org | head -n 1); \
	if test -z "$$lisp_version" || test "$$lisp_version" != "$$news_version"; then \
		echo "Version mismatch: gnosis.el=$$lisp_version NEWS.org=$$news_version"; \
		exit 1; \
	fi
	@set -e; for file in $(LISP_FILES); do \
		echo "checkdoc $$file"; \
		$(EMACS) --batch -q -L $(LISP_DIR) \
			--eval "(require 'checkdoc)" \
			--eval "(progn (checkdoc-file \"$$file\") (let ((buffer (get-buffer \"*Warnings*\"))) (when (and buffer (> (buffer-size buffer) 0)) (kill-emacs 1))))"; \
	done
	$(EMACS) --batch -q \
		--eval "(require 'check-declare)" \
		--eval "(let ((errors (apply #'check-declare-files command-line-args-left))) (kill-emacs (if errors 1 0)))" \
		$(LISP_FILES)
	$(EMACS) --batch -q -L $(LISP_DIR) \
		--eval "(package-initialize)" \
		--eval "(require 'package-lint)" \
		--eval "(let ((add-pinned (lambda (&rest _) (add-to-list 'package-alist (list 'keymap-popup (package-desc-create :name 'keymap-popup :version '(0 4 0) :summary \"Pinned UI dependency\" :reqs '((emacs (29 1))) :kind 'single :archive \"pinned\" :dir nil)))))) (funcall add-pinned) (advice-add 'package-initialize :after add-pinned))" \
		--eval "(setq package-lint-main-file \"lisp/gnosis.el\")" \
		-f package-lint-batch-and-exit $(LISP_FILES)

load:
	rm -f $(LISP_DIR)/*.elc
	@emacsclient --eval "(dolist (symbol \
	    '(gnosis-dashboard-mode-map gnosis-dashboard-themata-mode-map \
	      gnosis-nodes-map gnosis-themata-map gnosis-node-search-map \
	      gnosis-node-sort-map gnosis-node-page-map gnosis-nodes-page-mode-map \
	      gnosis-edit-mode-map gnosis-review-mode-map)) \
	  (when (boundp symbol) (makunbound symbol)))" > /dev/null
	@set -e; for file in $(EL_FILES); do \
		emacsclient -e "(load-file \"$(shell pwd)/$(LISP_DIR)/$$file\")" > /dev/null; \
	done
	@emacsclient --eval "(dolist (buffer (buffer-list)) \
	  (with-current-buffer buffer \
	    (pcase major-mode \
	      ('gnosis-dashboard-mode (use-local-map gnosis-dashboard-mode-map)) \
	      ('gnosis-dashboard-themata-mode \
	       (use-local-map gnosis-dashboard-themata-mode-map)) \
	      ('gnosis-nodes-page-mode (use-local-map gnosis-nodes-page-mode-map)) \
	      ('gnosis-edit-mode (use-local-map gnosis-edit-mode-map)) \
	      ('gnosis-review-mode (use-local-map gnosis-review-mode-map)) \
	      )))" > /dev/null
	@echo "Loaded $(words $(EL_FILES)) files."

clean:
	rm -rf .test-results .test-results.* build result
	rm -f $(TEXI) $(INFO) *-pkg.el* $(LISP_DIR)/*.elc $(TEST_DIR)/*.elc
