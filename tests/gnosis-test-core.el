;;; gnosis-test-core.el --- Tests for the Gnosis HTTP foundation  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-core)

(defun gnosis-test-core--http-buffer (status body &optional content-type)
  "Return synthetic HTTP response buffer for STATUS and BODY.
CONTENT-TYPE defaults to application/json."
  (let ((buffer (generate-new-buffer " *gnosis-core-http*")))
    (with-current-buffer buffer
      (set-buffer-multibyte nil)
      (insert "HTTP/1.1 " (number-to-string status) " Test\r\n"
              "Content-Type: " (or content-type "application/json")
              "\r\n\r\n" body)
      (setq-local url-http-response-status status)
      (setq-local url-http-end-of-headers
                  (copy-marker (1- (- (point-max) (string-bytes body))))))
    buffer))

(ert-deftest gnosis-core-validates-service-authority ()
  (dolist (url '("http://127.0.0.1:7842" "http://localhost:7842"
                 "https://gnosis.example.org"))
    (let ((gnosis-core-base-url url))
      (should (equal url (gnosis-core--validated-base-url)))))
  (dolist (url '("http://gnosis.example.org" "https://user@example.org"
                 "https://example.org/path" "https://example.org?query"))
    (let ((gnosis-core-base-url url))
      (should-error (gnosis-core--validated-base-url)
                    :type 'gnosis-core-validation-error))))

(ert-deftest gnosis-core-validates-bearer-grammar ()
  (should (gnosis-core--valid-token-p (make-string 40 ?a)))
  (should (gnosis-core--valid-token-p
           "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN+/="))
  (dolist (token (list nil "short" (make-string 129 ?a)
                       (concat (make-string 39 ?a) "φ")
                       (concat (make-string 39 ?a) " ")))
    (should-not (gnosis-core--valid-token-p token))))

(ert-deftest gnosis-core-resolves-function-secret-per-request ()
  (let ((calls 0))
    (cl-letf (((symbol-function 'auth-source-search)
               (lambda (&rest _)
                 (list (list :secret
                             (lambda ()
                               (cl-incf calls)
                               (make-string 40 ?x)))))))
      (should (equal (make-string 40 ?x) (gnosis-core--token)))
      (should (equal (make-string 40 ?x) (gnosis-core--token)))
      (should (= calls 2)))))

(ert-deftest gnosis-core-sends-bearer-and-json-without-retaining-token ()
  (let (headers data)
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (setq headers url-request-extra-headers
                       data url-request-data)
                 (gnosis-test-core--http-buffer 200 "{\"ok\":true}"))))
      (should (eq t (plist-get
                     (gnosis-core--perform-request
                      "POST" "/v1/test" '(:value 1) 200 nil t)
                     :ok)))
      (should (equal (concat "Bearer " (make-string 40 ?x))
                     (cdr (assoc "Authorization" headers))))
      (should (equal "application/json"
                     (cdr (assoc "Content-Type" headers))))
      (should (equal "application/json" (cdr (assoc "Accept" headers))))
      (should (string-match-p "\\\"value\\\":1" data)))))

(ert-deftest gnosis-core-accepts-binary-responses-without-assuming-a-media-type ()
  (let (headers)
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (setq headers url-request-extra-headers)
                 (gnosis-test-core--http-buffer
                  200 "asset" "application/octet-stream"))))
      (should (equal "asset"
                     (gnosis-core--perform-request
                      "GET" "/v1/assets/test" nil 200 t t)))
      (should (equal "*/*" (cdr (assoc "Accept" headers)))))))

(ert-deftest gnosis-core-openapi-request-is-public-and-retryable ()
  (let (calls)
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest arguments)
                 (setq calls arguments)
                 '(:openapi "3.1.0"
                   :info (:title "Gnosis API" :version "1.0.0")
                   :paths (:/v1/test (:get (:operationId "get_test")))))))
      (should (equal "3.1.0" (plist-get (gnosis-core-openapi) :openapi)))
      (should (equal '("GET" "/v1/openapi.json" nil 200 nil t :none)
                     calls)))))

(ert-deftest gnosis-core-rejects-malformed-openapi-document ()
  (dolist (value '(nil (:openapi "3.0.0" :info nil :paths nil)
                   (:openapi "3.1.0" :info (:title "Gnosis API") :paths nil)
                   (:openapi "3.1.0"
                    :info (:title "Not Gnosis" :version "1.0.0")
                    :paths (:/v1/test (:get nil)))
                   (:openapi "3.1.0"
                    :info (:title "Gnosis API" :version "")
                    :paths (:/v1/test (:get nil)))
                   (:openapi "3.1.0"
                    :info (:title "Gnosis API" :version "1.0.0")
                    :paths nil)))
    (should-not (gnosis-core--openapi-p value)))
  (cl-letf (((symbol-function 'gnosis-core--request)
             (lambda (&rest _) '(:openapi "3.0.0"))))
    (should-error (gnosis-core-openapi)
                  :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-maps-generic-error-envelope-by-status ()
  (cl-letf (((symbol-function 'gnosis-core--token)
             (lambda () (make-string 40 ?x)))
            ((symbol-function 'gnosis-core--retrieve-bounded)
             (lambda (&rest _)
               (gnosis-test-core--http-buffer
                422
                "{\"error\":{\"code\":\"future_error\",\"message\":\"bad request\",\"meta\":{\"new\":true}}}"))))
    (let ((condition
           (should-error
            (gnosis-core--perform-request "GET" "/v1/test" nil 200 nil t)
            :type 'gnosis-core-validation-error)))
      (should (equal '("bad request" "future_error") (cdr condition))))))

(ert-deftest gnosis-core-rejects-unbounded-or-malformed-error-envelope ()
  (dolist (body
           (list "{}"
                 "{\"error\":{\"code\":7,\"message\":\"bad\"}}"
                 (format "{\"error\":{\"code\":\"bad\",\"message\":\"%s\"}}"
                         (make-string 513 ?x))))
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (gnosis-test-core--http-buffer 500 body))))
      (should-error
       (gnosis-core--perform-request "GET" "/v1/test" nil 200 nil t)
       :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-rejects-redirect-and-wrong-content-type ()
  (dolist (response '((302 "application/json") (200 "text/html")))
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (gnosis-test-core--http-buffer
                  (car response) "{}" (cadr response)))))
      (should-error
       (gnosis-core--perform-request "GET" "/v1/test" nil 200 nil t)
       :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-retries-only-idempotent-requests ()
  (let ((calls 0))
    (cl-letf (((symbol-function 'gnosis-core--perform-request)
               (lambda (&rest _)
                 (cl-incf calls)
                 (if (= calls 1)
                     (signal 'gnosis-core-unavailable '("down"))
                   '(:ok t)))))
      (should (equal '(:ok t)
                     (gnosis-core--request
                      "GET" "/v1/test" nil 200 nil t t)))
      (should (= calls 2))))
  (dolist (method '("POST" "PUT"))
    (let ((calls 0))
      (cl-letf (((symbol-function 'gnosis-core--perform-request)
                 (lambda (&rest _)
                   (cl-incf calls)
                   (signal 'gnosis-core-unavailable '("down")))))
        (should-error
         (gnosis-core--request method "/v1/test" nil 200 nil t t)
         :type 'gnosis-core-unavailable)
        (should (= calls 1))))))

(ert-deftest gnosis-core-status-requires-ready-health ()
  (cl-letf (((symbol-function 'gnosis-core--perform-request)
             (lambda (&rest _) '(:status "ready"))))
    (should (equal '(:status "ready") (gnosis-core-status))))
  (cl-letf (((symbol-function 'gnosis-core--perform-request)
             (lambda (&rest _) '(:status "starting"))))
    (should-error (gnosis-core-status) :type 'gnosis-core-unavailable)))

(ert-deftest gnosis-core-api-unavailable-is-explicit ()
  (should-error (gnosis-core-api-unavailable "review")
                :type 'gnosis-core-api-unavailable))

(ert-deftest gnosis-core-rejects-oversized-partial-headers ()
  (let ((gnosis-core--response-header-limit 4))
    (with-temp-buffer
      (set-buffer-multibyte nil)
      (insert "123")
      (should-not (gnosis-core--header-status "4"))
      (should (eq (gnosis-core--header-status "56") 'overflow)))))

(ert-deftest gnosis-core-rejects-content-encoding-before-decoding ()
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (let* ((process (make-pipe-process
                     :name "gnosis-encoding" :buffer (current-buffer)
                     :noquery t))
           (state (vector nil nil nil nil)))
      (unwind-protect
          (progn
            (process-put process 'gnosis-core--state state)
            (process-put process 'gnosis-core--limit 8)
            (process-put process 'gnosis-core--headers-verified nil)
            (process-put process 'gnosis-core--original-filter
                         (lambda (_process _data)))
            (set-process-filter process #'gnosis-core--bounded-process-filter)
            (gnosis-core--bounded-process-filter
             process
             "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\ncompressed")
            (should (eq (car (aref state 2)) 'gnosis-core-protocol-error))
            (should-not (process-live-p process)))
        (when (process-live-p process) (delete-process process))))))

(ert-deftest gnosis-core-bypasses-proxy-only-for-loopback ()
  (let ((custom (lambda (_url) "CUSTOM")))
    (let ((url-proxy-locator custom))
      (should (equal "DIRECT"
                     (funcall (gnosis-core--locator-for-url
                               "http://127.0.0.1:7842") nil)))
      (should (eq custom
                  (gnosis-core--locator-for-url
                   "https://example.test"))))))

(provide 'gnosis-test-core)
;;; gnosis-test-core.el ends here
