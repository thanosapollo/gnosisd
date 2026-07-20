;;; gnosis-core.el --- Authenticated Gnosis HTTP client  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; URL: https://thanosapollo.org/projects/gnosis
;; Keywords: extensions

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Bounded, authenticated HTTP boundary to gnosisd.  The daemon remains the
;; only owner of Gnosis data, grading, scheduling, and durable review state.

;;; Code:

(require 'auth-source)
(require 'cl-lib)
(require 'json)
(require 'subr-x)
(require 'url)
(require 'url-http)
(require 'url-parse)
(require 'url-proxy)
(require 'url-util)

(defgroup gnosis-core nil
  "Authenticated HTTP transport for Gnosis."
  :group 'gnosis
  :prefix "gnosis-core-")

(defcustom gnosis-core-base-url "http://127.0.0.1:7842"
  "Base URL of the Gnosis HTTP service.
Plain HTTP is accepted only for loopback hosts.  Remote services must use
HTTPS.  User information, paths, queries, and fragments are rejected."
  :type 'string
  :group 'gnosis-core)

(defcustom gnosis-core-auth-user "gnosis"
  "User passed to `auth-source-search' for the Gnosis bearer token."
  :type 'string
  :group 'gnosis-core)

(defcustom gnosis-core-connect-timeout 5
  "Maximum seconds allowed for one HTTP request."
  :type 'number
  :group 'gnosis-core)

(defconst gnosis-core--response-limit (* 1024 1024)
  "Maximum accepted JSON response body size in bytes.")

(defconst gnosis-core--binary-response-limit (* 32 1024 1024)
  "Maximum accepted binary response body size in bytes.")

(defconst gnosis-core--response-header-limit (* 64 1024)
  "Maximum accepted HTTP response header size in bytes.")

(defconst gnosis-core--json-null (make-symbol "gnosis-json-null"))
(defconst gnosis-core--json-false (make-symbol "gnosis-json-false"))

(defconst gnosis-core--http-methods
  '("delete" "get" "head" "options" "patch" "post" "put" "trace"))

(defconst gnosis-core--reserved-request-headers
  '("accept" "accept-encoding" "authorization" "connection"
    "content-length" "content-type" "cookie" "expect" "host"
    "proxy-authorization" "proxy-connection" "te" "trailer"
    "transfer-encoding" "upgrade"))

(defvar gnosis-core--contract-cache nil
  "Cached live OpenAPI document and its validated service authority.")

(define-error 'gnosis-core-error "Gnosis core error")
(define-error 'gnosis-core-transport-error "Gnosis transport error" 'gnosis-core-error)
(define-error 'gnosis-core-timeout "Gnosis request timed out" 'gnosis-core-transport-error)
(define-error 'gnosis-core-authentication-error "Gnosis authentication failed" 'gnosis-core-error)
(define-error 'gnosis-core-authorization-error "Gnosis authorization failed" 'gnosis-core-error)
(define-error 'gnosis-core-not-found "Gnosis resource not found" 'gnosis-core-error)
(define-error 'gnosis-core-conflict "Gnosis state conflict" 'gnosis-core-error)
(define-error 'gnosis-core-validation-error "Gnosis request rejected" 'gnosis-core-error)
(define-error 'gnosis-core-unavailable "Gnosis service unavailable" 'gnosis-core-transport-error)
(define-error 'gnosis-core-protocol-error "Invalid Gnosis response" 'gnosis-core-error)
(define-error 'gnosis-core-mutation-uncertain
  "Gnosis mutation outcome is uncertain" 'gnosis-core-error)
(define-error 'gnosis-core-api-unavailable "Gnosis API bindings unavailable" 'gnosis-core-error)

(defun gnosis-core--invalid (message)
  "Signal a protocol error with public-safe MESSAGE."
  (signal 'gnosis-core-protocol-error (list message)))

(defun gnosis-core--loopback-host-p (host)
  "Return non-nil when HOST denotes the local machine."
  (member (downcase (or host ""))
          '("localhost" "127.0.0.1" "::1" "[::1]")))

(defun gnosis-core--validated-base-url ()
  "Return `gnosis-core-base-url' without a trailing slash.
Signal `gnosis-core-validation-error' when its authority is unsafe."
  (let* ((url (url-generic-parse-url gnosis-core-base-url))
         (scheme (url-type url))
         (host (url-host url))
         (path (url-filename url)))
    (unless (and host
                 (or (equal scheme "https")
                     (and (equal scheme "http")
                          (gnosis-core--loopback-host-p host)))
                 (string-empty-p (or (url-user url) ""))
                 (member path '(nil "" "/"))
                 (not (string-match-p "[?#]" gnosis-core-base-url)))
      (signal 'gnosis-core-validation-error '("Unsafe Gnosis base URL")))
    (string-remove-suffix "/" gnosis-core-base-url)))

(defun gnosis-core--valid-token-p (token)
  "Return non-nil when TOKEN satisfies gnosisd's bearer-token grammar."
  (and (stringp token)
       (<= 40 (string-bytes token) 128)
       (= (length token) (string-bytes token))
       (cl-every
        (lambda (character)
          (or (and (>= character ?A) (<= character ?Z))
              (and (>= character ?a) (<= character ?z))
              (and (>= character ?0) (<= character ?9))
              (memq character '(?- ?. ?_ ?~ ?+ ?/ ?=))))
        (string-to-list token))))

(defun gnosis-core--token ()
  "Return bearer token resolved through `auth-source'."
  (let* ((url (url-generic-parse-url (gnosis-core--validated-base-url)))
         (entry (car (auth-source-search
                      :host (url-host url) :port (url-port url)
                      :user gnosis-core-auth-user :max 1
                      :require '(:secret))))
         (secret (plist-get entry :secret))
         (token (if (functionp secret) (funcall secret) secret)))
    (unless (gnosis-core--valid-token-p token)
      (signal 'gnosis-core-authentication-error
              '("No valid Gnosis token in auth-source")))
    token))

(defun gnosis-core--json-body (value)
  "Return UTF-8 JSON bytes for VALUE."
  (encode-coding-string
   (json-serialize value :null-object gnosis-core--json-null
                   :false-object gnosis-core--json-false)
   'utf-8-unix))

(defun gnosis-core--decode-json (bytes)
  "Decode JSON BYTES to a plist with explicit null and false sentinels."
  (condition-case nil
      (json-parse-string
       (decode-coding-string bytes 'utf-8-unix)
       :object-type 'plist :array-type 'array
       :null-object gnosis-core--json-null
       :false-object gnosis-core--json-false)
    (error (gnosis-core--invalid "Invalid JSON response"))))

(defun gnosis-core--path-segment (value)
  "Return opaque VALUE encoded as one URL path segment."
  (unless (and (stringp value) (not (string-empty-p value)))
    (signal 'gnosis-core-validation-error '("Invalid Gnosis path value")))
  (url-hexify-string value))

(defun gnosis-core--query-value (value)
  "Return VALUE encoded for an HTTP query parameter."
  (url-hexify-string
   (cond ((stringp value) value)
         ((integerp value) (number-to-string value))
         ((eq value t) "true")
         ((eq value gnosis-core--json-false) "false")
         (t (signal 'gnosis-core-validation-error
                    '("Invalid Gnosis query value"))))))

(defun gnosis-core--query-string (pairs)
  "Return encoded HTTP query string from string-keyed PAIRS."
  (mapconcat
   (lambda (pair)
     (unless (and (consp pair) (stringp (car pair))
                  (not (string-empty-p (car pair))))
       (signal 'gnosis-core-validation-error '("Invalid Gnosis query name")))
     (concat (url-hexify-string (car pair)) "="
             (gnosis-core--query-value (cdr pair))))
   pairs "&"))

(defun gnosis-core--operation-id-p (value)
  "Return non-nil when VALUE is a valid operation identity."
  (and (stringp value) (<= 1 (length value) 128)
       (string-match-p "\\`[A-Za-z0-9][A-Za-z0-9._:-]*\\'" value)))

(defun gnosis-core-operation-id ()
  "Return a fresh operation identity suitable for a Gnosis mutation."
  (concat
   "emacs:"
   (substring
    (secure-hash
     'sha256
     (format "%S:%s:%s:%s" (current-time) (emacs-pid) (random) (user-uid)))
    0 32)))

(defun gnosis-core--if-match-header (revision)
  "Return an If-Match header for nonnegative integer REVISION."
  (unless (and (integerp revision) (>= revision 0))
    (signal 'gnosis-core-validation-error '("Invalid Gnosis revision")))
  (cons "If-Match" (format "\"%d\"" revision)))

(defun gnosis-core--idempotency-key-header (operation-id)
  "Return an Idempotency-Key header for OPERATION-ID."
  (unless (gnosis-core--operation-id-p operation-id)
    (signal 'gnosis-core-validation-error '("Invalid Gnosis operation identity")))
  (cons "Idempotency-Key" operation-id))

(defun gnosis-core--safe-request-headers (headers)
  "Return validated caller-supplied HEADERS."
  (unless (listp headers)
    (signal 'gnosis-core-validation-error '("Invalid Gnosis request headers")))
  (let ((names
         (mapcar
          (lambda (header)
            (unless (and (consp header) (stringp (car header)))
              (signal 'gnosis-core-validation-error
                      '("Invalid Gnosis request header")))
            (downcase (car header)))
          headers)))
    (unless (= (length names) (length (delete-dups (copy-sequence names))))
      (signal 'gnosis-core-validation-error '("Duplicate Gnosis request header")))
    (mapcar
     (lambda (header)
       (pcase-let ((`(,name . ,value) header))
         (unless (and (stringp name) (stringp value)
                      (string-match-p "\\`[A-Za-z0-9-]+\\'" name)
                      (<= 1 (length value) 512)
                      (not (string-match-p "[\r\n]" value))
                      (not (member (downcase name)
                                   gnosis-core--reserved-request-headers)))
           (signal 'gnosis-core-validation-error '("Unsafe Gnosis request header")))
         (cons name value)))
     headers)))

(defun gnosis-core--header-content-type ()
  "Return current response Content-Type header value."
  (save-excursion
    (goto-char (point-min))
    (let ((case-fold-search t))
      (when (re-search-forward
             "\\(?:\\`\\|[\r\n]\\)Content-Type:[ \t]*\\([^ \t\r\n]+\\)"
             (point-max) t)
        (downcase (match-string-no-properties 1))))))

(defun gnosis-core--response-body-start ()
  "Return the first body position in the current URL response buffer."
  (if (and (boundp 'url-http-end-of-headers)
           url-http-end-of-headers)
      (1+ (if (markerp url-http-end-of-headers)
              (marker-position url-http-end-of-headers)
            url-http-end-of-headers))
    (save-excursion
      (goto-char (point-min))
      (and (re-search-forward "\r?\n\r?\n" nil t) (point)))))

(defun gnosis-core--response-bytes (binary-p)
  "Return response body bytes, preserving raw bytes when BINARY-P."
  (let* ((start (gnosis-core--response-body-start))
         (bytes (and start (buffer-substring-no-properties start (point-max)))))
    (unless bytes (gnosis-core--invalid "Malformed HTTP response"))
    (if binary-p (string-to-unibyte bytes) bytes)))

(defun gnosis-core--signal-http-error (status body)
  "Signal a bounded typed error for HTTP STATUS and decoded BODY."
  (pcase-let ((`(,message ,code) (gnosis-core--api-error-view body)))
    (let ((type (cond ((= status 401) 'gnosis-core-authentication-error)
                      ((= status 403) 'gnosis-core-authorization-error)
                      ((= status 404) 'gnosis-core-not-found)
                      ((memq status '(409 412 428)) 'gnosis-core-conflict)
                      ((memq status '(400 413 422)) 'gnosis-core-validation-error)
                      ((or (= status 408) (= status 504)) 'gnosis-core-timeout)
                      ((>= status 500) 'gnosis-core-unavailable)
                      (t 'gnosis-core-protocol-error))))
      (signal type (list message code)))))

(defun gnosis-core--direct-proxy (&rest _)
  "Return a direct connection directive for URL proxy dispatch."
  "DIRECT")

(defun gnosis-core--locator-for-url (url)
  "Return proxy locator to use for URL."
  (if (gnosis-core--loopback-host-p
       (url-host (url-generic-parse-url url)))
      #'gnosis-core--direct-proxy
    url-proxy-locator))

(defun gnosis-core--retrieval-callback (status state)
  "Record retrieval STATUS and current buffer in STATE."
  (unless (aref state 0)
    (aset state 0 t)
    (aset state 1 status)
    (aset state 3 (current-buffer))))

(defun gnosis-core--abort-retrieval (process state condition message)
  "Abort PROCESS and record CONDITION with MESSAGE in STATE."
  (aset state 0 t)
  (aset state 2 (list condition message))
  (when (and (processp process) (process-live-p process))
    (delete-process process))
  (when-let* ((buffer (and (processp process) (process-buffer process)))
              ((buffer-live-p buffer)))
    (with-current-buffer buffer
      (let ((inhibit-read-only t))
        (erase-buffer)))))

(defun gnosis-core--cleanup-retrieval (buffer process)
  "Terminate PROCESS and kill BUFFER when they remain live."
  (dolist (owned-process
           (delete-dups
            (list process
                  (and (buffer-live-p buffer) (get-buffer-process buffer)))))
    (when (and (processp owned-process) (process-live-p owned-process))
      (delete-process owned-process)))
  (when (buffer-live-p buffer) (kill-buffer buffer)))

(defun gnosis-core--restore-bounded-filter (process state limit)
  "Restore bounded filtering on PROCESS using STATE and LIMIT."
  (let ((active (or (and (buffer-live-p (process-buffer process))
                         (get-buffer-process (process-buffer process)))
                    process)))
    (unless (eq (process-filter active) #'gnosis-core--bounded-process-filter)
      (process-put active 'gnosis-core--original-filter (process-filter active))
      (process-put active 'gnosis-core--state state)
      (process-put active 'gnosis-core--limit limit)
      (process-put active 'gnosis-core--headers-verified nil)
      (set-process-filter active #'gnosis-core--bounded-process-filter))
    active))

(defun gnosis-core--received-body-bytes ()
  "Return received response body size in current URL buffer."
  (let ((start (gnosis-core--response-body-start)))
    (unless start (gnosis-core--invalid "Malformed HTTP response"))
    (- (or (position-bytes (point-max)) (point-max))
       (or (position-bytes start) start))))

(defun gnosis-core--unsupported-content-encoding-p (header)
  "Return non-nil when HEADER declares an encoding other than identity."
  (let ((case-fold-search t)
        (position 0)
        (unsupported nil))
    (while (and (not unsupported)
                (string-match
                 "^Content-Encoding:[ \t]*\\([^\r\n]+\\)" header position))
      (setq unsupported
            (not (string-equal-ignore-case
                  (string-trim (match-string 1 header)) "identity"))
            position (match-end 0)))
    unsupported))

(defun gnosis-core--header-status (data)
  "Return status of partial HTTP headers followed by DATA.
Return nil while incomplete, `complete' when safe, `overflow' when too large,
or `encoding' when content decoding is unsupported."
  (let* ((received (buffer-substring-no-properties (point-min) (point-max)))
         (combined (concat received data))
         (end (string-match "\r?\n\r?\n" combined)))
    (if end
        (let ((header (substring combined 0 (match-end 0))))
          (cond ((> (string-bytes header) gnosis-core--response-header-limit)
                 'overflow)
                ((gnosis-core--unsupported-content-encoding-p header) 'encoding)
                (t 'complete)))
      (and (> (string-bytes combined) gnosis-core--response-header-limit)
           'overflow))))

(defun gnosis-core--bounded-process-filter (process data)
  "Pass DATA to URL PROCESS while enforcing configured response bounds."
  (let* ((state (process-get process 'gnosis-core--state))
         (limit (process-get process 'gnosis-core--limit))
         (original (process-get process 'gnosis-core--original-filter))
         (header-status
          (unless (process-get process 'gnosis-core--headers-verified)
            (gnosis-core--header-status data))))
    (unless (aref state 0)
      (condition-case nil
          (if (memq header-status '(overflow encoding))
              (gnosis-core--abort-retrieval
               process state 'gnosis-core-protocol-error
               (if (eq header-status 'encoding)
                   "Gnosis response content encoding is unsupported"
                 "Gnosis response exceeds size limit"))
            (when (eq header-status 'complete)
              (process-put process 'gnosis-core--headers-verified t))
            (funcall original process data)
            (setq process
                  (gnosis-core--restore-bounded-filter process state limit))
            (when (and (process-buffer process)
                       (buffer-live-p (process-buffer process)))
              (with-current-buffer (process-buffer process)
                (when (or (and (boundp 'url-http-content-length)
                               (integerp url-http-content-length)
                               (> url-http-content-length limit))
                          (and (bound-and-true-p url-http-end-of-headers)
                               (> (gnosis-core--received-body-bytes) limit)))
                  (gnosis-core--abort-retrieval
                   process state 'gnosis-core-protocol-error
                   "Gnosis response exceeds size limit")))))
        (error
         (gnosis-core--abort-retrieval
          process state 'gnosis-core-unavailable
          "Gnosis transport failed while receiving response"))))))

(defun gnosis-core--retrieve-bounded (url limit)
  "Retrieve URL synchronously while bounding its body to LIMIT bytes."
  (let* ((state (vector nil nil nil nil))
         (locator (gnosis-core--locator-for-url url))
         (deadline (+ (float-time) gnosis-core-connect-timeout))
         (buffer nil)
         (process nil)
         (transferred nil))
    (unwind-protect
        (progn
          (condition-case condition
              (let ((url-proxy-locator locator))
                (setq buffer
                      (url-retrieve url #'gnosis-core--retrieval-callback
                                    (list state) t t)))
            (gnosis-core-error (signal (car condition) (cdr condition)))
            (quit (signal 'quit nil))
            (error
             (signal 'gnosis-core-unavailable '("Gnosis transport failed"))))
          (unless (buffer-live-p buffer)
            (signal 'gnosis-core-unavailable '("Gnosis transport did not start")))
          (with-current-buffer buffer
            (setq-local url-proxy-locator locator)
            (setq process (get-buffer-process buffer)))
          (unless (processp process)
            (signal 'gnosis-core-unavailable '("Gnosis transport has no process")))
          (process-put process 'gnosis-core--state state)
          (process-put process 'gnosis-core--limit limit)
          (process-put process 'gnosis-core--headers-verified nil)
          (process-put process 'gnosis-core--original-filter
                       (process-filter process))
          (set-process-filter process #'gnosis-core--bounded-process-filter)
          (while (and (not (aref state 0)) (< (float-time) deadline))
            (setq process (or (get-buffer-process buffer) process))
            (accept-process-output
             process (max 0 (min 0.05 (- deadline (float-time))))))
          (unless (aref state 0)
            (gnosis-core--abort-retrieval
             process state 'gnosis-core-timeout "Gnosis request timed out"))
          (when-let* ((failure (aref state 2)))
            (signal (car failure) (cdr failure)))
          (let ((status (aref state 1)))
            (when (and (plist-member status :error)
                       (with-current-buffer buffer
                         (not (integerp
                               (and (boundp 'url-http-response-status)
                                    url-http-response-status)))))
              (signal 'gnosis-core-unavailable '("Gnosis transport failed"))))
          (setq transferred t)
          buffer)
      (unless transferred
        (gnosis-core--cleanup-retrieval buffer process)))))

(defun gnosis-core--perform-request
    (method path body expected-status binary-p auth-p &optional headers)
  "Perform one METHOD request to PATH with BODY.
EXPECTED-STATUS is the accepted status.  Return binary or decoded JSON as
selected by BINARY-P.  Add authentication when AUTH-P is non-nil and append
validated caller-supplied HEADERS."
  (let* ((url (concat (gnosis-core--validated-base-url) path))
         (locator (gnosis-core--locator-for-url url))
         (headers (gnosis-core--safe-request-headers headers)))
    (let ((url-proxy-locator locator)
          (url-debug (and (not auth-p) url-debug)))
      (let* ((url-request-method method)
             (url-request-data (and body (gnosis-core--json-body body)))
             (url-request-extra-headers
              (append
               (and auth-p
                    `(("Authorization" .
                       ,(concat "Bearer " (gnosis-core--token)))))
               headers
               (and body '(("Content-Type" . "application/json")))
               `(("Accept" . ,(if binary-p "*/*" "application/json"))
                 ("Accept-Encoding" . "identity"))))
             (url-max-redirections 0)
             (url-http-attempt-keepalives nil)
             (limit (if binary-p gnosis-core--binary-response-limit
                      gnosis-core--response-limit))
             (buffer (gnosis-core--retrieve-bounded url limit)))
        (unwind-protect
            (with-current-buffer buffer
              (let* ((status
                      (or (and (boundp 'url-http-response-status)
                               url-http-response-status)
                          0))
                     (bytes (gnosis-core--response-bytes binary-p))
                     (content-type (gnosis-core--header-content-type)))
                (when (> (string-bytes bytes) limit)
                  (gnosis-core--invalid "Gnosis response exceeds size limit"))
                (when (memq status '(301 302 303 307 308))
                  (gnosis-core--invalid "Gnosis redirect refused"))
                (cond
                 ((/= status expected-status)
                  (unless (gnosis-core--json-content-type-p content-type)
                    (gnosis-core--invalid "Unexpected Gnosis content type"))
                  (gnosis-core--signal-http-error
                   status (gnosis-core--decode-json bytes)))
                 (binary-p bytes)
                 (t
                  (unless (gnosis-core--json-content-type-p content-type)
                    (gnosis-core--invalid "Unexpected Gnosis content type"))
                  (gnosis-core--decode-json bytes)))))
          (gnosis-core--cleanup-retrieval
           buffer (and (buffer-live-p buffer) (get-buffer-process buffer))))))))

(defun gnosis-core--request
    (method path &optional body expected-status binary-p retry-p auth-p headers)
  "Request METHOD PATH with optional BODY and return the decoded response.
EXPECTED-STATUS defaults to 200.  BINARY-P preserves bytes.  RETRY-P retries
one transport failure using the exact request.  AUTH-P defaults to non-nil;
`:none' disables authentication.  HEADERS are validated caller headers."
  (let ((attempts (if (and retry-p (equal method "GET")) 2 1))
        (authenticate (not (eq auth-p :none))))
    (catch 'result
      (dotimes (attempt attempts)
        (condition-case condition
            (throw 'result
                   (gnosis-core--perform-request
                    method path body (or expected-status 200)
                    binary-p authenticate headers))
          ((gnosis-core-unavailable gnosis-core-timeout)
           (when (= attempt (1- attempts)) (signal (car condition) (cdr condition)))))))))

(defun gnosis-core--json-content-type-p (value)
  "Return non-nil when VALUE is a supported JSON content type."
  (and value
       (string-match-p
        "\\`application/\\(?:problem\\+\\)?json\\(?:;\\|\\'\\)" value)))

(defun gnosis-core--api-error-view (body)
  "Return bounded message and code from generic error BODY."
  (let* ((view (and (proper-list-p body) (plist-get body :error)))
         (code (and (proper-list-p view) (plist-get view :code)))
         (message (and (proper-list-p view) (plist-get view :message))))
    (unless (and (stringp code) (<= 1 (length code) 64)
                 (string-match-p "\\`[a-z][a-z0-9_]*\\'" code)
                 (stringp message) (<= 1 (length message) 512))
      (gnosis-core--invalid "Invalid Gnosis error response"))
    (list message code)))

(defun gnosis-core--openapi-p (value)
  "Return non-nil when VALUE is a minimal OpenAPI 3.1 document."
  (let* ((info (and (proper-list-p value) (plist-get value :info)))
         (title (and (proper-list-p info) (plist-get info :title)))
         (version (and (proper-list-p info) (plist-get info :version)))
         (paths (and (proper-list-p value) (plist-get value :paths))))
    (and (equal "3.1.0" (plist-get value :openapi))
         (proper-list-p info)
         (equal "Gnosis API" title)
         (stringp version)
         (not (string-empty-p version))
         (proper-list-p paths)
         paths
         (cl-loop for (key item) on paths by #'cddr
                  always (and (keywordp key)
                              (string-prefix-p
                               "/" (string-remove-prefix
                                    ":" (symbol-name key)))
                              (proper-list-p item))))))

(defun gnosis-core-refresh-contract ()
  "Fetch and cache the live OpenAPI contract for the current authority."
  (let ((base (gnosis-core--validated-base-url))
        (document (gnosis-core-openapi)))
    (setq gnosis-core--contract-cache
          (list :base base :document document))
    document))

(defun gnosis-core--contract ()
  "Return the live OpenAPI contract for the current authority."
  (let ((base (gnosis-core--validated-base-url)))
    (if (and (equal base (plist-get gnosis-core--contract-cache :base))
             (plist-get gnosis-core--contract-cache :document))
        (plist-get gnosis-core--contract-cache :document)
      (gnosis-core-refresh-contract))))

(defun gnosis-core--json-key (name)
  "Return plist keyword for JSON object key NAME."
  (intern (concat ":" name)))

(defun gnosis-core--json-object-p (value)
  "Return non-nil when VALUE has decoded JSON object shape."
  (and (proper-list-p value)
       (cl-evenp (length value))
       (let ((keys (cl-loop for (key _item) on value by #'cddr
                            collect key)))
         (and (cl-every #'keywordp keys)
              (= (length keys)
                 (length (delete-dups (copy-sequence keys))))))))

(defun gnosis-core--schema-ref (contract reference)
  "Resolve schema REFERENCE in CONTRACT or return nil."
  (let ((prefix "#/components/schemas/"))
    (and (stringp reference)
         (string-prefix-p prefix reference)
         (plist-get
          (plist-get (plist-get contract :components) :schemas)
          (gnosis-core--json-key (substring reference (length prefix)))))))

(defun gnosis-core--schema-type-valid-p (value type)
  "Return non-nil when VALUE has JSON schema TYPE."
  (pcase type
    ('nil t)
    ("array" (vectorp value))
    ("boolean" (memq value (list t gnosis-core--json-false)))
    ("integer" (integerp value))
    ("null" (eq value gnosis-core--json-null))
    ("number" (numberp value))
    ("object" (gnosis-core--json-object-p value))
    ("string" (stringp value))
    (_ nil)))

(defun gnosis-core--schema-bounds-valid-p (value schema)
  "Return non-nil when VALUE respects simple bounds in SCHEMA."
  (and
   (or (not (stringp value))
       (and (or (not (plist-member schema :minLength))
                (<= (plist-get schema :minLength) (length value)))
            (or (not (plist-member schema :maxLength))
                (<= (length value) (plist-get schema :maxLength)))))
   (or (not (numberp value))
       (and (or (not (plist-member schema :minimum))
                (<= (plist-get schema :minimum) value))
            (or (not (plist-member schema :maximum))
                (<= value (plist-get schema :maximum)))))))

(defun gnosis-core--schema-array-valid-p (value schema contract depth)
  "Return non-nil when array VALUE satisfies SCHEMA in CONTRACT at DEPTH."
  (let ((items (plist-get schema :items))
        (length (length value)))
    (and (or (not (plist-member schema :minItems))
             (<= (plist-get schema :minItems) length))
         (or (not (plist-member schema :maxItems))
             (<= length (plist-get schema :maxItems)))
         (or (not items)
             (cl-loop for item across value
                      always (gnosis-core--schema-valid-p
                              item items contract (1+ depth)))))))

(defun gnosis-core--schema-property-valid-p
    (key value properties additional contract depth)
  "Return non-nil when object KEY and VALUE satisfy PROPERTIES.
ADDITIONAL defines unknown-key handling using CONTRACT at DEPTH."
  (let* ((name (string-remove-prefix ":" (symbol-name key)))
         (keyword (gnosis-core--json-key name)))
    (cond ((plist-member properties keyword)
           (gnosis-core--schema-valid-p
            value (plist-get properties keyword) contract (1+ depth)))
          ((eq additional gnosis-core--json-false) nil)
          ((gnosis-core--json-object-p additional)
           (gnosis-core--schema-valid-p value additional contract (1+ depth)))
          (t t))))

(defun gnosis-core--schema-object-valid-p (value schema contract depth)
  "Return non-nil when object VALUE satisfies SCHEMA in CONTRACT at DEPTH."
  (let ((required (plist-get schema :required))
        (properties (plist-get schema :properties))
        (additional (plist-get schema :additionalProperties)))
    (and
     (or (not required)
         (cl-loop for name across required
                  always (plist-member value (gnosis-core--json-key name))))
     (cl-loop for (key item) on value by #'cddr
              always (gnosis-core--schema-property-valid-p
                      key item properties additional contract depth)))))

(defun gnosis-core--schema-enum-valid-p (value schema)
  "Return non-nil when VALUE satisfies const and enum rules in SCHEMA."
  (and (or (not (plist-member schema :const))
           (equal value (plist-get schema :const)))
       (or (not (plist-member schema :enum))
           (cl-find value (plist-get schema :enum) :test #'equal))))

(defun gnosis-core--schema-composition-valid-p (value schemas mode contract depth)
  "Validate VALUE against SCHEMAS using MODE and CONTRACT at DEPTH."
  (let ((matches
         (cl-loop for schema across schemas
                  count (gnosis-core--schema-valid-p
                         value schema contract (1+ depth)))))
    (pcase mode
      ('one (= matches 1))
      ('any (> matches 0))
      ('all (= matches (length schemas))))))

(defun gnosis-core--schema-base-valid-p (value schema contract depth)
  "Validate VALUE against non-composition SCHEMA rules in CONTRACT at DEPTH."
  (let ((type (plist-get schema :type)))
    (and (if (vectorp type)
             (cl-loop for item across type
                      thereis (gnosis-core--schema-type-valid-p value item))
           (gnosis-core--schema-type-valid-p value type))
         (gnosis-core--schema-enum-valid-p value schema)
         (gnosis-core--schema-bounds-valid-p value schema)
         (or (not (vectorp value))
             (gnosis-core--schema-array-valid-p value schema contract depth))
         (or (not (gnosis-core--json-object-p value))
             (gnosis-core--schema-object-valid-p value schema contract depth)))))

(defun gnosis-core--schema-valid-p (value schema contract &optional depth)
  "Return non-nil when VALUE satisfies SCHEMA from CONTRACT at DEPTH."
  (let ((depth (or depth 0)))
    (condition-case nil
        (and (< depth 64)
             (gnosis-core--schema-base-valid-p value schema contract depth)
             (or (not (plist-get schema :$ref))
                 (when-let* ((resolved (gnosis-core--schema-ref
                                        contract (plist-get schema :$ref))))
                   (gnosis-core--schema-valid-p
                    value resolved contract (1+ depth))))
             (or (not (plist-get schema :oneOf))
                 (gnosis-core--schema-composition-valid-p
                  value (plist-get schema :oneOf) 'one contract depth))
             (or (not (plist-get schema :anyOf))
                 (gnosis-core--schema-composition-valid-p
                  value (plist-get schema :anyOf) 'any contract depth))
             (or (not (plist-get schema :allOf))
                 (gnosis-core--schema-composition-valid-p
                  value (plist-get schema :allOf) 'all contract depth))
             (or (not (plist-member schema :not))
                 (not (gnosis-core--schema-valid-p
                       value (plist-get schema :not) contract (1+ depth)))))
      (error nil))))

(defun gnosis-core--operation-entry (identity)
  "Return live contract entry for operation IDENTITY."
  (let* ((contract (gnosis-core--contract))
         (paths (plist-get contract :paths))
         (matches
          (cl-loop for (path-key item) on paths by #'cddr
                   for path = (string-remove-prefix ":" (symbol-name path-key))
                   append
                   (cl-loop for method in gnosis-core--http-methods
                            for operation = (plist-get item
                                                       (gnosis-core--json-key method))
                            when (equal identity (plist-get operation :operationId))
                            collect (list :method (upcase method) :path path
                                          :operation operation)))))
    (unless (= (length matches) 1)
      (gnosis-core--invalid "Gnosis operation is absent or ambiguous"))
    (car matches)))

(defun gnosis-core--bearer-requirement-p (requirement)
  "Return non-nil when REQUIREMENT names only bearer authentication."
  (and (gnosis-core--json-object-p requirement)
       (= (length requirement) 2)
       (vectorp (plist-get requirement :bearerAuth))
       (= (length (plist-get requirement :bearerAuth)) 0)))

(defun gnosis-core--operation-bearer-p (operation)
  "Return non-nil when OPERATION requires bearer authentication."
  (let ((security (plist-get operation :security)))
    (and (vectorp security) (> (length security) 0)
         (cl-loop for requirement across security
                  thereis (gnosis-core--bearer-requirement-p requirement)))))

(defun gnosis-core--operation-scopes-p (operation)
  "Return non-nil when OPERATION declares nonempty string scopes."
  (let ((scopes (plist-get operation :x-gnosis-required-scopes)))
    (and (vectorp scopes) (> (length scopes) 0)
         (cl-loop for scope across scopes
                  always (and (stringp scope) (not (string-empty-p scope)))))))

(defun gnosis-core--parameter-names (operation location &optional required)
  "Return OPERATION parameter names at LOCATION.
When REQUIRED is non-nil, return only required parameters."
  (let ((parameters (or (plist-get operation :parameters) [])))
    (unless (vectorp parameters)
      (gnosis-core--invalid "Invalid Gnosis operation parameters"))
    (cl-loop for parameter across parameters
           when (and (equal location (plist-get parameter :in))
                     (or (not required) (eq t (plist-get parameter :required))))
             collect (plist-get parameter :name))))

(defun gnosis-core--parameter (operation location name)
  "Return OPERATION parameter at LOCATION named NAME."
  (let ((fold-case (equal location "header")))
    (cl-find-if
     (lambda (parameter)
       (and (equal location (plist-get parameter :in))
            (if fold-case
                (string-equal-ignore-case name (plist-get parameter :name))
              (equal name (plist-get parameter :name)))))
     (append (or (plist-get operation :parameters) []) nil))))

(defun gnosis-core--validate-parameter-values
    (pairs operation location contract)
  "Validate PAIRS against OPERATION parameters at LOCATION in CONTRACT."
  (dolist (pair pairs)
    (unless (and (consp pair) (stringp (car pair)))
      (gnosis-core--invalid "Invalid Gnosis parameter bindings"))
    (let* ((parameter (gnosis-core--parameter operation location (car pair)))
           (schema (and parameter (plist-get parameter :schema))))
      (unless (and schema
                   (gnosis-core--schema-valid-p
                    (cdr pair) schema contract))
        (gnosis-core--invalid "Invalid Gnosis parameter value")))))

(defun gnosis-core--validate-binding-names
    (provided allowed required label &optional fold-case)
  "Validate PROVIDED names against ALLOWED and REQUIRED for LABEL.
Compare names without case distinctions when FOLD-CASE is non-nil."
  (unless (cl-every #'stringp (append provided allowed required))
    (gnosis-core--invalid (format "Invalid Gnosis %s bindings" label)))
  (let* ((normalize (if fold-case #'downcase #'identity))
         (provided (mapcar normalize provided))
         (allowed (mapcar normalize allowed))
         (required (mapcar normalize required)))
    (unless (and (= (length provided)
                    (length (delete-dups (copy-sequence provided))))
                 (cl-every (lambda (name) (member name allowed)) provided)
                 (cl-every (lambda (name) (member name provided)) required))
      (gnosis-core--invalid (format "Invalid Gnosis %s bindings" label)))))

(defun gnosis-core--validate-operation-bindings
    (operation template path-values query headers contract)
  "Validate OPERATION bindings against TEMPLATE and CONTRACT.
PATH-VALUES, QUERY, and HEADERS contain the supplied bindings."
  (let ((path (mapcar #'car path-values))
        (declared-path (gnosis-core--parameter-names operation "path"))
        (query-names (mapcar #'car query))
        (header-names (mapcar #'car headers)))
    (unless (equal (sort (copy-sequence declared-path) #'string<)
                   (sort (gnosis-core--path-parameter-names template) #'string<))
      (gnosis-core--invalid "Invalid Gnosis path template"))
    (gnosis-core--validate-binding-names
     path declared-path
     (gnosis-core--parameter-names operation "path" t) "path")
    (gnosis-core--validate-binding-names
     query-names (gnosis-core--parameter-names operation "query")
     (gnosis-core--parameter-names operation "query" t) "query")
    (gnosis-core--validate-binding-names
     header-names (gnosis-core--parameter-names operation "header")
     (gnosis-core--parameter-names operation "header" t) "header" t)
    (gnosis-core--validate-parameter-values
     path-values operation "path" contract)
    (gnosis-core--validate-parameter-values query operation "query" contract)
    (gnosis-core--validate-parameter-values
     headers operation "header" contract)))

(defun gnosis-core--path-parameter-names (template)
  "Return parameter names present in OpenAPI path TEMPLATE."
  (let ((position 0)
        names)
    (while (string-match "{\\([^}]+\\)}" template position)
      (push (match-string 1 template) names)
      (setq position (match-end 0)))
    (nreverse names)))

(defun gnosis-core--operation-path (template path-values query)
  "Fill TEMPLATE from PATH-VALUES and append encoded QUERY."
  (let ((path
         (cl-reduce
          (lambda (path pair)
            (replace-regexp-in-string
             (regexp-quote (format "{%s}" (car pair)))
             (gnosis-core--path-segment (cdr pair)) path t t))
          path-values :initial-value template)))
    (when (string-match-p "{[^}]+}" path)
      (gnosis-core--invalid "Incomplete Gnosis path bindings"))
    (if query
        (concat path "?" (gnosis-core--query-string query))
      path)))

(defun gnosis-core--operation-json-schema (container)
  "Return application/json schema nested in OpenAPI CONTAINER."
  (plist-get (plist-get (plist-get container :content) :application/json)
             :schema))

(defun gnosis-core--schema-properties (schema contract)
  "Return object properties from SCHEMA after resolving it in CONTRACT."
  (let ((resolved
         (if-let* ((reference (plist-get schema :$ref)))
             (gnosis-core--schema-ref contract reference)
           schema)))
    (and resolved (plist-get resolved :properties))))

(defun gnosis-core--schema-shape-valid-p (value schema contract &optional depth)
  "Check VALUE shape against SCHEMA in CONTRACT at optional DEPTH."
  (let ((depth (or depth 0)))
    (and
     (< depth 64)
     (cond
      ((plist-get schema :$ref)
       (when-let* ((resolved
                    (gnosis-core--schema-ref contract (plist-get schema :$ref))))
         (gnosis-core--schema-shape-valid-p
          value resolved contract (1+ depth))))
      ((or (plist-get schema :oneOf) (plist-get schema :anyOf))
       (cl-loop for branch across (or (plist-get schema :oneOf)
                                      (plist-get schema :anyOf))
                thereis (gnosis-core--schema-shape-valid-p
                         value branch contract (1+ depth))))
      ((plist-get schema :allOf)
       (cl-loop for branch across (plist-get schema :allOf)
                always (gnosis-core--schema-shape-valid-p
                        value branch contract (1+ depth))))
      (t
       (let ((type (plist-get schema :type)))
         (if (vectorp type)
             (cl-loop for branch across type
                      thereis (gnosis-core--schema-type-valid-p value branch))
           (gnosis-core--schema-type-valid-p value type))))))))

(defun gnosis-core--shared-response-fields-valid-p
    (body response-schema contract)
  "Check BODY against matching RESPONSE-SCHEMA fields using CONTRACT."
  (let ((properties
         (and (gnosis-core--json-object-p body)
              (gnosis-core--schema-properties response-schema contract))))
    (or (not (gnosis-core--json-object-p body))
        (not properties)
        (cl-loop for (key value) on body by #'cddr
                 for schema = (and (plist-member properties key)
                                   (plist-get properties key))
                 always (or (not schema)
                            (gnosis-core--schema-shape-valid-p
                             value schema contract))))))

(defun gnosis-core--operation-success (operation)
  "Return the sole success status and response schema for OPERATION."
  (let ((matches
         (cl-loop for (key response) on (plist-get operation :responses) by #'cddr
                  for status = (string-remove-prefix ":" (symbol-name key))
                  when (string-match-p "\\`2[0-9][0-9]\\'" status)
                  collect (cons (string-to-number status)
                                (gnosis-core--operation-json-schema response)))))
    (unless (= (length matches) 1)
      (gnosis-core--invalid "Gnosis operation has ambiguous success responses"))
    (car matches)))

(defun gnosis-core--typed-request
    (identity &optional path-values query body headers)
  "Execute live-contract operation IDENTITY.
PATH-VALUES fill declared path parameters.  QUERY, BODY, and HEADERS supply
the remaining declared request inputs."
  (let* ((contract (gnosis-core--contract))
         (entry (gnosis-core--operation-entry identity))
         (operation (plist-get entry :operation))
         (method (plist-get entry :method))
         (template (plist-get entry :path))
         (request-body (plist-get operation :requestBody))
         (request-schema (gnosis-core--operation-json-schema request-body))
         (success (gnosis-core--operation-success operation))
         (headers (gnosis-core--safe-request-headers headers)))
    (unless (and (string-prefix-p "/v1/" template)
                 (gnosis-core--operation-bearer-p operation)
                 (gnosis-core--operation-scopes-p operation))
      (gnosis-core--invalid "Gnosis operation violates client doctrine"))
    (gnosis-core--validate-operation-bindings
     operation template path-values query headers contract)
    (when (and (eq t (plist-get request-body :required)) (not body))
      (gnosis-core--invalid "Gnosis operation requires a request body"))
    (when (and body
               (not (and request-schema
                         (gnosis-core--schema-valid-p body request-schema contract))))
      (gnosis-core--invalid "Invalid Gnosis request body"))
    (unless (gnosis-core--shared-response-fields-valid-p
             body (cdr success) contract)
      (gnosis-core--invalid
       "Gnosis request cannot satisfy its response contract"))
    (let ((value
           (condition-case condition
               (gnosis-core--request
                method
                (gnosis-core--operation-path template path-values query)
                body (car success) nil (equal method "GET") t headers)
             ((gnosis-core-transport-error gnosis-core-protocol-error)
              (if (equal method "GET")
                  (signal (car condition) (cdr condition))
                (signal 'gnosis-core-mutation-uncertain
                        '("Gnosis mutation outcome is uncertain")))))))
      (unless (and (cdr success)
                   (gnosis-core--schema-valid-p value (cdr success) contract))
        (if (equal method "GET")
            (gnosis-core--invalid "Invalid Gnosis operation response")
          (signal 'gnosis-core-mutation-uncertain
                  '("Gnosis mutation response is invalid"))))
      value)))

(defun gnosis-core--typed-binary-request (identity &optional path-values query)
  "Execute binary GET operation IDENTITY from the live contract.
PATH-VALUES fill path parameters and QUERY supplies query parameters."
  (let* ((contract (gnosis-core--contract))
         (entry (gnosis-core--operation-entry identity))
         (operation (plist-get entry :operation))
         (method (plist-get entry :method))
         (template (plist-get entry :path))
         (success (gnosis-core--operation-success operation)))
    (unless (and (equal method "GET")
                 (string-prefix-p "/v1/" template)
                 (gnosis-core--operation-bearer-p operation)
                 (gnosis-core--operation-scopes-p operation)
                 (null (cdr success)))
      (gnosis-core--invalid "Gnosis binary operation violates client doctrine"))
    (gnosis-core--validate-operation-bindings
     operation template path-values query nil contract)
    (gnosis-core--request
     method (gnosis-core--operation-path template path-values query)
     nil (car success) t t t)))

(defun gnosis-core-sync-identity ()
  "Return authenticated replica device identity."
  (gnosis-core--typed-request "get_sync_identity"))

(defun gnosis-core-sync-state ()
  "Return authenticated replica state."
  (gnosis-core--typed-request "get_sync_state"))

(defun gnosis-core-sync-snapshot (&optional cursor)
  "Return replica snapshot page after opaque CURSOR."
  (if cursor
      (gnosis-core--typed-request
       "get_sync_snapshot" nil (list (cons "cursor" cursor)))
    (gnosis-core--typed-request "get_sync_snapshot")))

(defun gnosis-core-sync-changes (cursor)
  "Return replica change page after opaque CURSOR."
  (gnosis-core--typed-request
   "get_sync_changes" nil (list (cons "cursor" cursor))))

(defun gnosis-core-blob (digest)
  "Return immutable blob bytes matching hexadecimal SHA-256 DIGEST."
  (unless (and (stringp digest)
               (string-match-p "\\`[0-9a-f]\\{64\\}\\'" digest))
    (signal 'gnosis-core-validation-error '("Invalid Gnosis blob digest")))
  (let ((bytes
         (gnosis-core--typed-binary-request
          "get_blobs__digest_" (list (cons "digest" digest)))))
    (unless (equal digest (secure-hash 'sha256 bytes))
      (gnosis-core--invalid "Gnosis blob digest mismatch"))
    bytes))

(defun gnosis-core--org-string (bytes)
  "Return strict UTF-8 Org text decoded from unibyte BYTES."
  (unless (stringp bytes)
    (signal 'gnosis-core-validation-error '("Invalid Gnosis Org bytes")))
  (let* ((raw (string-to-unibyte bytes))
         (text (decode-coding-string raw 'utf-8)))
    (unless (equal raw (encode-coding-string text 'utf-8 t))
      (signal 'gnosis-core-validation-error '("Invalid Gnosis UTF-8 content")))
    text))

(defun gnosis-core--document-mutation-result
    (result operation-id &optional document-id revision logical-path digest active)
  "Validate mutation RESULT against request expectations.
OPERATION-ID is mandatory.  DOCUMENT-ID, REVISION, LOGICAL-PATH, DIGEST, and
ACTIVE constrain an applied authoritative head when non-nil."
  (unless (and (= (or (plist-get result :format_version) -1) 1)
               (equal operation-id (plist-get result :operation_id))
               (stringp (plist-get result :cursor)))
    (signal 'gnosis-core-mutation-uncertain
            '("Gnosis mutation response correlation failed")))
  (when (equal "applied" (plist-get result :status))
    (let ((head (plist-get result :authoritative)))
      (unless (and head
                   (or (null document-id)
                       (equal document-id (plist-get head :document_id)))
                   (or (null revision)
                       (= (1+ revision) (plist-get head :revision)))
                   (or (null logical-path)
                       (equal logical-path (plist-get head :logical_path)))
                   (or (null digest)
                       (equal digest (plist-get head :content_sha256)))
                   (or (null active)
                       (eq (not (eq active :inactive))
                           (not (null (plist-get head :active))))))
        (signal 'gnosis-core-mutation-uncertain
                '("Gnosis mutation authoritative head is invalid")))))
  result)

(defun gnosis-core--document-mutation-headers (operation-id &optional revision)
  "Return mutation headers for OPERATION-ID and optional REVISION."
  (delq nil
        (list (gnosis-core--idempotency-key-header operation-id)
              (and revision (gnosis-core--if-match-header revision)))))

(defun gnosis-core-document-create (operation-id logical-path bytes)
  "Create LOGICAL-PATH from exact Org BYTES under OPERATION-ID."
  (let* ((text (gnosis-core--org-string bytes))
         (digest (secure-hash 'sha256 (string-to-unibyte bytes)))
         (result
          (gnosis-core--typed-request
           "post_document-mutations" nil nil
           (list :logical_path logical-path :org text)
           (gnosis-core--document-mutation-headers operation-id))))
    (gnosis-core--document-mutation-result
     result operation-id nil nil logical-path digest t)))

(defun gnosis-core-document-replace
    (document-id operation-id revision logical-path bytes)
  "Replace DOCUMENT-ID at REVISION with LOGICAL-PATH and exact Org BYTES.
Use OPERATION-ID as stable mutation identity."
  (let* ((text (gnosis-core--org-string bytes))
         (digest (secure-hash 'sha256 (string-to-unibyte bytes)))
         (result
          (gnosis-core--typed-request
           "put_document-mutations__id_" (list (cons "id" document-id)) nil
           (list :logical_path logical-path :org text)
           (gnosis-core--document-mutation-headers operation-id revision))))
    (gnosis-core--document-mutation-result
     result operation-id document-id revision logical-path digest t)))

(defun gnosis-core-document-rename
    (document-id operation-id revision logical-path)
  "Rename DOCUMENT-ID at REVISION to LOGICAL-PATH.
Use OPERATION-ID as stable mutation identity."
  (gnosis-core--document-mutation-result
   (gnosis-core--typed-request
    "post_document-mutations__id__rename" (list (cons "id" document-id)) nil
    (list :logical_path logical-path)
    (gnosis-core--document-mutation-headers operation-id revision))
   operation-id document-id revision logical-path nil t))

(defun gnosis-core-document-tombstone (document-id operation-id revision)
  "Tombstone DOCUMENT-ID at REVISION under OPERATION-ID."
  (gnosis-core--document-mutation-result
   (gnosis-core--typed-request
    "delete_document-mutations__id_" (list (cons "id" document-id)) nil nil
    (gnosis-core--document-mutation-headers operation-id revision))
   operation-id document-id revision nil nil :inactive))

(defun gnosis-core-dashboard (&optional days)
  "Return the server dashboard projection for optional window DAYS."
  (if days
      (gnosis-core--typed-request
       "get_dashboard" nil (list (cons "days" days)))
    (gnosis-core--typed-request "get_dashboard")))

(defun gnosis-core-nodes (&optional query after limit)
  "Return a server node page filtered by optional QUERY, AFTER, and LIMIT."
  (let ((parameters
         (delq nil (list (and after (cons "after" after))
                         (and limit (cons "limit" limit))
                         (and query (cons "q" query))))))
    (if parameters
        (gnosis-core--typed-request "get_nodes" nil parameters)
      (gnosis-core--typed-request "get_nodes"))))

(defun gnosis-core-thema (thema-id)
  "Return the server projection for opaque THEMA-ID."
  (let ((projection
         (gnosis-core--typed-request
          "get_themata__id_" (list (cons "id" thema-id)))))
    (unless (equal thema-id (plist-get projection :thema_id))
      (gnosis-core--invalid "Gnosis thema identity mismatch"))
    projection))

(defun gnosis-core--operation-request-view (identity projection)
  "Project IDENTITY's request fields from server PROJECTION."
  (let* ((contract (gnosis-core--contract))
         (operation (plist-get (gnosis-core--operation-entry identity) :operation))
         (request-schema
          (gnosis-core--operation-json-schema
           (plist-get operation :requestBody)))
         (schema (if-let* ((reference (plist-get request-schema :$ref)))
                     (gnosis-core--schema-ref contract reference)
                   request-schema))
         (properties (and schema (plist-get schema :properties)))
         (view
          (cl-loop for (key _property) on properties by #'cddr
                   when (plist-member projection key)
                   append (list key (plist-get projection key)))))
    (unless (and properties
                 (gnosis-core--schema-valid-p view schema contract))
      (gnosis-core--invalid "Invalid Gnosis write projection"))
    view))

(defun gnosis-core-thema-write-view (projection)
  "Return thema write fields derived from server PROJECTION."
  (gnosis-core--operation-request-view "put_themata__id_" projection))

(defun gnosis-core-thema-create (request)
  "Create and return a thema from REQUEST without automatic retry."
  (gnosis-core--typed-request "post_themata" nil nil request))

(defun gnosis-core-thema-update (thema-id revision request)
  "Update opaque THEMA-ID at REVISION from REQUEST without automatic retry."
  (let ((projection
         (gnosis-core--typed-request
          "put_themata__id_" (list (cons "id" thema-id)) nil request
          (list (gnosis-core--if-match-header revision)))))
    (unless (equal thema-id (plist-get projection :thema_id))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis mutation response identity is invalid")))
    projection))

(defun gnosis-core-api-unavailable (operation)
  "Signal that generated bindings for OPERATION are not available."
  (signal 'gnosis-core-api-unavailable
          (list (format "Gnosis %s is not yet available" operation))))

(defun gnosis-core-status ()
  "Return validated unauthenticated gnosisd health status."
  (let ((value (gnosis-core--perform-request "GET" "/health" nil 200 nil nil)))
    (unless (and (proper-list-p value)
                 (equal "ready" (plist-get value :status)))
      (signal 'gnosis-core-unavailable '("gnosisd is not ready")))
    value))

(defun gnosis-core-openapi ()
  "Return the daemon's minimally validated public OpenAPI document."
  (let ((value (gnosis-core--request
                "GET" "/v1/openapi.json" nil 200 nil t :none)))
    (unless (gnosis-core--openapi-p value)
      (gnosis-core--invalid "Invalid Gnosis OpenAPI document"))
    value))

(provide 'gnosis-core)
;;; gnosis-core.el ends here
