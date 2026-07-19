;;; gnosis-test-openapi.el --- OpenAPI generator tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-openapi-generator)

(defvar gnosis-core--generated-openapi-operations)
(defvar gnosis-core--generated-openapi-version)

(defconst gnosis-test-openapi--document
  "{\"openapi\":\"3.1.0\",\"info\":{\"title\":\"Gnosis API\",\"version\":\"1.0.0\"},\"paths\":{\"/v1/z\":{\"post\":{\"operationId\":\"post_z\",\"security\":[{\"bearerAuth\":[]}],\"responses\":{\"200\":{\"description\":\"ok\"}}}},\"/v1/a\":{\"get\":{\"operationId\":\"get_a\",\"security\":[{\"bearerAuth\":[]}],\"responses\":{\"200\":{\"description\":\"ok\"}}}}}}"
  "Small valid OpenAPI document used by generator tests.")

(defmacro gnosis-test-openapi--with-files (input output &rest body)
  "Bind temporary INPUT and OUTPUT paths while evaluating BODY."
  (declare (indent 2) (debug t))
  `(let ((,input (make-temp-file "gnosis-openapi-input-" nil ".json"))
         (,output (make-temp-file "gnosis-openapi-output-" nil ".el")))
     (unwind-protect
         (progn ,@body)
       (dolist (path (list ,input ,output))
         (when (file-exists-p path) (delete-file path))))))

(ert-deftest gnosis-openapi-generates-loadable-deterministic-manifest ()
  (gnosis-test-openapi--with-files input output
    (with-temp-file input (insert gnosis-test-openapi--document))
    (gnosis-openapi-generate input output)
    (let ((first (with-temp-buffer
                   (insert-file-contents output)
                   (buffer-string))))
      (gnosis-openapi-generate input output)
      (should (equal first (with-temp-buffer
                             (insert-file-contents output)
                             (buffer-string)))))
    (load-file output)
    (should (equal "1.0.0" gnosis-core--generated-openapi-version))
    (should (equal '(("get_a" "GET" "/v1/a" t)
                     ("post_z" "POST" "/v1/z" t))
                   gnosis-core--generated-openapi-operations))))

(ert-deftest gnosis-openapi-rejects-duplicate-operation-identities ()
  (gnosis-test-openapi--with-files input output
    (with-temp-file input
      (insert (replace-regexp-in-string "post_z" "get_a"
                                        gnosis-test-openapi--document t t)))
    (should-error (gnosis-openapi-generate input output)
                  :type 'gnosis-openapi-error)))

(ert-deftest gnosis-openapi-rejects-malformed-documents ()
  (gnosis-test-openapi--with-files input output
    (dolist (document '("{}" "{\"openapi\":\"3.0.0\"}"
                        "{\"openapi\":\"3.1.0\",\"info\":{},\"paths\":{}}"
                        "{\"openapi\":\"3.1.0\",\"info\":{\"title\":\"Gnosis API\",\"version\":\"1.0.0\"},\"paths\":{\"/v1/empty\":{}}}"))
      (with-temp-file input (insert document))
      (should-error (gnosis-openapi-generate input output)
                    :type 'gnosis-openapi-error))))

(provide 'gnosis-test-openapi)
;;; gnosis-test-openapi.el ends here
