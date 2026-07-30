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

(defconst gnosis-core--json-null :gnosis-json-null)
(defconst gnosis-core--json-false :gnosis-json-false)

(defconst gnosis-core--default-online-review-target-count 100)

(defconst gnosis-core--uint32-max #xffffffff)

(defconst gnosis-core--thema-catalog-page-limit 100
  "Maximum themata requested in one management page.")

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
  (unless (or (and (stringp value) (not (string-empty-p value)))
              (integerp value))
    (signal 'gnosis-core-validation-error '("Invalid Gnosis path value")))
  (url-hexify-string
   (if (integerp value) (number-to-string value) value)))

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
             (gnosis-core--response-body-start) t)
        (downcase (match-string-no-properties 1))))))

(defun gnosis-core--header-etag ()
  "Return current response ETag header value."
  (save-excursion
    (goto-char (point-min))
    (let ((case-fold-search t))
      (when (re-search-forward
             "\\(?:\\`\\|[\r\n]\\)ETag:[ \t]*\\([^ \t\r\n]+\\)"
             (gnosis-core--response-body-start) t)
        (match-string-no-properties 1)))))

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
            (with-current-buffer (process-buffer process)
              (gnosis-core--header-status data)))))
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
    (method path body expected-status binary-p auth-p
            &optional headers expected-etag)
  "Perform one METHOD request to PATH with BODY.
EXPECTED-STATUS is the accepted status.  Return binary or decoded JSON as
selected by BINARY-P.  Add authentication when AUTH-P is non-nil and append
validated caller-supplied HEADERS.  Require EXPECTED-ETAG when non-nil."
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
                 ((and expected-etag
                       (not (equal expected-etag
                                   (gnosis-core--header-etag))))
                  (gnosis-core--invalid "Gnosis response ETag mismatch"))
                 (binary-p bytes)
                 (t
                  (unless (gnosis-core--json-content-type-p content-type)
                    (gnosis-core--invalid "Unexpected Gnosis content type"))
                  (gnosis-core--decode-json bytes)))))
          (gnosis-core--cleanup-retrieval
           buffer (and (buffer-live-p buffer) (get-buffer-process buffer))))))))

(defun gnosis-core--request
    (method path &optional body expected-status binary-p retry-p auth-p
            headers expected-etag)
  "Request METHOD PATH with optional BODY and return the decoded response.
EXPECTED-STATUS defaults to 200.  BINARY-P preserves bytes.  RETRY-P retries
one transport failure using the exact request.  AUTH-P defaults to non-nil;
`:none' disables authentication.  HEADERS are validated caller headers.
EXPECTED-ETAG requires an exact successful response ETag."
  (let ((attempts (if (and retry-p (equal method "GET")) 2 1))
        (authenticate (not (eq auth-p :none))))
    (catch 'result
      (dotimes (attempt attempts)
        (condition-case condition
            (throw 'result
                   (gnosis-core--perform-request
                    method path body (or expected-status 200)
                    binary-p authenticate headers expected-etag))
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

(defun gnosis-core--contract-cached-p ()
  "Return non-nil when the current authority has a cached contract."
  (let ((base (gnosis-core--validated-base-url)))
    (and (equal base (plist-get gnosis-core--contract-cache :base))
         (plist-get gnosis-core--contract-cache :document))))

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

(defun gnosis-core--uint32-p (value)
  "Return non-nil when VALUE is representable by Core as uint32."
  (and (integerp value) (<= 0 value gnosis-core--uint32-max)))

(defun gnosis-core--schema-format-valid-p (value schema)
  "Return non-nil when VALUE satisfies supported format rules in SCHEMA."
  (pcase (plist-get schema :format)
    ("uint32" (or (eq value gnosis-core--json-null)
                  (gnosis-core--uint32-p value)))
    (_ t)))

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
         (gnosis-core--schema-format-valid-p value schema)
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

(defun gnosis-core--typed-read-request
    (identity &optional path-values query)
  "Execute typed GET IDENTITY, refreshing one previously cached contract.
PATH-VALUES and QUERY supply the declared request inputs."
  (let ((cached (gnosis-core--contract-cached-p))
        (arguments
         (cond
          (query (list identity path-values query))
          (path-values (list identity path-values))
          (t (list identity)))))
    (condition-case condition
        (apply #'gnosis-core--typed-request arguments)
      (gnosis-core-protocol-error
       (if cached
           (progn
             (gnosis-core-refresh-contract)
             (apply #'gnosis-core--typed-request arguments))
         (signal (car condition) (cdr condition)))))))

(defun gnosis-core--typed-binary-request
    (identity &optional path-values query expected-etag)
  "Execute binary GET operation IDENTITY from the live contract.
PATH-VALUES fill path parameters and QUERY supplies query parameters.
EXPECTED-ETAG requires an exact successful response ETag."
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
     nil (car success) t t t nil expected-etag)))

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

(defun gnosis-core-document-revision (document-id revision digest)
  "Return DOCUMENT-ID source at exact REVISION matching DIGEST."
  (unless (and (natnump revision) (> revision 0)
               (stringp digest)
               (string-match-p "\\`[0-9a-f]\\{64\\}\\'" digest))
    (signal 'gnosis-core-validation-error
            '("Invalid Gnosis document revision request")))
  (let ((bytes
         (gnosis-core--typed-binary-request
         "get_notes__id__revisions__revision_"
          (list (cons "id" document-id)
                (cons "revision" revision))
          nil (format "\"%d\"" revision))))
    (unless (equal digest (secure-hash 'sha256 bytes))
      (gnosis-core--invalid "Gnosis document revision digest mismatch"))
    bytes))

(defun gnosis-core--org-string (bytes)
  "Return strict UTF-8 Org text decoded from unibyte BYTES."
  (unless (stringp bytes)
    (signal 'gnosis-core-validation-error '("Invalid Gnosis Org bytes")))
  (let* ((raw (string-to-unibyte bytes))
         (text (decode-coding-string raw 'utf-8)))
    (unless (and (not (cl-loop for char across text
                               thereis (eq 'eight-bit (char-charset char))))
                 (equal raw (encode-coding-string text 'utf-8 t)))
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
  (pcase (plist-get result :status)
    ("applied"
     (let ((head (plist-get result :authoritative))
           (changed (plist-get result :changed)))
       (unless (and (memq changed (list t gnosis-core--json-false))
                    head
                    (integerp (plist-get head :revision))
                    (or (null document-id)
                        (equal document-id (plist-get head :document_id)))
                    (pcase revision
                      (:create (and (eq changed t)
                                    (= 1 (plist-get head :revision))))
                      ((pred integerp)
                       (= (+ revision (if (eq changed t) 1 0))
                          (plist-get head :revision)))
                      (_ t))
                    (or (null logical-path)
                        (equal logical-path (plist-get head :logical_path)))
                    (pcase digest
                      (:null
                       (let ((value (plist-get head :content_sha256)))
                         (or (null value) (eq value gnosis-core--json-null))))
                      ((pred stringp)
                       (equal digest (plist-get head :content_sha256)))
                      (_ t))
                    (or (null active)
                        (pcase active
                          (:inactive
                           (eq (plist-get head :active)
                               gnosis-core--json-false))
                          (_ (eq (plist-get head :active) t)))))
         (signal 'gnosis-core-mutation-uncertain
                 '("Gnosis mutation authoritative head is invalid")))))
    ("rejected"
     (let ((rejection (plist-get result :rejection)))
       (unless (and rejection
                    (stringp (plist-get rejection :reason))
                    (or (not (equal "revision_conflict"
                                    (plist-get rejection :reason)))
                        (let ((expected
                               (let ((value (plist-get rejection :expected_revision)))
                                 (unless (eq value gnosis-core--json-null) value)))
                              (current
                               (let ((value (plist-get rejection :current_revision)))
                                 (unless (eq value gnosis-core--json-null) value))))
                          (and (or (null revision) (equal revision expected))
                               (or (null current) (integerp current))))))
         (signal 'gnosis-core-mutation-uncertain
                 '("Gnosis mutation rejection is invalid")))))
    (_
     (signal 'gnosis-core-mutation-uncertain
             '("Gnosis mutation status is invalid"))))
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
           (list :logical_path logical-path
                 :content_type "text/org; charset=utf-8"
                 :source text)
           (gnosis-core--document-mutation-headers operation-id))))
    (gnosis-core--document-mutation-result
     result operation-id nil :create logical-path digest t)))

(defun gnosis-core-document-replace
    (document-id operation-id revision logical-path bytes)
  "Replace DOCUMENT-ID at REVISION with LOGICAL-PATH and exact Org BYTES.
Use OPERATION-ID as stable mutation identity."
  (let* ((text (gnosis-core--org-string bytes))
         (digest (secure-hash 'sha256 (string-to-unibyte bytes)))
         (result
          (gnosis-core--typed-request
           "put_document-mutations__id_" (list (cons "id" document-id)) nil
           (list :logical_path logical-path
                 :content_type "text/org; charset=utf-8"
                 :source text)
           (gnosis-core--document-mutation-headers operation-id revision))))
    (gnosis-core--document-mutation-result
     result operation-id document-id revision logical-path digest t)))

(defun gnosis-core-document-rename
    (document-id operation-id revision logical-path expected-digest)
  "Rename DOCUMENT-ID at REVISION to LOGICAL-PATH.
Use OPERATION-ID as stable mutation identity and require EXPECTED-DIGEST."
  (gnosis-core--document-mutation-result
   (gnosis-core--typed-request
    "post_document-mutations__id__rename" (list (cons "id" document-id)) nil
    (list :logical_path logical-path)
    (gnosis-core--document-mutation-headers operation-id revision))
   operation-id document-id revision logical-path expected-digest t))

(defun gnosis-core-document-tombstone
    (document-id operation-id revision logical-path)
  "Tombstone DOCUMENT-ID at REVISION and LOGICAL-PATH under OPERATION-ID."
  (gnosis-core--document-mutation-result
   (gnosis-core--typed-request
    "delete_document-mutations__id_" (list (cons "id" document-id)) nil nil
    (gnosis-core--document-mutation-headers operation-id revision))
   operation-id document-id revision logical-path :null :inactive))

(defun gnosis-core-dashboard (&optional days)
  "Return the server dashboard projection for optional window DAYS."
  (if days
      (gnosis-core--typed-request
       "get_dashboard" nil (list (cons "days" days)))
    (gnosis-core--typed-request "get_dashboard")))

(defun gnosis-core--opaque-id-p (value)
  "Return non-nil when VALUE is a valid opaque protocol identifier."
  (and (stringp value)
       (not (string-empty-p (string-trim value)))
       (<= (string-bytes value) 128)))

(defun gnosis-core--document-path-p (value)
  "Return non-nil when VALUE is a confined relative document path."
  (and (stringp value)
       (not (string-empty-p value))
       (not (file-name-absolute-p value))
       (not (string-match-p "[\\\\\0]" value))
       (not (string-prefix-p "assets/" value))
       (cl-every (lambda (part)
                   (and (not (string-empty-p part))
                        (not (member part '("." "..")))))
                 (split-string value "/" nil))))

(defun gnosis-core--unique-vector-p (values)
  "Return non-nil when string vector VALUES has no duplicates."
  (and
   (vectorp values)
   (let ((seen (make-hash-table :test #'equal)))
     (cl-loop
      for value across values
      always (and (stringp value)
                  (not (gethash value seen))
                  (progn (puthash value t seen) t))))))

(defun gnosis-core--node-span-p (start end)
  "Return non-nil when START and END form a valid optional byte span."
  (or (and (eq start gnosis-core--json-null)
           (eq end gnosis-core--json-null))
      (and (natnump start) (natnump end) (< start end))))

(defun gnosis-core--node-projection-p (node)
  "Return non-nil when NODE satisfies semantic wire invariants."
  (let ((title (plist-get node :title))
        (path (plist-get node :title_path))
        (tags (plist-get node :tags)))
    (and
     (gnosis-core--opaque-id-p (plist-get node :node_id))
     (gnosis-core--opaque-id-p (plist-get node :document_id))
     (gnosis-core--document-path-p (plist-get node :logical_path))
     (natnump (plist-get node :document_revision))
     (> (plist-get node :document_revision) 0)
     (stringp (plist-get node :content_sha256))
     (string-match-p
      "\\`[0-9a-f]\\{64\\}\\'" (plist-get node :content_sha256))
     (stringp title)
     (not (string-empty-p (string-trim title)))
     (vectorp path)
     (> (length path) 0)
     (cl-every (lambda (part)
                 (and (stringp part)
                      (not (string-empty-p (string-trim part)))))
               path)
     (equal title (aref path (1- (length path))))
     (gnosis-core--unique-vector-p tags)
     (cl-every (lambda (tag) (not (string-empty-p (string-trim tag)))) tags)
     (natnump (plist-get node :level))
     (gnosis-core--node-span-p
      (plist-get node :start_byte) (plist-get node :end_byte))
     (memq (plist-get node :active) (list t gnosis-core--json-false)))))

(defun gnosis-core--increasing-node-ids-p (items after)
  "Return non-nil when ITEMS increase strictly after optional AFTER."
  (let ((previous after))
    (cl-loop
     for node across items
     for node-id = (plist-get node :node_id)
     always (prog1
                (or (null previous) (string-lessp previous node-id))
              (setq previous node-id)))))

(defun gnosis-core--node-page-p (page after limit)
  "Return non-nil when PAGE satisfies node pagination after AFTER at LIMIT."
  (let* ((items (plist-get page :items))
         (next (plist-get page :next))
         (last (and (vectorp items) (> (length items) 0)
                    (plist-get (aref items (1- (length items))) :node_id))))
    (and
     (vectorp items)
     (<= (length items) limit)
     (cl-every #'gnosis-core--node-projection-p items)
     (gnosis-core--increasing-node-ids-p items after)
     (if last
         (equal next last)
       (eq next gnosis-core--json-null)))))

(defun gnosis-core-nodes (&optional query after limit)
  "Return a server node page filtered by optional QUERY, AFTER, and LIMIT."
  (let ((parameters
         (delq nil (list (and after (cons "after" after))
                         (and limit (cons "limit" limit))
                         (and query (cons "q" query))))))
    (let ((page
           (if parameters
               (gnosis-core--typed-read-request "get_nodes" nil parameters)
             (gnosis-core--typed-read-request "get_nodes"))))
      (unless (gnosis-core--node-page-p page after (or limit 100))
        (gnosis-core--invalid "Invalid Gnosis node page"))
      page)))

(defun gnosis-core--catalog-node-p (node document-id)
  "Return non-nil when NODE is a valid compact catalog item.
When DOCUMENT-ID is non-nil, require NODE to belong to it."
  (let ((path (plist-get node :title_path)))
    (and
     (gnosis-core--opaque-id-p (plist-get node :node_id))
     (gnosis-core--opaque-id-p (plist-get node :document_id))
     (or (null document-id)
         (equal document-id (plist-get node :document_id)))
     (gnosis-core--document-path-p (plist-get node :logical_path))
     (vectorp path)
     (> (length path) 0)
     (cl-every (lambda (part)
                 (and (stringp part)
                      (not (string-empty-p (string-trim part)))))
               path)
     (memq (plist-get node :active) (list t gnosis-core--json-false)))))

(defun gnosis-core--catalog-generation-p (value)
  "Return non-nil when VALUE is a bounded opaque catalog generation."
  (and (stringp value)
       (not (string-empty-p value))
       (<= (string-bytes value) 512)))

(defun gnosis-core--node-catalog-page-p
    (page document-id after limit)
  "Return non-nil when PAGE for DOCUMENT-ID after AFTER fits LIMIT."
  (let* ((items (plist-get page :items))
         (next (plist-get page :next))
         (last (and (vectorp items) (> (length items) 0)
                    (plist-get (aref items (1- (length items))) :node_id))))
    (and
     (gnosis-core--catalog-generation-p (plist-get page :generation))
     (vectorp items)
     (<= (length items) limit)
     (cl-every (lambda (node)
                 (gnosis-core--catalog-node-p node document-id))
               items)
     (gnosis-core--increasing-node-ids-p items after)
     (or (eq next gnosis-core--json-null)
         (and last (equal next last))))))

(defun gnosis-core-node-catalog (&optional document-id after limit)
  "Return a compact node catalog page.
Optionally filter by DOCUMENT-ID, page after AFTER, and request LIMIT items."
  (let* ((parameters
          (delq nil
                (list (and document-id (cons "document_id" document-id))
                      (and after (cons "after" after))
                      (and limit (cons "limit" limit)))))
         (page
          (if parameters
              (gnosis-core--typed-read-request
               "get_nodes_catalog" nil parameters)
            (gnosis-core--typed-read-request "get_nodes_catalog"))))
    (unless (gnosis-core--node-catalog-page-p
             page document-id after (or limit 5000))
      (gnosis-core--invalid "Invalid Gnosis node catalog page"))
    page))

(defun gnosis-core--catalog-thema-p (thema)
  "Return non-nil when THEMA has valid compact catalog metadata."
  (and (gnosis-core--opaque-id-p (plist-get thema :thema_id))
       (integerp (plist-get thema :thema_revision))
       (> (plist-get thema :thema_revision) 0)
       (natnump (plist-get thema :state_revision))
       (member (plist-get thema :kind) '("basic" "cloze" "mcq" "mc-cloze"))
       (stringp (plist-get thema :keimenon))
       (vectorp (plist-get thema :hypothesis))
       (vectorp (plist-get thema :answer))
       (gnosis-core--unique-vector-p (plist-get thema :tags))
       (memq (plist-get thema :archived) (list t gnosis-core--json-false))
       (memq (plist-get thema :suspended) (list t gnosis-core--json-false))))

(defun gnosis-core--increasing-thema-ids-p (items after)
  "Return non-nil when catalog ITEMS increase strictly after AFTER."
  (let ((previous after))
    (cl-loop
     for thema across items
     for thema-id = (plist-get thema :thema_id)
     always (prog1
                (or (null previous) (string-lessp previous thema-id))
              (setq previous thema-id)))))

(defun gnosis-core--thema-catalog-page-p (page after limit)
  "Return non-nil when catalog PAGE after AFTER fits LIMIT."
  (let* ((items (plist-get page :items))
         (next (plist-get page :next))
         (last (and (vectorp items) (> (length items) 0)
                    (plist-get (aref items (1- (length items))) :thema_id))))
    (and (gnosis-core--catalog-generation-p (plist-get page :generation))
         (vectorp items)
         (<= (length items) limit)
         (cl-every #'gnosis-core--catalog-thema-p items)
         (gnosis-core--increasing-thema-ids-p items after)
         (or (eq next gnosis-core--json-null)
             (and last (equal next last))))))

(defun gnosis-core-thema-catalog (&optional query after limit)
  "Return a compact thema management page.
Optionally filter by QUERY, page after AFTER, and request LIMIT items."
  (let* ((parameters
          (delq nil
                (list (and query (cons "q" query))
                      (and after (cons "after" after))
                      (and limit (cons "limit" limit)))))
         (page
          (if parameters
              (gnosis-core--typed-read-request
               "get_themata_catalog" nil parameters)
            (gnosis-core--typed-read-request "get_themata_catalog"))))
    (unless (gnosis-core--thema-catalog-page-p
             page after (or limit gnosis-core--thema-catalog-page-limit))
      (gnosis-core--invalid "Invalid Gnosis thema catalog page"))
    page))

(defun gnosis-core-node (node-id)
  "Return the server projection for opaque NODE-ID."
  (let ((projection
         (gnosis-core--typed-read-request
          "get_nodes__id_" (list (cons "id" node-id)))))
    (unless (and (equal node-id (plist-get projection :node_id))
                 (gnosis-core--node-projection-p projection))
      (gnosis-core--invalid "Gnosis node identity mismatch"))
    projection))

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

(defun gnosis-core--thema-mutation-result
    (result operation-id &optional thema-id revision)
  "Return authoritative thema from mutation RESULT.
Correlate OPERATION-ID and, when non-nil, THEMA-ID and REVISION."
  (let ((status (plist-get result :status))
        (cursor (plist-get result :cursor)))
    (unless (and (gnosis-core--json-object-p result)
                 (equal 1 (plist-get result :format_version))
                 (equal operation-id (plist-get result :operation_id))
                 (stringp cursor)
                 (not (string-empty-p cursor)))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis mutation response identity is invalid")))
    (pcase status
      ("applied"
       (let* ((changed (plist-get result :changed))
              (authoritative (plist-get result :authoritative))
              (projection (plist-get authoritative :value))
              (result-revision (plist-get projection :revision)))
         (unless
             (and (memq changed (list t gnosis-core--json-false))
                  (gnosis-core--json-object-p authoritative)
                  (equal "thema" (plist-get authoritative :kind))
                  (gnosis-core--json-object-p projection)
                  (stringp (plist-get projection :thema_id))
                  (not (string-empty-p (plist-get projection :thema_id)))
                  (integerp result-revision)
                  (if thema-id
                      (and (equal thema-id (plist-get projection :thema_id))
                           (= result-revision
                              (if (eq changed t) (1+ revision) revision)))
                    (and (eq changed t) (= result-revision 1))))
           (signal 'gnosis-core-mutation-uncertain
                   '("Gnosis mutation response projection is invalid")))
         projection))
      ("rejected"
       (let* ((rejection (plist-get result :rejection))
              (reason (plist-get rejection :reason))
              (expected (plist-get rejection :expected_revision))
              (current (plist-get rejection :current_revision)))
         (unless
             (and (gnosis-core--json-object-p rejection)
                  (member reason '("revision_conflict" "not_found"
                                   "invalid_content" "path_conflict"
                                   "rename_required"))
                  (or (not (equal reason "revision_conflict"))
                      (and revision
                           (equal revision expected)
                           (or (eq current gnosis-core--json-null)
                               (and (integerp current) (>= current 0))))))
           (signal 'gnosis-core-mutation-uncertain
                   '("Gnosis mutation rejection is invalid")))
         (pcase reason
           ("revision_conflict"
            (signal 'gnosis-core-conflict '("Gnosis thema revision conflict")))
           ("not_found"
            (signal 'gnosis-core-not-found '("Gnosis thema not found")))
           (_
            (signal 'gnosis-core-validation-error
                    '("Gnosis thema mutation rejected"))))))
      (_
       (signal 'gnosis-core-mutation-uncertain
               '("Gnosis mutation response status is invalid"))))))

(defun gnosis-core-thema-create (operation-id request)
  "Create a thema from REQUEST under OPERATION-ID without retry.
Return its authoritative projection."
  (gnosis-core--thema-mutation-result
   (gnosis-core--typed-request
    "post_themata" nil nil request
    (list (gnosis-core--idempotency-key-header operation-id)))
   operation-id))

(defun gnosis-core-thema-update (thema-id operation-id revision request)
  "Update opaque THEMA-ID at REVISION from REQUEST under OPERATION-ID.
Do not retry automatically."
  (gnosis-core--thema-mutation-result
   (gnosis-core--typed-request
    "put_themata__id_" (list (cons "id" thema-id)) nil request
    (list (gnosis-core--idempotency-key-header operation-id)
          (gnosis-core--if-match-header revision)))
   operation-id thema-id revision))

(defun gnosis-core--thema-suspension-result
    (result thema-id operation-id state-revision suspended)
  "Validate suspension RESULT for THEMA-ID and OPERATION-ID.
STATE-REVISION is the requested base and SUSPENDED is the desired state."
  (let ((status (plist-get result :status))
        (cursor (plist-get result :cursor)))
    (unless (and (gnosis-core--json-object-p result)
                 (equal 1 (plist-get result :format_version))
                 (equal operation-id (plist-get result :operation_id))
                 (stringp cursor) (not (string-empty-p cursor)))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis suspension response identity is invalid")))
    (pcase status
      ("applied"
       (let* ((changed (plist-get result :changed))
              (authority (plist-get result :authoritative))
              (projection (plist-get authority :value))
              (state (plist-get projection :state))
              (expected (+ state-revision (if (eq changed t) 1 0))))
         (unless (and (memq changed (list t gnosis-core--json-false))
                      (equal "review_state" (plist-get authority :kind))
                      (equal thema-id (plist-get projection :thema_id))
                      (equal expected (plist-get state :state_revision))
                      (eq suspended (eq t (plist-get state :suspended))))
           (signal 'gnosis-core-mutation-uncertain
                   '("Gnosis suspension projection is invalid")))
         projection))
      ("rejected"
       (let* ((rejection (plist-get result :rejection))
              (reason (plist-get rejection :reason)))
         (unless (and (gnosis-core--json-object-p rejection)
                      (member reason '("revision_conflict" "not_found"
                                       "invalid_content"))
                      (or (not (equal reason "revision_conflict"))
                          (equal state-revision
                                 (plist-get rejection :expected_revision))))
           (signal 'gnosis-core-mutation-uncertain
                   '("Gnosis suspension rejection is invalid")))
         (pcase reason
           ("revision_conflict"
            (signal 'gnosis-core-conflict
                    '("Gnosis review-state revision conflict")))
           ("not_found"
            (signal 'gnosis-core-not-found '("Gnosis thema not found")))
           (_
            (signal 'gnosis-core-validation-error
                    '("Gnosis suspension mutation rejected"))))))
      (_
       (signal 'gnosis-core-mutation-uncertain
               '("Gnosis suspension response status is invalid"))))))

(defun gnosis-core-thema-set-suspended
    (thema-id state-revision suspended &optional operation-id)
  "Set THEMA-ID suspension at STATE-REVISION to SUSPENDED.
Use optional OPERATION-ID and return the authoritative review-state projection."
  (unless (memq suspended '(nil t))
    (signal 'gnosis-core-validation-error
            '("Invalid Gnosis suspension state")))
  (let* ((operation-id (or operation-id (gnosis-core-operation-id)))
         (wire-suspended (if suspended t gnosis-core--json-false))
         (result
          (gnosis-core--typed-request
           "put_themata__id__suspension" (list (cons "id" thema-id)) nil
           (list :suspended wire-suspended)
           (list (gnosis-core--idempotency-key-header operation-id)
                 (gnosis-core--if-match-header state-revision)))))
    (gnosis-core--thema-suspension-result
     result thema-id operation-id state-revision suspended)))

(defun gnosis-core--review-id-valid-p (value)
  "Return non-nil when VALUE is a valid review identity."
  (and (stringp value)
       (not (string-empty-p value))
       (<= (string-bytes value) 128)
       (not (string-search "\0" value))))

(defun gnosis-core--review-session-valid-p (session expected-status)
  "Return non-nil when SESSION is a valid online EXPECTED-STATUS projection."
  (let ((session-id (plist-get session :session_id))
        (status (plist-get session :status))
        (position (plist-get session :current_position))
        (length (plist-get session :queue_length))
        (started-at (plist-get session :started_at_us)))
    (and (gnosis-core--review-id-valid-p session-id)
         (equal "online" (plist-get session :mode))
         (equal expected-status status)
         (integerp started-at) (> started-at 0)
         (gnosis-core--uint32-p position)
         (gnosis-core--uint32-p length)
         (pcase status
           ("active" (< position length))
           ("completed" (= position length))
           ((or "quit" "aborted") (<= position length))
           (_ nil)))))

(defun gnosis-core-review-session-create (source &optional target-count)
  "Create and return a review session from SOURCE and TARGET-COUNT."
  (let ((session
         (gnosis-core--typed-request
          "post_review_sessions" nil nil
          (list :source source
                :target_count (or target-count gnosis-core--json-null)))))
    (unless (and (gnosis-core--review-session-valid-p session "active")
                 (equal source (plist-get session :source))
                 (equal (or target-count
                            gnosis-core--default-online-review-target-count)
                        (plist-get session :target_count)))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review session response is not correlated")))
    session))

(defun gnosis-core-review-session-active ()
  "Return the active review-session envelope."
  (let* ((envelope (gnosis-core--typed-request "get_review_sessions_active"))
         (session (plist-get envelope :session)))
    (unless (or (eq session gnosis-core--json-null)
                (gnosis-core--review-session-valid-p session "active"))
      (gnosis-core--invalid "Gnosis active review session is invalid"))
    envelope))

(defun gnosis-core--review-session-action (session-id action expected-status)
  "Apply review session ACTION to SESSION-ID."
  (let ((session
         (gnosis-core--typed-request
          (format "post_review_sessions__id__%s" action)
          (list (cons "id" session-id)))))
    (unless (and (equal session-id (plist-get session :session_id))
                 (gnosis-core--review-session-valid-p session expected-status))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review session response is invalid")))
    session))

(defun gnosis-core-review-session-resume (session-id)
  "Resume review SESSION-ID."
  (gnosis-core--review-session-action session-id "resume" "active"))

(defun gnosis-core-review-session-quit (session-id)
  "Quit review SESSION-ID."
  (gnosis-core--review-session-action session-id "quit" "quit"))

(defun gnosis-core-review-session-abort (session-id)
  "Abort review SESSION-ID."
  (gnosis-core--review-session-action session-id "abort" "aborted"))

(defun gnosis-core--review-skipped-item-valid-p (item)
  "Return non-nil when ITEM is a valid skipped-review projection."
  (let ((position (plist-get item :position))
        (thema-id (plist-get item :thema_id)))
    (and (gnosis-core--json-object-p item)
         (gnosis-core--uint32-p position)
         (gnosis-core--review-id-valid-p thema-id)
         (member (plist-get item :reason)
                 '("archived" "suspended" "content_changed"
                   "scheduling_changed" "unsafe_presentation")))))

(defun gnosis-core--review-skipped-valid-p
    (items start-position current-position)
  "Return non-nil when ITEMS cover START-POSITION to CURRENT-POSITION."
  (and (vectorp items)
       (gnosis-core--uint32-p start-position)
       (gnosis-core--uint32-p current-position)
       (let ((end (+ start-position (length items))))
         (and (gnosis-core--uint32-p end)
              (= end current-position)
              (let ((expected start-position))
                (seq-every-p
                 (lambda (item)
                   (let ((position (plist-get item :position)))
                     (and (gnosis-core--review-skipped-item-valid-p item)
                          (= position expected)
                          (progn (setq expected (1+ expected)) t))))
                 items))))))

(defun gnosis-core-review-attempt-begin (session-id)
  "Begin and return the reconciliation envelope for review SESSION-ID."
  (let* ((result
          (gnosis-core--typed-request
           "post_review_sessions__id__attempts"
           (list (cons "id" session-id))))
         (session (plist-get result :session))
         (attempt (plist-get result :attempt))
         (skipped (plist-get result :skipped))
         (active-p (gnosis-core--review-session-valid-p session "active"))
         (completed-p
          (gnosis-core--review-session-valid-p session "completed")))
    (unless
        (and (gnosis-core--json-object-p result)
             (equal session-id (plist-get session :session_id))
             (or active-p completed-p)
             (gnosis-core--review-skipped-valid-p
              skipped (plist-get result :start_position)
              (plist-get session :current_position))
             (if active-p
                 (and (gnosis-core--json-object-p attempt)
                      (gnosis-core--review-id-valid-p
                       (plist-get attempt :attempt_id))
                      (gnosis-core--review-id-valid-p
                       (plist-get attempt :thema_id))
                      (equal session-id (plist-get attempt :session_id))
                      (member (plist-get attempt :status)
                              '("pending" "graded")))
               (eq attempt gnosis-core--json-null)))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review reconciliation identity is invalid")))
    result))

(defun gnosis-core-review-attempt-answer (attempt-id answer)
  "Submit structured ANSWER for ATTEMPT-ID without automatic retry."
  (let ((attempt
         (gnosis-core--typed-request
          "post_review_attempts__id__answer"
          (list (cons "id" attempt-id)) nil answer)))
    (unless (and (equal attempt-id (plist-get attempt :attempt_id))
                 (equal (plist-get attempt :status) "graded")
                 (equal (plist-get attempt :response) answer))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review answer response is not correlated")))
    attempt))

(defun gnosis-core--review-cloze-check-valid-p (check inputs)
  "Return non-nil when CHECK is correlated with cloze INPUTS."
  (let* ((matches (plist-get check :matches))
         (all-entered-correct (plist-get check :all_entered_correct))
         (complete (plist-get check :complete))
         (question (plist-get check :question))
         (script (plist-get check :next_input_script))
         (expected-all-correct
          (if (and (vectorp matches)
                   (seq-every-p (lambda (value) (eq value t)) matches))
              t
            gnosis-core--json-false)))
    (and (vectorp inputs)
         (vectorp matches)
         (= (length inputs) (length matches))
         (seq-every-p (lambda (value)
                        (memq value (list t gnosis-core--json-false)))
                      matches)
         (eq all-entered-correct expected-all-correct)
         (memq complete (list t gnosis-core--json-false))
         (or (eq complete gnosis-core--json-false)
             (and (eq all-entered-correct t)
                  (eq script gnosis-core--json-null)))
         (stringp question)
         (not (string-empty-p (string-trim question)))
         (or (eq script gnosis-core--json-null)
             (and (stringp script)
                  (let ((case-fold-search nil))
                    (string-match-p "\\`[a-z-]+\\'" script)))))))

(defun gnosis-core-review-attempt-check-cloze (attempt-id inputs)
  "Check cumulative cloze INPUTS for ATTEMPT-ID without mutating it."
  (let ((check
         (gnosis-core--typed-request
          "post_review_attempts__id__cloze-check"
          (list (cons "id" attempt-id)) nil (list :inputs inputs))))
    (unless (gnosis-core--review-cloze-check-valid-p check inputs)
      (gnosis-core--invalid "Invalid Gnosis cloze-check response"))
    check))

(defun gnosis-core-review-attempt-state (attempt-id)
  "Return authoritative revisioned state for ATTEMPT-ID."
  (let ((state
         (gnosis-core--typed-request
          "get_review_attempt_state" (list (cons "id" attempt-id)))))
    (unless (equal attempt-id
                   (plist-get (plist-get state :attempt) :attempt_id))
      (gnosis-core--invalid "Gnosis review attempt state identity mismatch"))
    state))

(defun gnosis-core--review-desired-state
    (identity attempt-id expected-revision key desired operation-id)
  "Set ATTEMPT-ID's desired review KEY through IDENTITY.
Correlate the response with EXPECTED-REVISION and OPERATION-ID."
  (let* ((operation-id (or operation-id (gnosis-core-operation-id)))
         (result
          (gnosis-core--typed-request
           identity (list (cons "id" attempt-id)) nil
           (list :version 1 :operation_id operation-id
                 :expected_attempt_revision expected-revision key desired)))
         (attempt (plist-get result :attempt))
         (changed (plist-get result :changed))
         (expected-result-revision (1+ expected-revision)))
    (unless (and (integerp expected-revision)
                 (integerp (plist-get result :attempt_revision))
                 (equal 1 (plist-get result :version))
                 (equal operation-id (plist-get result :operation_id))
                 (memq changed (list t gnosis-core--json-false))
                 (= expected-result-revision
                    (plist-get result :attempt_revision))
                 (equal attempt-id (plist-get attempt :attempt_id))
                 (equal desired (plist-get attempt key)))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review mutation response correlation failed")))
    result))

(defun gnosis-core-review-attempt-set-accepted-success
    (attempt-id expected-revision accepted-success &optional operation-id)
  "Set ATTEMPT-ID's ACCEPTED-SUCCESS at EXPECTED-REVISION."
  (gnosis-core--review-desired-state
   "put_review_attempt_accepted_success" attempt-id expected-revision
   :accepted_success accepted-success operation-id))

(defun gnosis-core-review-attempt-set-suspension
    (attempt-id expected-revision desired-suspended &optional operation-id)
  "Set ATTEMPT-ID's DESIRED-SUSPENDED at EXPECTED-REVISION."
  (gnosis-core--review-desired-state
   "put_review_attempt_suspension" attempt-id expected-revision
   :desired_suspended desired-suspended operation-id))

(defun gnosis-core-review-attempt-commit (attempt-id)
  "Commit ATTEMPT-ID without automatic retry and return its event identity."
  (let ((result
         (gnosis-core--typed-request
          "post_review_attempts__id__commit" (list (cons "id" attempt-id)))))
    (unless (equal attempt-id (plist-get result :attempt_id))
      (signal 'gnosis-core-mutation-uncertain
              '("Gnosis review commit response correlation failed")))
    result))

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
