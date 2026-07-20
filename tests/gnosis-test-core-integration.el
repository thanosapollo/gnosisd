;;; gnosis-test-core-integration.el --- Live foundation tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-core)
(require 'gnosis-nodes)

(defvar gnosis-core--generated-openapi-operations)
(defvar gnosis-core--generated-openapi-operation-count)
(defvar gnosis-core--generated-openapi-path-count)

(defun gnosis-test-core-integration--authenticated-get-paths ()
  "Return parameter-free authenticated GET paths from the generated contract."
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (cl-loop for operation in gnosis-core--generated-openapi-operations
           for method = (plist-get (cdr operation) :method)
           for path = (plist-get (cdr operation) :path)
           for authenticated = (plist-get (cdr operation) :authenticated)
           when (and authenticated
                     (equal method "GET")
                     (not (string-match-p "{" path)))
           collect path))

(defun gnosis-test-core-integration--available-p ()
  "Return non-nil when live foundation-test variables are present."
  (and (getenv "GNOSIS_HTTP_TEST_URL")
       (getenv "GNOSIS_HTTP_TEST_TOKEN")
       (getenv "GNOSIS_OPENAPI_GENERATED")))

(defmacro gnosis-test-core-integration--with-client (&rest body)
  "Evaluate BODY with explicit integration credentials."
  (declare (indent 0) (debug t))
  `(let ((gnosis-core-base-url (getenv "GNOSIS_HTTP_TEST_URL"))
         (token (getenv "GNOSIS_HTTP_TEST_TOKEN")))
     (cl-letf (((symbol-function 'auth-source-search)
                (lambda (&rest _) (list (list :secret token)))))
       ,@body)))

(ert-deftest gnosis-core-integration-health-and-openapi ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (should (equal "ready" (plist-get (gnosis-core-status) :status)))
    (should (equal "3.1.0" (plist-get (gnosis-core-openapi) :openapi)))))

(ert-deftest gnosis-core-integration-authenticated-transport ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let ((paths (gnosis-test-core-integration--authenticated-get-paths))
          succeeded)
      (should paths)
      (while (and paths (not succeeded))
        (condition-case nil
            (progn
              (gnosis-core--request "GET" (pop paths) nil 200 nil nil t)
              (setq succeeded t))
          ((gnosis-core-validation-error gnosis-core-not-found
            gnosis-core-conflict))))
      (should succeeded))))

(ert-deftest gnosis-core-integration-dashboard-projection ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((gnosis-core--contract-cache nil)
           (projection (gnosis-core-dashboard)))
      (should (integerp (plist-get projection :review_day)))
      (should (proper-list-p (plist-get projection :today)))
      (should (proper-list-p (plist-get projection :queue)))
      (should (proper-list-p (plist-get projection :content))))))

(ert-deftest gnosis-core-integration-generated-manifest ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (should (> gnosis-core--generated-openapi-path-count 10))
  (should (= (length gnosis-core--generated-openapi-operations)
             gnosis-core--generated-openapi-operation-count))
  (should (> (length gnosis-core--generated-openapi-operations) 10))
  (dolist (operation gnosis-core--generated-openapi-operations)
    (let ((identity (car operation))
          (method (plist-get (cdr operation) :method))
          (path (plist-get (cdr operation) :path))
          (authenticated (plist-get (cdr operation) :authenticated)))
      (should (stringp identity))
      (should (member method '("DELETE" "GET" "PATCH" "POST" "PUT")))
      (should (string-prefix-p "/v1/" path))
      (should (memq authenticated '(t nil))))))

(ert-deftest gnosis-core-integration-local-org-sync-journey ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((root (make-temp-file "gnosis-org-sync-" t))
         (gnosis-nodes-directory root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (path (format "integration/%s.org" (gnosis-core-operation-id)))
         (prefix ":PROPERTIES:\n:ID: integration-root\n:END:\n")
         (initial (string-to-unibyte (concat prefix "#+TITLE: Initial\n")))
         (local (string-to-unibyte (concat prefix "#+TITLE: Local\n")))
         (remote (string-to-unibyte (concat prefix "#+TITLE: Remote\n")))
         (conflicting-remote
          (string-to-unibyte (concat prefix "#+TITLE: Remote conflict\n")))
         create document-id)
    (unwind-protect
        (progn
          (setq create
                (gnosis-core-document-create
                 (gnosis-core-operation-id) path initial)
                document-id
                (plist-get (plist-get create :authoritative) :document_id))
          (gnosis-nodes-bootstrap)
          (should (equal initial (gnosis-nodes--read-bytes
                                  (gnosis-nodes--safe-path root path))))
          (gnosis-nodes--write-document path local)
          (gnosis-nodes-sync)
          (should (= 2 (plist-get (gnosis-nodes--base
                                   (gnosis-nodes--read-state) document-id path)
                                  :revision)))
          (gnosis-core-document-replace
           document-id (gnosis-core-operation-id) 2 path remote)
          (gnosis-nodes-sync)
          (should (equal remote (gnosis-nodes--read-bytes
                                 (gnosis-nodes--safe-path root path))))
          (gnosis-nodes--write-document
           "integration/restart.org"
           ":PROPERTIES:\n:ID: integration-restart\n:END:\n#+TITLE: Restart\n")
          (gnosis-nodes-track-file
           (gnosis-nodes--safe-path root "integration/restart.org"))
          (gnosis-nodes--send-outbox (gnosis-nodes--read-state))
          (should (seq-some
                   (lambda (entry) (eq :applied (plist-get entry :status)))
                   (append (plist-get (gnosis-nodes--read-state) :outbox) nil)))
          (gnosis-nodes--write-document
           path (concat prefix "#+TITLE: Local conflict\n"))
          (gnosis-core-document-replace
           document-id (gnosis-core-operation-id) 3 path conflicting-remote)
          (gnosis-nodes-sync)
          (should (equal (string-to-unibyte
                          (concat prefix "#+TITLE: Local conflict\n"))
                         (gnosis-nodes--read-bytes
                          (gnosis-nodes--safe-path root path))))
          (should (= 1 (length (plist-get (gnosis-nodes--read-state)
                                          :conflicts)))))
      (delete-directory root t)))))

(provide 'gnosis-test-core-integration)
;;; gnosis-test-core-integration.el ends here
