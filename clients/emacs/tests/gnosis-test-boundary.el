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

(ert-deftest gnosis-boundary-loads-only-eight-runtime-modules ()
  (dolist (feature '(gnosis gnosis-core gnosis-core-assets
                     gnosis-edit gnosis-nodes gnosis-review gnosis-completion
                     gnosis-dashboard))
    (should (require feature nil t)))
  (dolist (feature '(gnosis-db gnosis-sqlite gnosis-algorithm
                     gnosis-tl gnosis-history gnosis-anki))
    (should-not (locate-library (symbol-name feature)))))

(ert-deftest gnosis-boundary-exposes-no-domain-or-database-api ()
  (dolist (symbol '(gnosis-core-request gnosis-core-sql
                    gnosis-core-themata
                    gnosis-db-select gnosis-db-update))
    (should-not (fboundp symbol))))

(ert-deftest gnosis-boundary-exposes-typed-http-operations ()
  (dolist (symbol '(gnosis-core-dashboard gnosis-core-nodes gnosis-core-thema
                    gnosis-core-thema-create gnosis-core-thema-update
                    gnosis-core-review-session-create
                    gnosis-core-review-attempt-answer
                    gnosis-core-review-attempt-state
                    gnosis-core-review-attempt-commit))
    (should (fboundp symbol)))
  (should-not (fboundp 'gnosis-core-request)))

(ert-deftest gnosis-boundary-retains-public-command-names ()
  (dolist (command '(gnosis gnosis-add-thema gnosis-edit-thema
                    gnosis-review gnosis-review-topic
                    gnosis-review-resume))
    (should (commandp command))))

(ert-deftest gnosis-boundary-review-flows-have-no-local-domain-backend ()
  (let ((source (gnosis-test-boundary--library-string 'gnosis-review)))
    (should-not
     (string-match-p
      "gnosis-db\\|sqlite\\|gnosis-algorithm\\|gnosis-core-api-unavailable"
      source))))

(ert-deftest gnosis-boundary-has-no-dead-dtos-or-handwritten-routes ()
  (dolist (feature '(gnosis gnosis-core gnosis-edit gnosis-nodes gnosis-review))
    (let ((source (gnosis-test-boundary--library-string feature)))
      (should-not
       (string-match-p
        "node_ids\\|receipt\\|proposed_transition"
        source))))
  (dolist (feature '(gnosis gnosis-edit gnosis-nodes gnosis-review))
    (should-not
     (string-match-p "content_type"
                     (gnosis-test-boundary--library-string feature))))
  (let ((source (gnosis-test-boundary--library-string 'gnosis-core)))
    (dolist (route '("/v1/dashboard" "/v1/nodes" "/v1/themata"
                     "/v1/review/" "/v1/documents"))
      (should-not (string-match-p (regexp-quote route) source)))))

(ert-deftest gnosis-boundary-documentation-describes-org-sync ()
  (skip-unless (file-exists-p "README.org"))
  (dolist (file '("README.org" "docs/gnosis.org"))
    (with-temp-buffer
      (insert-file-contents file)
      (should (re-search-forward "OpenAPI" nil t))
      (should (re-search-forward "gnosis-nodes-bootstrap" nil t))
      (goto-char (point-min))
      (should (search-forward
               "M-x gnosis= for the server-projected dashboard" nil t))
      (goto-char (point-min))
      (should (re-search-forward "start\\(?:s\\)? or resume\\(?:s\\)?" nil t))
      (should (search-forward "=R=" nil t))
      (should (search-forward "active review controls" nil t))
      (should (search-forward "=n=" nil t))
      (should (search-forward "nodes and documents" nil t))
      (should (search-forward "M-x gnosis-review= for due review" nil t))
      (should (re-search-forward "=n= commits the current accepted result" nil t))
      (should (search-forward
               "=o= overrides the accepted result without committing" nil t))
      (should (search-forward "=g= refreshes" nil t))
      (should (search-forward "committed attempt" nil t))
      (goto-char (point-min))
      (should-not (re-search-forward "commits the detected result" nil t))
      (should-not (re-search-forward "POST /v1/themata" nil t))))
  (with-temp-buffer
    (insert-file-contents "docs/gnosis.org")
    (should (re-search-forward "isolated review" nil t))
    (should-not (re-search-forward "does not seed or exercise" nil t))))

(provide 'gnosis-test-boundary)
;;; gnosis-test-boundary.el ends here
