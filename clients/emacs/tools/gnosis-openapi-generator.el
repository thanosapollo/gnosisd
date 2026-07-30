;;; gnosis-openapi-generator.el --- Generate Gnosis contract manifests  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Free Software Foundation, Inc.

;;; Commentary:

;; Convert the daemon's OpenAPI document into a deterministic, temporary
;; operation manifest.  Domain bindings are intentionally generated later,
;; after the public API stabilizes.

;;; Code:

(require 'cl-lib)
(require 'json)
(require 'map)
(require 'subr-x)

(define-error 'gnosis-openapi-error "Invalid Gnosis OpenAPI document")

(defconst gnosis-openapi--methods
  '("delete" "get" "head" "options" "patch" "post" "put" "trace")
  "HTTP operation keys recognized in an OpenAPI path item.")

(defun gnosis-openapi--invalid (message)
  "Signal a generator error with MESSAGE."
  (signal 'gnosis-openapi-error (list message)))

(defun gnosis-openapi--read (path)
  "Return OpenAPI document read from PATH."
  (condition-case nil
      (with-temp-buffer
        (insert-file-contents path)
        (json-parse-buffer :object-type 'hash-table :array-type 'array
                           :null-object nil :false-object :json-false))
    (error (gnosis-openapi--invalid "Cannot read OpenAPI JSON"))))

(defun gnosis-openapi--required-string (table key label)
  "Return string KEY from TABLE or reject with LABEL."
  (let ((value (and (hash-table-p table) (gethash key table))))
    (unless (and (stringp value) (not (string-empty-p value)))
      (gnosis-openapi--invalid (format "Missing OpenAPI %s" label)))
    value))

(defun gnosis-openapi--authenticated-p (operation inherited)
  "Return non-nil when OPERATION or INHERITED security is nonempty."
  (let* ((missing (make-symbol "missing"))
         (specific (gethash "security" operation missing))
         (security (if (eq specific missing) inherited specific))
         (authenticated
          (and (vectorp security) (> (length security) 0)
               (cl-loop for requirement across security
                        thereis
                        (and (hash-table-p requirement)
                             (= (hash-table-count requirement) 1)
                             (let ((scopes (gethash "bearerAuth" requirement
                                                    missing)))
                               (and (vectorp scopes)
                                    (= (length scopes) 0))))))))
    (when (and (vectorp security) (> (length security) 0)
               (not authenticated))
      (gnosis-openapi--invalid "Unsupported OpenAPI security scheme"))
    authenticated))

(defun gnosis-openapi--string-vector (table key label)
  "Return string vector KEY from TABLE as a list, or reject with LABEL."
  (let ((value (or (gethash key table) [])))
    (unless (and (vectorp value)
                 (cl-loop for item across value always (stringp item)))
      (gnosis-openapi--invalid (format "Invalid OpenAPI %s" label)))
    (sort (append value nil) #'string<)))

(defun gnosis-openapi--schema-reference (container)
  "Return application/json schema reference nested in CONTAINER."
  (when-let* ((content (and (hash-table-p container)
                            (gethash "content" container)))
              (media (and (hash-table-p content)
                          (gethash "application/json" content)))
              (schema (and (hash-table-p media) (gethash "schema" media)))
              (reference (and (hash-table-p schema) (gethash "$ref" schema))))
    (unless (stringp reference)
      (gnosis-openapi--invalid "Invalid OpenAPI schema reference"))
    reference))

(defun gnosis-openapi--required-headers (operation)
  "Return required header names declared by OPERATION."
  (let ((parameters (or (gethash "parameters" operation) [])))
    (unless (vectorp parameters)
      (gnosis-openapi--invalid "Invalid OpenAPI parameters"))
    (sort
     (cl-loop for parameter across parameters
              when (and (hash-table-p parameter)
                        (equal "header" (gethash "in" parameter))
                        (eq t (gethash "required" parameter)))
              collect (gnosis-openapi--required-string
                       parameter "name" "header name"))
     #'string<)))

(defun gnosis-openapi--success (responses)
  "Return sole success status and response schema from RESPONSES."
  (let ((statuses
         (sort
          (cl-remove-if-not
           (lambda (status) (string-match-p "\\`2[0-9][0-9]\\'" status))
           (map-keys responses))
          #'string<)))
    (unless (= (length statuses) 1)
      (gnosis-openapi--invalid "OpenAPI operation needs one success response"))
    (let* ((status (car statuses))
           (response (gethash status responses)))
      (cons (string-to-number status)
            (gnosis-openapi--schema-reference response)))))

(defun gnosis-openapi--operation (path method value inherited)
  "Return manifest entry for PATH, METHOD, and operation VALUE."
  (let* ((identity (gnosis-openapi--required-string
                    value "operationId" "operationId"))
         (responses (and (hash-table-p value) (gethash "responses" value)))
         (authenticated (gnosis-openapi--authenticated-p value inherited))
         (scopes (gnosis-openapi--string-vector
                  value "x-gnosis-required-scopes" "required scopes")))
    (unless (and (hash-table-p responses) (> (hash-table-count responses) 0))
      (gnosis-openapi--invalid (format "Operation %s has no responses" identity)))
    (when (and authenticated (not scopes))
      (gnosis-openapi--invalid "Authenticated operation needs required scopes"))
    (let ((success (gnosis-openapi--success responses)))
      (list identity
            :method (upcase method)
            :path path
            :authenticated (and authenticated t)
            :scopes scopes
            :required-headers (gnosis-openapi--required-headers value)
            :request-schema (gnosis-openapi--schema-reference
                             (gethash "requestBody" value))
            :response-status (car success)
            :response-schema (cdr success)))))

(defun gnosis-openapi--path-operations (path item inherited)
  "Return generated operations for PATH from path ITEM."
  (unless (and (string-prefix-p "/" path) (hash-table-p item))
    (gnosis-openapi--invalid "Invalid OpenAPI path item"))
  (cl-loop for method in gnosis-openapi--methods
           for operation = (gethash method item)
           when operation
           collect (gnosis-openapi--operation
                    path method operation inherited)))

(defun gnosis-openapi--operations (document)
  "Return sorted operation manifest from DOCUMENT."
  (let* ((paths (gethash "paths" document))
         (inherited (gethash "security" document))
         (operations
          (cl-loop for path in (sort (map-keys paths) #'string<)
                   append (gnosis-openapi--path-operations
                           path (gethash path paths) inherited)))
         (identities (mapcar #'car operations)))
    (unless operations
      (gnosis-openapi--invalid "OpenAPI document has no operations"))
    (unless (= (length identities)
               (length (delete-dups (copy-sequence identities))))
      (gnosis-openapi--invalid "Duplicate OpenAPI operationId"))
    (sort operations (lambda (left right)
                       (string< (car left) (car right))))))

(defun gnosis-openapi--contract (document)
  "Return API version, path count, and operation manifest from DOCUMENT."
  (unless (and (hash-table-p document)
               (equal "3.1.0" (gethash "openapi" document)))
    (gnosis-openapi--invalid "OpenAPI 3.1.0 is required"))
  (let* ((info (gethash "info" document))
         (version (gnosis-openapi--required-string
                   info "version" "info.version"))
         (title (gnosis-openapi--required-string info "title" "info.title"))
         (paths (gethash "paths" document)))
    (unless (and (equal title "Gnosis API")
                 (hash-table-p paths) (> (hash-table-count paths) 0))
      (gnosis-openapi--invalid "Invalid Gnosis OpenAPI metadata"))
    (list version (hash-table-count paths)
          (gnosis-openapi--operations document))))

(defun gnosis-openapi--write (output version path-count operations)
  "Write VERSION, PATH-COUNT, and OPERATIONS manifest to OUTPUT."
  (make-directory (file-name-directory (expand-file-name output)) t)
  (with-temp-file output
    (insert ";;; gnosis-openapi-contract.el --- Generated Gnosis API manifest  -*- lexical-binding: t; -*-\n\n"
            ";; Generated from gnosisd /v1/openapi.json.  Do not edit.\n\n"
            "(defconst gnosis-core--generated-openapi-version\n  ")
    (prin1 version (current-buffer))
    (insert ")\n\n(defconst gnosis-core--generated-openapi-path-count\n  ")
    (prin1 path-count (current-buffer))
    (insert ")\n\n(defconst gnosis-core--generated-openapi-operation-count\n  ")
    (prin1 (length operations) (current-buffer))
    (insert ")\n\n(defconst gnosis-core--generated-openapi-operations\n  '")
    (prin1 operations (current-buffer))
    (insert ")\n\n(provide 'gnosis-openapi-contract)\n"
            ";;; gnosis-openapi-contract.el ends here\n")))

(defun gnosis-openapi-generate (input output)
  "Generate a temporary contract manifest from INPUT at OUTPUT."
  (pcase-let ((`(,version ,path-count ,operations)
               (gnosis-openapi--contract (gnosis-openapi--read input))))
    (gnosis-openapi--write output version path-count operations)
    output))

(defun gnosis-openapi-generate-from-environment ()
  "Generate using GNOSIS_OPENAPI_INPUT and GNOSIS_OPENAPI_OUTPUT."
  (let ((input (getenv "GNOSIS_OPENAPI_INPUT"))
        (output (getenv "GNOSIS_OPENAPI_OUTPUT")))
    (unless (and input output
                 (not (string-empty-p input))
                 (not (string-empty-p output)))
      (gnosis-openapi--invalid "OpenAPI input and output are required"))
    (gnosis-openapi-generate input output)))

(provide 'gnosis-openapi-generator)
;;; gnosis-openapi-generator.el ends here
