;;; gnosis-test-boundary.el --- Client boundary tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis)
(require 'gnosis-core)
(require 'gnosis-edit)
(require 'gnosis-review)

(defun gnosis-test-boundary--library-string (feature)
  "Return source text for FEATURE's library."
  (with-temp-buffer
    (insert-file-contents (locate-library (symbol-name feature)))
    (buffer-string)))

(ert-deftest gnosis-boundary-loads-only-six-runtime-modules ()
  (dolist (feature '(gnosis gnosis-core gnosis-core-assets
                     gnosis-edit gnosis-review gnosis-completion))
    (should (require feature nil t)))
  (dolist (feature '(gnosis-db gnosis-sqlite gnosis-algorithm
                     gnosis-dashboard gnosis-history gnosis-anki))
    (should-not (locate-library (symbol-name feature)))))

(ert-deftest gnosis-boundary-exposes-no-domain-or-database-api ()
  (dolist (symbol '(gnosis-core-request gnosis-core-sql
                    gnosis-core-dashboard gnosis-core-nodes
                    gnosis-core-themata gnosis-core-thema-create
                    gnosis-core-review-session-create
                    gnosis-core-review-attempt-answer
                    gnosis-db-select gnosis-db-update))
    (should-not (fboundp symbol))))

(ert-deftest gnosis-boundary-retains-public-command-names ()
  (dolist (command '(gnosis gnosis-add-thema gnosis-edit-thema
                    gnosis-review gnosis-review-topic
                    gnosis-review-resume))
    (should (commandp command))))

(ert-deftest gnosis-boundary-unbound-flows-fail-before-local-mutation ()
  (dolist (buffer '("*Gnosis New*" "*Gnosis Edit*" "*gnosis*"))
    (when-let* ((live (get-buffer buffer))) (kill-buffer live)))
  (dolist (call '((gnosis-add-thema)
                  (gnosis-edit-thema "thema")
                  (gnosis-review)
                  (gnosis-review-topic "node")
                  (gnosis-review-resume)))
    (should-error (eval call t) :type 'gnosis-core-api-unavailable))
  (dolist (buffer '("*Gnosis New*" "*Gnosis Edit*" "*gnosis*"))
    (should-not (get-buffer buffer))))

(ert-deftest gnosis-boundary-has-no-handwritten-domain-dtos-or-routes ()
  (dolist (feature '(gnosis gnosis-core gnosis-edit gnosis-review))
    (let ((source (gnosis-test-boundary--library-string feature)))
      (should-not
       (string-match-p
        "node_ids\\|source_guid\\|content_type\\|concepts\\|operation_id\\|expected_revision\\|receipt\\|attempt_revision\\|proposed_transition"
        source))))
  (let ((source (gnosis-test-boundary--library-string 'gnosis-core)))
    (dolist (route '("/v1/dashboard" "/v1/nodes" "/v1/themata"
                     "/v1/review/" "/v1/documents"))
      (should-not (string-match-p (regexp-quote route) source)))))

(ert-deftest gnosis-boundary-documentation-describes-foundation-state ()
  (skip-unless (file-exists-p "README.org"))
  (dolist (file '("README.org" "docs/gnosis.org"))
    (with-temp-buffer
      (insert-file-contents file)
      (should (re-search-forward "OpenAPI" nil t))
      (should (re-search-forward "not yet available" nil t))
      (should-not (re-search-forward "POST /v1/themata" nil t)))))

(provide 'gnosis-test-boundary)
;;; gnosis-test-boundary.el ends here
