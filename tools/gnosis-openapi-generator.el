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
         (security (if (eq specific missing) inherited specific)))
    (and (vectorp security) (> (length security) 0))))

(defun gnosis-openapi--operation (path method value inherited)
  "Return manifest entry for PATH, METHOD, and operation VALUE."
  (let ((identity (gnosis-openapi--required-string
                   value "operationId" "operationId"))
        (responses (and (hash-table-p value) (gethash "responses" value))))
    (unless (and (hash-table-p responses) (> (hash-table-count responses) 0))
      (gnosis-openapi--invalid (format "Operation %s has no responses" identity)))
    (list identity (upcase method) path
          (and (gnosis-openapi--authenticated-p value inherited) t))))

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
  "Return API version and operation manifest from DOCUMENT."
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
    (list version (gnosis-openapi--operations document))))

(defun gnosis-openapi--write (output version operations)
  "Write VERSION and OPERATIONS manifest to OUTPUT."
  (make-directory (file-name-directory (expand-file-name output)) t)
  (with-temp-file output
    (insert ";;; gnosis-openapi-contract.el --- Generated Gnosis API manifest  -*- lexical-binding: t; -*-\n\n"
            ";; Generated from gnosisd /v1/openapi.json.  Do not edit.\n\n"
            "(defconst gnosis-core--generated-openapi-version\n  ")
    (prin1 version (current-buffer))
    (insert ")\n\n(defconst gnosis-core--generated-openapi-operations\n  '")
    (prin1 operations (current-buffer))
    (insert ")\n\n(provide 'gnosis-openapi-contract)\n"
            ";;; gnosis-openapi-contract.el ends here\n")))

(defun gnosis-openapi-generate (input output)
  "Generate a temporary contract manifest from INPUT at OUTPUT."
  (pcase-let ((`(,version ,operations)
               (gnosis-openapi--contract (gnosis-openapi--read input))))
    (gnosis-openapi--write output version operations)
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
