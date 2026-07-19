;;; gnosis-test-core-integration.el --- Live foundation tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-core)

(defvar gnosis-core--generated-openapi-operations)

(defun gnosis-test-core-integration--authenticated-get-paths ()
  "Return parameter-free authenticated GET paths from the generated contract."
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (cl-loop for (_identity method path authenticated)
           in gnosis-core--generated-openapi-operations
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

(ert-deftest gnosis-core-integration-generated-manifest ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (should (> (length gnosis-core--generated-openapi-operations) 10))
  (dolist (operation gnosis-core--generated-openapi-operations)
    (pcase-let ((`(,identity ,method ,path ,authenticated) operation))
      (should (stringp identity))
      (should (member method '("DELETE" "GET" "PATCH" "POST" "PUT")))
      (should (string-prefix-p "/v1/" path))
      (should (memq authenticated '(t nil))))))

(provide 'gnosis-test-core-integration)
;;; gnosis-test-core-integration.el ends here
