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
       :object-type 'plist :array-type 'list
       :null-object gnosis-core--json-null
       :false-object gnosis-core--json-false)
    (error (gnosis-core--invalid "Invalid JSON response"))))

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
    (method path body expected-status binary-p auth-p)
  "Perform one METHOD request to PATH with BODY.
EXPECTED-STATUS is the accepted status.  Return binary or decoded JSON as
selected by BINARY-P.  Add authentication when AUTH-P is non-nil."
  (let* ((url (concat (gnosis-core--validated-base-url) path))
         (locator (gnosis-core--locator-for-url url)))
    (let ((url-proxy-locator locator)
          (url-debug (and (not auth-p) url-debug)))
      (let* ((url-request-method method)
             (url-request-data (and body (gnosis-core--json-body body)))
             (url-request-extra-headers
              (append
               (and auth-p
                    `(("Authorization" .
                       ,(concat "Bearer " (gnosis-core--token)))))
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
    (method path &optional body expected-status binary-p retry-p auth-p)
  "Request METHOD PATH with optional BODY and return the decoded response.
EXPECTED-STATUS defaults to 200.  BINARY-P preserves bytes.  RETRY-P retries
one transport failure using the exact request.  AUTH-P defaults to non-nil;
`:none' disables authentication."
  (let ((attempts (if (and retry-p (equal method "GET")) 2 1))
        (authenticate (not (eq auth-p :none))))
    (catch 'result
      (dotimes (attempt attempts)
        (condition-case condition
            (throw 'result
                   (gnosis-core--perform-request
                    method path body (or expected-status 200)
                    binary-p authenticate))
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
