;;; gnosis-test-openapi.el --- OpenAPI generator tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-openapi-generator)

(defvar gnosis-core--generated-openapi-operations)
(defvar gnosis-core--generated-openapi-operation-count)
(defvar gnosis-core--generated-openapi-path-count)
(defvar gnosis-core--generated-openapi-version)

(defconst gnosis-test-openapi--document
  "{\"openapi\":\"3.1.0\",\"info\":{\"title\":\"Gnosis API\",\"version\":\"1.0.0\"},\"paths\":{\"/v1/z\":{\"post\":{\"operationId\":\"post_z\",\"security\":[{\"bearerAuth\":[]}],\"x-gnosis-required-scopes\":[\"content:write\"],\"parameters\":[{\"in\":\"header\",\"name\":\"Idempotency-Key\",\"required\":true}],\"requestBody\":{\"content\":{\"application/json\":{\"schema\":{\"$ref\":\"#/components/schemas/Write\"}}}},\"responses\":{\"201\":{\"description\":\"ok\",\"content\":{\"application/json\":{\"schema\":{\"$ref\":\"#/components/schemas/Result\"}}}}}}},\"/v1/a\":{\"get\":{\"operationId\":\"get_a\",\"security\":[{\"bearerAuth\":[]}],\"x-gnosis-required-scopes\":[\"read\"],\"responses\":{\"200\":{\"description\":\"ok\",\"content\":{\"application/json\":{\"schema\":{\"$ref\":\"#/components/schemas/Result\"}}}}}}}}}"
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
    (should (= 2 gnosis-core--generated-openapi-path-count))
    (should (= 2 gnosis-core--generated-openapi-operation-count))
    (should (equal '(("get_a" :method "GET" :path "/v1/a"
                      :authenticated t :scopes ("read")
                      :required-headers nil :request-schema nil
                      :response-status 200
                      :response-schema "#/components/schemas/Result")
                     ("post_z" :method "POST" :path "/v1/z"
                      :authenticated t :scopes ("content:write")
                      :required-headers ("Idempotency-Key")
                      :request-schema "#/components/schemas/Write"
                      :response-status 201
                      :response-schema "#/components/schemas/Result"))
                   gnosis-core--generated-openapi-operations))))

(ert-deftest gnosis-openapi-rejects-duplicate-operation-identities ()
  (gnosis-test-openapi--with-files input output
    (with-temp-file input
      (insert (replace-regexp-in-string "post_z" "get_a"
                                        gnosis-test-openapi--document t t)))
    (should-error (gnosis-openapi-generate input output)
                  :type 'gnosis-openapi-error)))

(ert-deftest gnosis-openapi-rejects-unsupported-authentication ()
  (gnosis-test-openapi--with-files input output
    (with-temp-file input
      (insert (replace-regexp-in-string
               "bearerAuth" "apiKeyAuth" gnosis-test-openapi--document t t)))
    (should-error (gnosis-openapi-generate input output)
                  :type 'gnosis-openapi-error)))

(ert-deftest gnosis-openapi-rejects-authentication-without-scopes ()
  (gnosis-test-openapi--with-files input output
    (with-temp-file input
      (insert (string-replace
               "\"x-gnosis-required-scopes\":[\"read\"]," ""
               gnosis-test-openapi--document)))
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
