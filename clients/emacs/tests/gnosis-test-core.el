;;; gnosis-test-core.el --- Tests for the Gnosis HTTP foundation  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-core)

(defvar gnosis-test-core--begin-response nil
  "Stubbed begin-attempt response for correlation tests.")

(defun gnosis-test-core--return-begin-response (&rest _arguments)
  "Return `gnosis-test-core--begin-response'."
  gnosis-test-core--begin-response)

(defun gnosis-test-core--node (&rest overrides)
  "Return a valid node projection with OVERRIDES."
  (let ((node
         (copy-tree
          `(:node_id "node-a"
            :document_id "document-a"
            :logical_path "notes/a.org"
            :document_revision 3
            :content_sha256
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            :title "Alpha"
            :title_path ["Root" "Alpha"]
            :tags ["one" "two"]
            :level 2
            :start_byte ,gnosis-core--json-null
            :end_byte ,gnosis-core--json-null
            :active t)
          t)))
    (while overrides
      (setq node (plist-put node (pop overrides) (pop overrides))))
    node))

(defun gnosis-test-core--catalog-node (&rest overrides)
  "Return a valid compact node catalog item with OVERRIDES."
  (let ((node
         (copy-tree
          '(:node_id "node-a"
            :document_id "document-a"
            :logical_path "notes/a.org"
            :title_path ["Root" "Alpha"]
            :active t)
          t)))
    (while overrides
      (setq node (plist-put node (pop overrides) (pop overrides))))
    node))

(defun gnosis-test-core--catalog-thema (&rest overrides)
  "Return a valid compact thema catalog item with OVERRIDES."
  (let ((thema
         (copy-tree
          `(:thema_id "thema-a" :thema_revision 3 :state_revision 2
            :kind "basic" :keimenon "Question" :hypothesis [,gnosis-core--json-null]
            :answer ["Answer"] :tags ["tag"] :archived ,gnosis-core--json-false
            :suspended ,gnosis-core--json-false)
          t)))
    (while overrides
      (setq thema (plist-put thema (pop overrides) (pop overrides))))
    thema))

(defvar gnosis-core--contract-cache)

(defun gnosis-test-core--http-buffer
    (status body &optional content-type headers)
  "Return synthetic HTTP response buffer for STATUS and BODY.
CONTENT-TYPE defaults to application/json.  HEADERS are appended."
  (let ((buffer (generate-new-buffer " *gnosis-core-http*")))
    (with-current-buffer buffer
      (set-buffer-multibyte nil)
      (insert "HTTP/1.1 " (number-to-string status) " Test\r\n"
              "Content-Type: " (or content-type "application/json")
              "\r\n")
      (dolist (header headers)
        (insert (car header) ": " (cdr header) "\r\n"))
      (insert "\r\n" body)
      (setq-local url-http-response-status status)
      (setq-local url-http-end-of-headers
                  (copy-marker (1- (- (point-max) (string-bytes body))))))
    buffer))

(defun gnosis-test-core--serve-json-once (server client _message)
  "Configure CLIENT accepted by SERVER to return one JSON response."
  (set-process-coding-system client 'binary 'binary)
  (set-process-filter
   client
   (lambda (process data)
     (let ((request (concat (or (process-get process 'request) "") data)))
       (process-put process 'request request)
       (when-let* ((header-end (string-match "\r\n\r\n" request))
                   ((string-match
                     "\r\nContent-length: \\([0-9]+\\)" request))
                   (content-length (string-to-number (match-string 1 request)))
                   ((>= (string-bytes request)
                        (+ header-end 4 content-length))))
         (process-put server 'request request)
         (process-send-string
          process
          "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
         (delete-process process))))))

(defun gnosis-test-core--contract ()
  "Return a small live-contract fixture for typed request tests."
  `(:openapi "3.1.0"
    :info (:title "Gnosis API" :version "1.0.0")
    :paths
    (:/v1/widgets/{id}
     (:put
      (:operationId "put_widget"
       :parameters
       [(:in "path" :name "id" :required t :schema (:type "string"))
        (:in "header" :name "If-Match" :required t
         :schema (:type "string"))]
       :requestBody
       (:required t :content
        (:application/json (:schema (:$ref "#/components/schemas/WidgetWrite"))))
       :responses
       (:200 (:description "Success" :content
              (:application/json
               (:schema (:$ref "#/components/schemas/Widget")))))
       :security [(:bearerAuth [])]
       :x-gnosis-required-scopes ["content:write"])))
    :components
    (:schemas
     (:WidgetWrite
      (:type "object" :additionalProperties ,gnosis-core--json-false
       :required ["value"] :properties (:value (:type "string")))
      :Widget
      (:type "object" :additionalProperties ,gnosis-core--json-false
       :required ["id"] :properties (:id (:type "string")))))))

(defun gnosis-test-core--thema-contract ()
  "Return a small live-contract fixture for thema write tests."
  `(:openapi "3.1.0" :info (:title "Gnosis API" :version "1.0.0")
    :paths
    (:/v1/themata/{id}
     (:put
      (:operationId "put_themata__id_"
       :parameters
       [(:in "path" :name "id" :required t :schema (:type "string"))
        (:in "header" :name "If-Match" :required t :schema (:type "string"))]
       :requestBody
       (:required t :content
        (:application/json (:schema (:$ref "#/components/schemas/ThemaWrite"))))
       :responses (:200 (:description "ok" :content
                         (:application/json (:schema (:type "object")))))
       :security [(:bearerAuth [])]
       :x-gnosis-required-scopes ["content:write"])))
    :components
    (:schemas
     (:ThemaWrite
      (:type "object" :additionalProperties ,gnosis-core--json-false
       :required ["kind" "keimenon" "answer"]
       :properties
       (:kind (:enum ["basic" "cloze"] :type "string")
        :keimenon (:type "string")
        :answer (:type "array" :items (:type "string"))
        :concepts
        (:type "array" :items
         (:type "object" :additionalProperties ,gnosis-core--json-false
          :required ["node_id" "relation"]
          :properties
          (:node_id (:type "string")
           :relation (:enum ["primary" "supporting" "reference"]
                      :type "string"))))))))))

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

(ert-deftest gnosis-core-preserves-json-array-shape ()
  (let ((value (gnosis-core--decode-json
                "{\"empty\":[],\"items\":[\"one\"]}")))
    (should (vectorp (plist-get value :empty)))
    (should (equal ["one"] (plist-get value :items)))))

(ert-deftest gnosis-core-rejects-duplicate-json-object-keys ()
  (should-not (gnosis-core--json-object-p '(:value "one" :value "two"))))

(ert-deftest gnosis-core-encodes-opaque-path-and-query-values ()
  (should (equal "opaque%2Fid%3Fx" (gnosis-core--path-segment "opaque/id?x")))
  (should (equal "7" (gnosis-core--path-segment 7)))
  (should (equal "after=a%2Fb%3Fc&q=%C3%BC"
                 (gnosis-core--query-string
                  '(("after" . "a/b?c") ("q" . "ü"))))))

(ert-deftest gnosis-core-generates-valid-distinct-operation-identities ()
  (let ((first (gnosis-core-operation-id))
        (second (gnosis-core-operation-id)))
    (should (string-match-p
             "\\`emacs:[A-Za-z0-9][A-Za-z0-9._:-]\\{0,127\\}\\'" first))
    (should-not (equal first second))))

(ert-deftest gnosis-core-builds-revision-and-idempotency-headers ()
  (let ((identity (gnosis-core-operation-id)))
    (should (equal '("If-Match" . "\"7\"")
                   (gnosis-core--if-match-header 7)))
    (should (equal (cons "Idempotency-Key" identity)
                   (gnosis-core--idempotency-key-header identity))))
  (dolist (value '(-1 1.5 "7"))
    (should-error (gnosis-core--if-match-header value)
                  :type 'gnosis-core-validation-error))
  (should-error (gnosis-core--idempotency-key-header "bad identity")
                :type 'gnosis-core-validation-error))

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

(ert-deftest gnosis-core-sends-utf8-json-with-multibyte-ascii-token ()
  (let* ((server
          (make-network-process
           :name "gnosis-test-http-server" :server t :host "127.0.0.1"
           :service t :family 'ipv4 :coding 'binary :noquery t
           :log #'gnosis-test-core--serve-json-once))
         (port (process-contact server :service))
         (gnosis-core-base-url (format "http://127.0.0.1:%d" port))
         (gnosis-core-connect-timeout 0.5))
    (unwind-protect
        (cl-letf (((symbol-function 'auth-source-search)
                   (lambda (&rest _)
                     (list (list :secret
                                 (string-to-multibyte (make-string 40 ?x)))))))
          (should
           (eq t
               (plist-get
                (gnosis-core--perform-request
                 "POST" "/test" '(:kind "text" :value "пилет") 200 nil t)
                :ok)))
          (should
           (string-match-p
            (regexp-quote
             (encode-coding-string
              "{\"kind\":\"text\",\"value\":\"пилет\"}" 'utf-8-unix))
            (process-get server 'request))))
      (delete-process server))))

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

(ert-deftest gnosis-core-adds-only-safe-caller-headers ()
  (let (headers)
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (setq headers url-request-extra-headers)
                 (gnosis-test-core--http-buffer 200 "{}"))))
      (gnosis-core--perform-request
       "PUT" "/v1/test" nil 200 nil t '(("If-Match" . "\"7\"")))
      (should (equal "\"7\"" (cdr (assoc "If-Match" headers))))))
  (dolist (headers '((("Authorization" . "replacement"))
                     (("Accept" . "text/plain"))
                     (("Host" . "attacker.example"))
                     (("Content-Length" . "0"))
                     (("If-Match" . "bad\nvalue"))
                     (("Bad Header" . "value"))))
    (should-error
     (gnosis-core--perform-request "PUT" "/v1/test" nil 200 nil t headers)
     :type 'gnosis-core-validation-error)))

(ert-deftest gnosis-core-typed-request-follows-live-operation-contract ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract)))
        calls)
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest arguments)
                 (setq calls arguments)
                 '(:id "opaque/id"))))
      (should (equal '(:id "opaque/id")
                     (gnosis-core--typed-request
                      "put_widget" '(("id" . "opaque/id")) nil
                      '(:value "text") '(("If-Match" . "\"3\"")))))
      (should (equal
               '("PUT" "/v1/widgets/opaque%2Fid" (:value "text")
                 200 nil nil t (("If-Match" . "\"3\"")))
               calls)))))

(ert-deftest gnosis-core-mutation-revalidates-response-after-contract-refresh ()
  (let* ((stale (gnosis-test-core--contract))
         (fresh (copy-tree stale t))
         (widget (plist-get
                  (plist-get (plist-get fresh :components) :schemas)
                  :Widget))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842" :document stale))
         (requests 0)
         (refreshes 0))
    (setf (plist-get (plist-get widget :properties) :extra)
          '(:type "boolean"))
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest _)
                 (cl-incf requests)
                 '(:id "widget" :extra t)))
              ((symbol-function 'gnosis-core-refresh-contract)
               (lambda ()
                 (cl-incf refreshes)
                 (setf (plist-get gnosis-core--contract-cache :document)
                       fresh))))
      (should
       (equal '(:id "widget" :extra t)
              (gnosis-core--typed-request
               "put_widget" '(("id" . "widget")) nil '(:value "text")
               '(("If-Match" . "\"3\"")))))
      (should (= requests 1))
      (should (= refreshes 1)))))

(ert-deftest gnosis-core-mutation-rejects-refreshed-auth-doctrine-drift ()
  (dolist (key '(:security :x-gnosis-required-scopes))
    (let* ((stale (gnosis-test-core--contract))
           (fresh (copy-tree stale t))
           (widget (plist-get
                    (plist-get (plist-get fresh :components) :schemas)
                    :Widget))
           (operation (plist-get
                       (plist-get (plist-get fresh :paths)
                                  :/v1/widgets/{id})
                       :put))
           (gnosis-core--contract-cache
            (list :base "http://127.0.0.1:7842" :document stale))
           (requests 0)
           (refreshes 0))
      (setf (plist-get (plist-get widget :properties) :extra)
            '(:type "boolean")
            (plist-get operation key) [])
      (cl-letf (((symbol-function 'gnosis-core--request)
                 (lambda (&rest _)
                   (cl-incf requests)
                   '(:id "widget" :extra t)))
                ((symbol-function 'gnosis-core-refresh-contract)
                 (lambda ()
                   (cl-incf refreshes)
                   (setf (plist-get gnosis-core--contract-cache :document)
                         fresh))))
        (should-error
         (gnosis-core--typed-request
          "put_widget" '(("id" . "widget")) nil '(:value "text")
          '(("If-Match" . "\"3\"")))
         :type 'gnosis-core-mutation-uncertain)
        (should (= requests 1))
        (should (= refreshes 1))))))

(ert-deftest gnosis-core-mutation-rejects-response-after-contract-refresh ()
  (let* ((contract (gnosis-test-core--contract))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842" :document contract))
         (requests 0)
         (refreshes 0))
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest _)
                 (cl-incf requests)
                 '(:id "widget" :extra t)))
              ((symbol-function 'gnosis-core-refresh-contract)
               (lambda ()
                 (cl-incf refreshes)
                 contract)))
      (should-error
       (gnosis-core--typed-request
        "put_widget" '(("id" . "widget")) nil '(:value "text")
        '(("If-Match" . "\"3\"")))
       :type 'gnosis-core-mutation-uncertain)
      (should (= requests 1))
      (should (= refreshes 1)))))

(ert-deftest gnosis-core-typed-request-rejects-contract-drift ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract))))
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . "widget")) nil '(:value "text") nil)
     :type 'gnosis-core-protocol-error)
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . "widget")) nil '(:unknown "text")
      '(("If-Match" . "\"3\"")))
     :type 'gnosis-core-protocol-error)
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . "widget")) nil '(:value "text")
      '(("If-Match" . "bad\nvalue")))
     :type 'gnosis-core-validation-error)
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest _) '(:id "widget" :extra t))))
      (should-error
       (gnosis-core--typed-request
        "put_widget" '(("id" . "widget")) nil '(:value "text")
        '(("If-Match" . "\"3\"")))
       :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-typed-request-requires-bearer-scope-metadata ()
  (dolist (key '(:security :x-gnosis-required-scopes))
    (let* ((contract (copy-tree (gnosis-test-core--contract) t))
           (operation (plist-get
                       (plist-get (plist-get contract :paths)
                                  :/v1/widgets/{id})
                       :put))
           (gnosis-core--contract-cache
            (list :base "http://127.0.0.1:7842" :document contract)))
      (setf (plist-get operation key) [])
      (should-error
       (gnosis-core--typed-request
        "put_widget" '(("id" . "widget")) nil '(:value "text")
        '(("If-Match" . "\"3\"")))
       :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-typed-request-rejects-compound-authentication ()
  (let* ((contract (copy-tree (gnosis-test-core--contract) t))
         (operation (plist-get
                     (plist-get (plist-get contract :paths)
                                :/v1/widgets/{id})
                     :put))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842" :document contract)))
    (setf (plist-get operation :security)
          [(:bearerAuth [] :apiKeyAuth [])])
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . "widget")) nil '(:value "text")
      '(("If-Match" . "\"3\"")))
     :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-typed-request-validates-parameter-schemas ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract))))
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . 7)) nil '(:value "text")
      '(("If-Match" . "\"3\"")))
     :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-typed-request-rejects-incoherent-path-template ()
  (let* ((contract (copy-tree (gnosis-test-core--contract) t))
         (item (plist-get (plist-get contract :paths) :/v1/widgets/{id}))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842"
                :document (plist-put contract :paths
                                     (list :/v1/widgets item)))))
    (should-error
     (gnosis-core--typed-request
      "put_widget" '(("id" . "widget")) nil '(:value "text")
      '(("If-Match" . "\"3\"")))
     :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-validates-schema-composition-without-explicit-type ()
  (let ((contract (gnosis-test-core--contract)))
    (should (gnosis-core--schema-valid-p
             "allowed" '(:enum ["allowed"]) contract))
    (should (gnosis-core--schema-valid-p
             "allowed" '(:not (:enum ["blocked"])) contract))
    (should (gnosis-core--schema-valid-p
             gnosis-core--json-null
             '(:anyOf [(:$ref "#/components/schemas/Widget")
                       (:type "null")])
             contract))
    (should-not
     (gnosis-core--schema-valid-p
      '(:kind "allowed")
      '(:type "object" :required ["id"]
        :oneOf [(:type "object" :required ["kind"]
                 :properties (:kind (:const "allowed")))])
      contract))
    (should-not (gnosis-core--schema-valid-p
                  "blocked" '(:not (:enum ["blocked"])) contract))))

(ert-deftest gnosis-core-validates-uint32-schema-format ()
  (let ((schema '(:type "integer" :format "uint32")))
    (should (gnosis-core--schema-valid-p 0 schema nil))
    (should (gnosis-core--schema-valid-p #xffffffff schema nil))
    (should-not (gnosis-core--schema-valid-p -1 schema nil))
    (should-not (gnosis-core--schema-valid-p #x100000000 schema nil))
    (should
     (gnosis-core--schema-valid-p
      gnosis-core--json-null
      '(:type ["integer" "null"] :format "uint32") nil))))

(ert-deftest gnosis-core-refreshes-contract-when-authority-changes ()
  (let ((gnosis-core--contract-cache nil)
        (gnosis-core-base-url "https://one.example")
        (calls 0))
    (cl-letf (((symbol-function 'gnosis-core-openapi)
               (lambda ()
                 (cl-incf calls)
                 (gnosis-test-core--contract))))
      (should (eq (gnosis-core--contract) (gnosis-core--contract)))
      (should (= calls 1))
      (let ((gnosis-core-base-url "https://two.example"))
        (gnosis-core--contract)
        (should (= calls 2))))))

(ert-deftest gnosis-core-read-request-refreshes-one-stale-contract ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract)))
        (requests 0)
        (refreshes 0))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _)
                 (cl-incf requests)
                 (if (= requests 1)
                     (signal 'gnosis-core-protocol-error '("stale"))
                   '(:items [] :next :gnosis-json-null))))
              ((symbol-function 'gnosis-core-refresh-contract)
               (lambda ()
                 (cl-incf refreshes)
                 (setf (plist-get gnosis-core--contract-cache :document)
                       '(:refreshed t)))))
      (should
       (equal '(:items [] :next :gnosis-json-null)
              (gnosis-core--typed-read-request "get_nodes")))
      (should (= requests 2))
      (should (= refreshes 1)))))

(ert-deftest gnosis-core-read-request-does-not-loop-on-contract-failure ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract)))
        (requests 0)
        (refreshes 0))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _)
                 (cl-incf requests)
                 (signal 'gnosis-core-protocol-error '("drift"))))
              ((symbol-function 'gnosis-core-refresh-contract)
               (lambda () (cl-incf refreshes))))
      (should-error (gnosis-core--typed-read-request "get_nodes")
                    :type 'gnosis-core-protocol-error)
      (should (= requests 2))
      (should (= refreshes 1)))))

(ert-deftest gnosis-core-read-request-does-not-refresh-a-fresh-contract ()
  (let ((gnosis-core--contract-cache nil)
        (refreshes 0))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _)
                 (signal 'gnosis-core-protocol-error '("drift"))))
              ((symbol-function 'gnosis-core-refresh-contract)
               (lambda () (cl-incf refreshes))))
      (should-error (gnosis-core--typed-read-request "get_nodes")
                    :type 'gnosis-core-protocol-error)
      (should (= refreshes 0)))))

(ert-deftest gnosis-core-read-bindings-use-operation-identities ()
  (let (calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 (pcase (car arguments)
                   ("get_themata__id_" '(:thema_id "opaque/thema?id"))
                   ("get_nodes__id_"
                    (gnosis-test-core--node
                     :node_id "opaque/node?id"))
                   ("get_nodes"
                    `(:items [] :next ,gnosis-core--json-null))
                   ("get_nodes_catalog"
                    `(:generation "feed-3" :items []
                      :next ,gnosis-core--json-null))
                   (_ '(:projection t))))))
      (should (equal '(:projection t) (gnosis-core-dashboard 90)))
      (should (equal `(:items [] :next ,gnosis-core--json-null)
                     (gnosis-core-nodes "logic" "opaque/cursor" 25)))
      (should
       (equal
        `(:generation "feed-3" :items [] :next ,gnosis-core--json-null)
        (gnosis-core-node-catalog "document-a" "opaque/cursor" 5000)))
      (should (equal "opaque/node?id"
                     (plist-get (gnosis-core-node "opaque/node?id")
                                :node_id)))
      (should (equal '(:thema_id "opaque/thema?id")
                     (gnosis-core-thema "opaque/thema?id"))))
    (should
     (equal
      '(("get_dashboard" nil (("days" . 90)))
        ("get_nodes" nil
         (("after" . "opaque/cursor") ("limit" . 25) ("q" . "logic")))
        ("get_nodes_catalog" nil
         (("document_id" . "document-a")
          ("after" . "opaque/cursor")
          ("limit" . 5000)))
        ("get_nodes__id_" (("id" . "opaque/node?id")))
        ("get_themata__id_" (("id" . "opaque/thema?id"))))
      (nreverse calls)))))

(ert-deftest gnosis-core-read-bindings-omit-absent-query-values ()
  (let (calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 (pcase (car arguments)
                   ("get_nodes"
                    `(:items [] :next ,gnosis-core--json-null))
                   ("get_nodes_catalog"
                    `(:generation "feed-3" :items []
                      :next ,gnosis-core--json-null))))))
      (gnosis-core-dashboard)
      (gnosis-core-nodes nil nil nil)
      (gnosis-core-node-catalog nil nil nil))
    (should (equal '(("get_dashboard") ("get_nodes") ("get_nodes_catalog"))
                   (nreverse calls)))))

(ert-deftest gnosis-core-node-search-bindings-preserve-strict-query-shapes ()
  (let ((node (gnosis-test-core--node)) calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 `(:items [,node] :next "node-a"))))
      (gnosis-core-nodes-by-tag "λογική" nil 25)
      (gnosis-core-node-graph "opaque/node?id" :inbound "node-0" 25))
    (should
     (equal
      '(("get_nodes" nil (("limit" . 25) ("tag" . "λογική")))
        ("get_nodes__id__inbound" (("id" . "opaque/node?id"))
         (("after" . "node-0") ("limit" . 25))))
      (nreverse calls)))))

(ert-deftest gnosis-core-content-search-validates-hit-and-cursor-shape ()
  (let ((hit `(:document_id "document-a" :logical_path "notes/a.org"
               :revision 3
               :content_digest
               "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
               :file_node_id "node-a")))
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest arguments)
                 (should
                  (equal '("get_nodes_content-search" nil
                           (("q" . "literal") ("cursor" . "opaque")
                            ("limit" . 25)))
                         arguments))
                 `(:items [,hit] :next "next"))))
      (should (equal "next"
                     (plist-get
                      (gnosis-core-node-content-search
                       "literal" "opaque" 25)
                      :next))))
    (dolist (page
             (list
              `(:items [,(plist-put (copy-tree hit) :content_digest "bad")]
                :next ,gnosis-core--json-null)
              `(:items [,hit] :next 4)
              `(:items [] :next "next")
              `(:items [,hit ,hit] :next ,gnosis-core--json-null)))
      (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
                 (lambda (&rest _) page)))
        (should-error
         (gnosis-core-node-content-search "literal" nil 1)
         :type 'gnosis-core-protocol-error)))
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest _) `(:items [,hit] :next "opaque"))))
      (should-error (gnosis-core-node-content-search "literal" "opaque" 1)
                    :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-thema-archive-binding-correlates-authority ()
  (let (arguments)
    (cl-letf (((symbol-function 'gnosis-core-operation-id)
               (lambda () "op:archive"))
              ((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest value)
                 (setq arguments value)
                 '(:status "applied" :format_version 1
                   :operation_id "op:archive" :changed t :cursor "cursor"
                   :authoritative
                   (:kind "thema"
                    :value (:thema_id "thema-a" :revision 4 :archived t))))))
      (should (eq t (plist-get (gnosis-core-thema-set-archived
                                "thema-a" 3 t)
                               :archived))))
    (should
     (equal
      '("post_themata__id__archive" (("id" . "thema-a")) nil nil
        (("Idempotency-Key" . "op:archive") ("If-Match" . "\"3\"")))
      arguments))))

(ert-deftest gnosis-core-thema-review-state-validates-server-projection ()
  (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
             (lambda (&rest arguments)
               (should (equal '("get_themata__id__review-state"
                                (("id" . "thema-a")))
                              arguments))
               `(:thema_id "thema-a"
                 :state (:state_revision 2 :review_count 0
                         :suspended ,gnosis-core--json-false)))))
    (should (equal 0 (plist-get
                      (plist-get (gnosis-core-thema-review-state "thema-a")
                                 :state)
                      :review_count))))
  (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
             (lambda (&rest _)
               '(:thema_id "other"
                 :state (:state_revision 2 :review_count 0 :suspended nil)))))
    (should-error (gnosis-core-thema-review-state "thema-a")
                  :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-node-binding-rejects-wrong-response-identity ()
  (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
             (lambda (&rest _)
               (gnosis-test-core--node :node_id "other"))))
    (should-error (gnosis-core-node "expected")
                  :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-node-binding-rejects-invalid-node-metadata ()
  (dolist (node
           (list
            (gnosis-test-core--node :document_revision 0)
            (gnosis-test-core--node :content_sha256 "not-a-digest")
            (gnosis-test-core--node :tags ["same" "same"])
            (gnosis-test-core--node :start_byte 4)
            (gnosis-test-core--node :start_byte 4 :end_byte 4)
            (gnosis-test-core--node :active nil)))
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest _) node)))
      (should-error (gnosis-core-node "node-a")
                    :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-node-page-validates-pagination-contract ()
  (let ((first (gnosis-test-core--node :node_id "node-b"))
        (second (gnosis-test-core--node :node_id "node-c")))
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest _)
                 `(:items [,first ,second] :next "node-c"))))
      (should (equal "node-c"
                     (plist-get (gnosis-core-nodes nil "node-a" 2) :next))))
    (dolist (case
             (list
              (list `(:items [,first ,second] :next "node-c")
                    "node-a" 1)
              (list `(:items [,second ,first] :next "node-b")
                    "node-a" 2)
              (list `(:items [,first] :next "wrong")
                    "node-a" 1)
              (list `(:items [,first] :next ,gnosis-core--json-null)
                    "node-a" 1)
              (list `(:items [,first] :next "node-b")
                    "node-b" 1)))
      (let ((page (nth 0 case))
            (after (nth 1 case))
            (limit (nth 2 case)))
        (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
                   (lambda (&rest _) page)))
          (should-error
           (gnosis-core-nodes nil after limit)
           :type 'gnosis-core-protocol-error))))))

(ert-deftest gnosis-core-node-catalog-validates-compact-pagination ()
  (let ((first (gnosis-test-core--catalog-node :node_id "node-b"))
        (second (gnosis-test-core--catalog-node :node_id "node-c")))
    (dolist (next (list "node-c" gnosis-core--json-null))
      (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
                 (lambda (&rest _)
                   `(:generation "feed-3"
                     :items [,first ,second]
                     :next ,next))))
        (should
         (equal "feed-3"
                (plist-get
                 (gnosis-core-node-catalog nil "node-a" 2)
                 :generation)))))
    (dolist
        (page
         (list
          `(:generation "" :items [,first] :next ,gnosis-core--json-null)
          `(:generation ,(make-string 513 ?x)
            :items [,first] :next ,gnosis-core--json-null)
          `(:generation "feed-3" :items [,second ,first] :next "node-b")
          `(:generation "feed-3" :items [,first] :next "wrong")
          `(:generation "feed-3"
            :items [,(plist-put (copy-tree first) :document_id "other")]
            :next ,gnosis-core--json-null)
          `(:generation "feed-3"
            :items [,(plist-put (copy-tree first) :title_path [])]
            :next ,gnosis-core--json-null)))
      (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
                 (lambda (&rest _) page)))
        (should-error
         (gnosis-core-node-catalog "document-a" "node-a" 2)
         :type 'gnosis-core-protocol-error)))))

(ert-deftest gnosis-core-thema-catalog-validates-compact-pagination ()
  (let ((first (gnosis-test-core--catalog-thema :thema_id "thema-b"))
        (second (gnosis-test-core--catalog-thema :thema_id "thema-c")))
    (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
               (lambda (&rest arguments)
                 (should
                  (equal
                   '("get_themata_catalog" nil
                     (("q" . "logic") ("after" . "thema-a") ("limit" . 2)))
                   arguments))
                 `(:generation "feed-3" :items [,first ,second]
                   :next "thema-c"))))
      (should
       (equal "feed-3"
              (plist-get
               (gnosis-core-thema-catalog "logic" "thema-a" 2)
               :generation))))
    (dolist
        (page
         (list
          `(:generation "" :items [,first] :next ,gnosis-core--json-null)
          `(:generation "feed-3" :items [,second ,first] :next "thema-b")
          `(:generation "feed-3" :items [,first] :next "wrong")
          `(:generation "feed-3"
            :items [,(plist-put (copy-tree first) :state_revision -1)]
            :next ,gnosis-core--json-null)
          `(:generation "feed-3"
            :items [,(plist-put (copy-tree first) :suspended nil)]
            :next ,gnosis-core--json-null)))
      (cl-letf (((symbol-function 'gnosis-core--typed-read-request)
                 (lambda (&rest _) page)))
        (should-error
         (gnosis-core-thema-catalog nil "thema-a" 2)
         :type 'gnosis-core-protocol-error)))))

(defun gnosis-test-core--cursor (server-id position token)
  "Return a canonical replica cursor for SERVER-ID, POSITION, and TOKEN."
  (base64url-encode-string
   (json-serialize `(:version 1 :server_id ,server-id
                     :position ,position :token ,token))
   t))

(ert-deftest gnosis-core-decodes-canonical-generation-cursor ()
  (let* ((cursor (gnosis-test-core--cursor "server-a" 7 "token-a"))
         (decoded (gnosis-core--decode-cursor cursor)))
    (should (equal "server-a" (plist-get decoded :server_id)))
    (should (= 7 (plist-get decoded :position)))
    (should (equal "token-a" (plist-get decoded :token))))
  (dolist (invalid '(nil "" "+" "e30="))
    (should-not (gnosis-core--decode-cursor invalid))))

(ert-deftest gnosis-core-thema-suspension-correlates-authoritative-state ()
  (let (calls)
    (cl-letf
        (((symbol-function 'gnosis-core--typed-request)
          (lambda (&rest arguments)
            (push arguments calls)
            `(:status "applied" :format_version 1
              :operation_id "suspend:1" :changed t
              :authoritative
              (:kind "review_state"
               :value
               (:thema_id "thema-a"
                :state (:state_revision 1 :suspended t)))
              :cursor "feed-4"))))
      (should
       (equal
        '(:thema_id "thema-a" :state (:state_revision 1 :suspended t))
        (gnosis-core-thema-set-suspended "thema-a" 0 t "suspend:1"))))
    (should
     (equal
      '(("put_themata__id__suspension" (("id" . "thema-a")) nil
         (:suspended t)
         (("Idempotency-Key" . "suspend:1") ("If-Match" . "\"0\""))))
      (nreverse calls)))))

(ert-deftest gnosis-core-thema-suspension-rejects-stale-or-uncorrelated-results ()
  (let ((result
         '(:status "rejected" :format_version 1
           :operation_id "suspend:1"
           :rejection (:reason "revision_conflict"
                       :expected_revision 0 :current_revision 1)
           :cursor "feed-4")))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) result)))
      (should-error
       (gnosis-core-thema-set-suspended "thema-a" 0 nil "suspend:1")
       :type 'gnosis-core-conflict)
      (setf (plist-get (plist-get result :rejection) :expected_revision) 2)
      (should-error
       (gnosis-core-thema-set-suspended "thema-a" 0 nil "suspend:1")
       :type 'gnosis-core-mutation-uncertain)))
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               `(:status "applied" :format_version 1
                 :operation_id "suspend:1" :changed t
                 :authoritative
                 (:kind "review_state"
                  :value
                  (:thema_id "other"
                   :state
                   (:state_revision 1 :suspended
                    ,gnosis-core--json-false)))
                 :cursor "feed-4"))))
    (should-error
     (gnosis-core-thema-set-suspended "thema-a" 0 nil "suspend:1")
     :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-core-thema-write-bindings-include-idempotency-headers ()
  (let ((request '(:kind "basic" :keimenon "Q" :answer ["A"]))
        (created '(:thema_id "created" :revision 1))
        (updated '(:thema_id "opaque/id" :revision 7))
        calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 (if (equal (car arguments) "put_themata__id_")
                     `(:status "applied" :format_version 1
                       :operation_id "op:update"
                       :changed ,gnosis-core--json-false
                       :authoritative (:kind "thema" :value ,updated)
                       :cursor "cursor:update")
                   `(:status "applied" :format_version 1
                     :operation_id "op:create" :changed t
                     :authoritative (:kind "thema" :value ,created)
                     :cursor "cursor:create")))))
      (should (eq created
                     (gnosis-core-thema-create "op:create" request)))
      (should (eq updated
                     (gnosis-core-thema-update
                      "opaque/id" "op:update" 7 request))))
    (should
     (equal
      `(("post_themata" nil nil ,request
         (("Idempotency-Key" . "op:create")))
        ("put_themata__id_" (("id" . "opaque/id")) nil ,request
         (("Idempotency-Key" . "op:update")
          ("If-Match" . "\"7\""))))
      (nreverse calls)))))

(ert-deftest gnosis-core-thema-bindings-reject-wrong-response-identity ()
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               '(:status "applied" :format_version 1
                 :operation_id "op:update" :changed t
                 :authoritative
                 (:kind "thema" :value (:thema_id "different" :revision 2))
                 :cursor "cursor"))))
    (should-error (gnosis-core-thema "expected")
                  :type 'gnosis-core-protocol-error)
    (should-error
     (gnosis-core-thema-update
      "expected" "op:update" 1
      '(:kind "basic" :keimenon "Q" :answer ["A"]))
     :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-core-thema-mutation-rejects-untrusted-envelopes ()
  (let ((request '(:kind "basic" :keimenon "Q" :answer ["A"])))
    (dolist
        (result
         '((:status "applied" :format_version 1 :operation_id "wrong"
            :changed t
            :authoritative (:kind "thema"
                            :value (:thema_id "created" :revision 1))
            :cursor "cursor")
           (:status "applied" :format_version 1 :operation_id "op:create"
            :changed t
            :authoritative (:kind "settings"
                            :value (:thema_id "created" :revision 1))
            :cursor "cursor")
           (:status "applied" :format_version 1 :operation_id "op:create"
            :changed t :authoritative (:kind "thema") :cursor "cursor")
           (:status "applied" :format_version 1 :operation_id "op:create"
            :changed "true"
            :authoritative (:kind "thema"
                            :value (:thema_id "created" :revision 1))
            :cursor "cursor")))
      (cl-letf (((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest _) result)))
        (should-error (gnosis-core-thema-create "op:create" request)
                      :type 'gnosis-core-mutation-uncertain)))))

(ert-deftest gnosis-core-thema-mutation-signals-correlated-rejection ()
  (let ((expected-revision 7))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _)
                 `(:status "rejected" :format_version 1
                   :operation_id "op:update"
                   :rejection (:reason "revision_conflict"
                               :expected_revision ,expected-revision
                               :current_revision 8)
                   :cursor "cursor"))))
      (should-error
       (gnosis-core-thema-update
        "opaque/id" "op:update" 7
        '(:kind "basic" :keimenon "Q" :answer ["A"]))
       :type 'gnosis-core-conflict)
      (setq expected-revision 6)
      (should-error
       (gnosis-core-thema-update
        "opaque/id" "op:update" 7
        '(:kind "basic" :keimenon "Q" :answer ["A"]))
       :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-thema-write-view-follows-request-schema ()
  (let* ((contract (gnosis-test-core--thema-contract))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842" :document contract))
         (projection
          '(:thema_id "opaque" :revision 7 :kind "basic" :keimenon "Q"
            :answer ["A"] :concepts [(:node_id "node/id"
                                      :relation "primary")])))
    (should
     (equal '(:kind "basic" :keimenon "Q" :answer ["A"]
              :concepts [(:node_id "node/id" :relation "primary")])
            (gnosis-core-thema-write-view projection)))
    (setf (plist-get (aref (plist-get projection :concepts) 0) :relation)
          "invented")
    (should-error (gnosis-core-thema-write-view projection)
                  :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-mutation-rejects-incoherent-response-before-transport ()
  (let* ((contract (copy-tree (gnosis-test-core--contract) t))
         (write (plist-get
                 (plist-get (plist-get contract :components) :schemas)
                 :WidgetWrite))
         (response (plist-get
                    (plist-get (plist-get contract :components) :schemas)
                    :Widget))
         (gnosis-core--contract-cache
          (list :base "http://127.0.0.1:7842" :document contract))
         (calls 0))
    (setf (plist-get (plist-get (plist-get write :properties) :value) :type)
          ["string" "null"])
    (setf (plist-get response :required) ["id" "value"])
    (setf (plist-get (plist-get response :properties) :value)
          '(:type "string"))
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest _) (cl-incf calls))))
      (should-error
       (gnosis-core--typed-request
        "put_widget" '(("id" . "widget")) nil
        `(:value ,gnosis-core--json-null) '(("If-Match" . "\"3\"")))
       :type 'gnosis-core-protocol-error))
    (should (= calls 0))))

(ert-deftest gnosis-core-shared-response-check-ignores-array-bodies ()
  (should
   (gnosis-core--shared-response-fields-valid-p
    ["value"] '(:type "object" :properties (:value (:type "string")))
    (gnosis-test-core--contract))))

(ert-deftest gnosis-core-shared-response-check-allows-normalized-objects ()
  (should
   (gnosis-core--shared-response-fields-valid-p
    '(:source (:kind "due"))
    '(:type "object"
      :properties
      (:source (:type "object" :required ["kind" "tags"]
                :properties (:kind (:type "string")
                             :tags (:type "array")))))
    (gnosis-test-core--contract))))

(ert-deftest gnosis-core-schema-shape-rejects-reference-cycles ()
  (let ((contract '(:components (:schemas (:Loop (:$ref
                                                   "#/components/schemas/Loop"))))))
    (should-not
     (gnosis-core--schema-shape-valid-p
      "value" '(:$ref "#/components/schemas/Loop") contract))))

(ert-deftest gnosis-core-mutation-does-not-hide-uncertain-outcome ()
  (let ((gnosis-core--contract-cache
         (list :base "http://127.0.0.1:7842"
               :document (gnosis-test-core--contract)))
        (calls 0))
    (cl-letf (((symbol-function 'gnosis-core--request)
               (lambda (&rest _)
                 (cl-incf calls)
                 (signal 'gnosis-core-timeout '("timeout")))))
      (should-error
       (gnosis-core--typed-request
        "put_widget" '(("id" . "widget")) nil '(:value "text")
        '(("If-Match" . "\"3\"")))
       :type 'gnosis-core-mutation-uncertain))
    (should (= calls 1))))

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
  (dolist (status '(400 413 422))
    (cl-letf (((symbol-function 'gnosis-core--token)
               (lambda () (make-string 40 ?x)))
              ((symbol-function 'gnosis-core--retrieve-bounded)
               (lambda (&rest _)
                 (gnosis-test-core--http-buffer
                  status
                  "{\"error\":{\"code\":\"future_error\",\"message\":\"bad request\",\"meta\":{\"new\":true}}}"))))
      (let ((condition
             (should-error
              (gnosis-core--perform-request "GET" "/v1/test" nil 200 nil t)
              :type 'gnosis-core-validation-error)))
        (should (eq 'gnosis-core-http-rejection (car condition)))
        (should (equal '("bad request" "future_error") (cdr condition)))))))

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

(ert-deftest gnosis-core-bounds-headers-in-the-process-buffer ()
  (let ((response-buffer (generate-new-buffer " *gnosis-response*"))
        (gnosis-core--response-header-limit 64)
        process)
    (unwind-protect
        (with-temp-buffer
          (insert (make-string 128 ?x))
          (setq process
                (make-pipe-process
                 :name "gnosis-header-buffer-test"
                 :buffer response-buffer
                 :noquery t))
          (let ((state (vector nil nil nil nil)))
            (process-put process 'gnosis-core--state state)
            (process-put process 'gnosis-core--limit 1024)
            (process-put process 'gnosis-core--headers-verified nil)
            (process-put process 'gnosis-core--original-filter
                         (lambda (_process _data)))
            (gnosis-core--bounded-process-filter
             process "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            (should-not (aref state 2))
            (should (process-live-p process))))
      (when (process-live-p process) (delete-process process))
      (when (buffer-live-p response-buffer) (kill-buffer response-buffer)))))

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

(ert-deftest gnosis-core-sync-read-bindings-use-operation-identities ()
  (let (calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (when (equal "get_sync_changes" (car arguments))
                   (should (= gnosis-core--replica-response-limit
                              gnosis-core--response-limit-override)))
                 (push arguments calls)
                 '(:ok t))))
      (gnosis-core-sync-identity)
      (gnosis-core-sync-state)
      (gnosis-core-sync-snapshot "snapshot/cursor")
      (gnosis-core-sync-changes "replica/cursor"))
    (should
     (equal
      '(("get_sync_identity")
        ("get_sync_state")
        ("get_sync_snapshot" nil (("cursor" . "snapshot/cursor")))
        ("get_sync_changes" nil (("cursor" . "replica/cursor"))))
      (nreverse calls)))))

(ert-deftest gnosis-core-blob-binding-verifies-digest ()
  (let* ((bytes (encode-coding-string "* Note\n" 'utf-8 t))
         (digest (secure-hash 'sha256 bytes)))
    (cl-letf (((symbol-function 'gnosis-core--typed-binary-request)
               (lambda (&rest _) bytes)))
      (should (equal bytes (gnosis-core-blob digest))))
    (cl-letf (((symbol-function 'gnosis-core--typed-binary-request)
               (lambda (&rest _) "wrong")))
      (should-error (gnosis-core-blob digest)
                    :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-document-revision-binds-identity-revision-and-digest ()
  (let* ((bytes (encode-coding-string "* Exact\n" 'utf-8 t))
         (digest (secure-hash 'sha256 bytes))
         call)
    (cl-letf (((symbol-function 'gnosis-core--typed-binary-request)
               (lambda (&rest arguments)
                 (setq call arguments)
                 bytes)))
      (should (equal bytes
                     (gnosis-core-document-revision
                      "document/id" 7 digest)))
      (should
       (equal
        '("get_notes__id__revisions__revision_"
          (("id" . "document/id") ("revision" . 7))
          nil "\"7\"")
        call)))
    (cl-letf (((symbol-function 'gnosis-core--typed-binary-request)
               (lambda (&rest _) "wrong")))
      (should-error
       (gnosis-core-document-revision "document/id" 7 digest)
       :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-document-revision-requires-matching-etag ()
  (cl-letf (((symbol-function 'gnosis-core--retrieve-bounded)
             (lambda (&rest _)
               (gnosis-test-core--http-buffer
                200 "body" "text/org" '(("ETag" . "\"7\""))))))
    (should
     (equal "body"
            (gnosis-core--perform-request
             "GET" "/v1/notes/doc/revisions/7" nil 200 t nil nil "\"7\"")))
    (should-error
     (gnosis-core--perform-request
      "GET" "/v1/notes/doc/revisions/8" nil 200 t nil nil "\"8\"")
     :type 'gnosis-core-protocol-error))
  (cl-letf (((symbol-function 'gnosis-core--retrieve-bounded)
             (lambda (&rest _)
               (gnosis-test-core--http-buffer
                200 "ETag: \"7\"\nbody" "text/org"))))
    (should-error
     (gnosis-core--perform-request
      "GET" "/v1/notes/doc/revisions/7" nil 200 t nil nil "\"7\"")
     :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-core-document-mutations-use-exact-bindings ()
  (let ((bytes (encode-coding-string "* Γνῶσις\n" 'utf-8 t))
        calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 (let ((operation (car arguments)))
                   (if (equal operation "post_document-mutations")
                       `(:status "applied" :format_version 1
                         :operation_id "op:create" :changed t
                         :authoritative
                         (:document_id "doc" :logical_path "note.org"
                          :revision 1 :content_sha256
                          ,(secure-hash 'sha256 bytes) :active t)
                         :cursor "hint")
                     '(:status "rejected" :format_version 1
                       :operation_id "op:next"
                       :rejection (:reason "revision_conflict"
                                   :expected_revision 1 :current_revision 2)
                       :cursor "hint"))))))
      (gnosis-core-document-create "op:create" "note.org" bytes)
      (gnosis-core-document-replace "doc" "op:next" 1 "note.org" bytes)
      (gnosis-core-document-rename
       "doc" "op:next" 1 "moved.org" (secure-hash 'sha256 bytes))
      (gnosis-core-document-tombstone "doc" "op:next" 1 "note.org"))
    (should
     (equal
      `(("post_document-mutations" nil nil
         (:logical_path "note.org"
          :content_type "text/org; charset=utf-8"
          :source "* Γνῶσις\n")
         (("Idempotency-Key" . "op:create")))
        ("put_document-mutations__id_" (("id" . "doc")) nil
         (:logical_path "note.org"
          :content_type "text/org; charset=utf-8"
          :source "* Γνῶσις\n")
         (("Idempotency-Key" . "op:next") ("If-Match" . "\"1\"")))
        ("post_document-mutations__id__rename" (("id" . "doc")) nil
         (:logical_path "moved.org")
         (("Idempotency-Key" . "op:next") ("If-Match" . "\"1\"")))
        ("delete_document-mutations__id_" (("id" . "doc")) nil nil
         (("Idempotency-Key" . "op:next") ("If-Match" . "\"1\""))))
      (nreverse calls)))))

(ert-deftest gnosis-core-document-content-rejects-malformed-utf-8-before-transport ()
  (let (called)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _)
                 (setq called t))))
      (dolist (call (list (lambda ()
                            (gnosis-core-document-create
                             "op:create" "note.org" (unibyte-string #xff)))
                          (lambda ()
                            (gnosis-core-document-replace
                             "doc" "op:replace" 1 "note.org"
                             (unibyte-string #xff)))))
        (should-error (funcall call) :type 'gnosis-core-validation-error)))
    (should-not called)))

(ert-deftest gnosis-core-document-mutation-rejects-wrong-operation-id ()
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               '(:status "applied" :format_version 1
                 :operation_id "different" :changed t
                 :authoritative
                 (:document_id "doc" :logical_path "note.org"
                  :revision 1 :content_sha256 nil :active t)
                 :cursor "hint"))))
    (should-error
     (gnosis-core-document-create "expected" "note.org" "body")
     :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-core-document-mutation-validates-rejected-result ()
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               '(:status "rejected" :format_version 1
                 :operation_id "expected"
                 :rejection (:reason "revision_conflict"
                             :expected_revision 8 :current_revision 10)
                 :cursor "hint"))))
    (should-error
     (gnosis-core-document-replace "doc" "expected" 7 "note.org" "body")
     :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-core-document-mutation-rejects-unknown-status ()
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               '(:status "maybe" :format_version 1
                 :operation_id "expected" :cursor "hint"))))
    (should-error
     (gnosis-core-document-create "expected" "note.org" "body")
     :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-core-document-mutation-accepts-valid-nullable-rejection ()
  (cl-letf (((symbol-function 'gnosis-core--typed-request)
             (lambda (&rest _)
               `(:status "rejected" :format_version 1
                 :operation_id "expected"
                 :rejection (:reason "revision_conflict"
                             :expected_revision 7
                             :current_revision ,gnosis-core--json-null)
                 :cursor "hint"))))
    (should (equal "rejected"
                   (plist-get
                    (gnosis-core-document-replace
                     "doc" "expected" 7 "note.org" "body")
                    :status)))))

(ert-deftest gnosis-core-document-mutation-rejects-impossible-heads ()
  (let (result)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) result)))
      (setq result
            '(:status "applied" :format_version 1 :operation_id "create"
              :authoritative
              (:document_id "doc" :logical_path "note.org" :revision 9
               :content_sha256
               "230d8358dc8e8890b4c58c5fbe2a2fca89b394e1003ede3c7e43cc9f0bb9a250"
               :active t)
              :cursor "hint"))
      (should-error (gnosis-core-document-create "create" "note.org" "body")
                    :type 'gnosis-core-mutation-uncertain)
      (setq result
            '(:status "applied" :format_version 1 :operation_id "rename"
              :authoritative
              (:document_id "doc" :logical_path "moved.org" :revision 2
               :content_sha256 "wrong" :active t)
              :cursor "hint"))
      (should-error
       (gnosis-core-document-rename "doc" "rename" 1 "moved.org" "digest")
       :type 'gnosis-core-mutation-uncertain)
      (setq result
            `(:status "applied" :format_version 1 :operation_id "delete"
              :authoritative
              (:document_id "doc" :logical_path "other.org" :revision 2
               :content_sha256 ,gnosis-core--json-null
               :active ,gnosis-core--json-false)
              :cursor "hint"))
      (should-error
       (gnosis-core-document-tombstone "doc" "delete" 1 "note.org")
       :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-document-mutation-validates-changed-revision ()
  (let ((head '(:document_id "doc" :logical_path "note.org"
                :revision 7
                :content_sha256
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                :active t)))
    (dolist (changed (list t gnosis-core--json-false))
      (let ((result `(:status "applied" :format_version 1
                      :operation_id "op" :changed ,changed
                      :authoritative ,(copy-tree head) :cursor "hint")))
        (setf (plist-get (plist-get result :authoritative) :revision)
              (if (eq changed t) 8 7))
        (should
         (eq result
             (gnosis-core--document-mutation-result
              result "op" "doc" 7 "note.org"
              "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
              t)))))
    (dolist (changed '(nil "true" 1))
      (should-error
       (gnosis-core--document-mutation-result
        `(:status "applied" :format_version 1 :operation_id "op"
          :changed ,changed :authoritative ,head :cursor "hint")
        "op" "doc" 7 "note.org"
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" t)
       :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-document-create-cannot-be-an-unchanged-replay ()
  (should-error
   (gnosis-core--document-mutation-result
    `(:status "applied" :format_version 1 :operation_id "op"
      :changed ,gnosis-core--json-false
      :authoritative (:document_id "doc" :logical_path "note.org"
                      :revision 1
                      :content_sha256
                      "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                      :active t)
      :cursor "hint")
    "op" "doc" :create "note.org"
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" t)
   :type 'gnosis-core-mutation-uncertain))

(ert-deftest gnosis-core-review-bindings-use-live-operation-identities ()
  (let ((source `(:kind "due" :tags [] :exclude_tags []
                  :topic_node_id ,gnosis-core--json-null
                  :topic_forward_depth ,gnosis-core--json-null
                  :topic_backward_depth ,gnosis-core--json-null
                  :manual_ids []))
        calls)
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments)
                 (push arguments calls)
                 (pcase (car arguments)
                   ("get_review_sessions_active"
                    `(:session ,gnosis-core--json-null))
                   ((or "post_review_sessions"
                        "post_review_sessions__id__resume")
                    `(:session_id "session" :mode "online" :status "active"
                      :source ,source :target_count 12
                      :current_position 0 :queue_length 1 :started_at_us 1))
                   ("post_review_sessions__id__quit"
                    `(:session_id "session" :mode "online" :status "quit"
                      :source ,source :target_count 12
                      :current_position 0 :queue_length 1 :started_at_us 1))
                   ("post_review_sessions__id__abort"
                    `(:session_id "session" :mode "online" :status "aborted"
                      :source ,source :target_count 12
                      :current_position 0 :queue_length 1 :started_at_us 1))
                   ("post_review_sessions__id__attempts"
                    `(:session
                      (:session_id "session" :mode "online" :status "active"
                       :source ,source :target_count 12
                       :current_position 0 :queue_length 1 :started_at_us 1)
                      :attempt
                      (:attempt_id "attempt" :session_id "session" :thema_id "thema"
                       :status "pending")
                      :start_position 0
                      :skipped []))
                   ("post_review_attempts__id__answer"
                    (list :attempt_id "attempt" :status "graded"
                          :response (nth 3 arguments)))
                   ("post_review_attempts__id__cloze-check"
                   `(:matches [t] :all_entered_correct t
                      :complete ,gnosis-core--json-false
                      :question "one and (...)"
                      :cloze_spans
                      [(:start_byte 0 :end_byte 3 :state "correct")
                       (:start_byte 8 :end_byte 13 :state "concealed")]
                      :next_input_script "greek"))
                   ("get_review_attempt_state"
                    '(:version 1 :attempt_revision 3
                      :attempt (:attempt_id "attempt")))
                   ("post_review_attempts__id__commit"
                    '(:attempt_id "attempt" :event_id "event"))))))
      (gnosis-core-review-session-create source 12)
      (gnosis-core-review-session-active)
      (gnosis-core-review-session-resume "session")
      (gnosis-core-review-session-quit "session")
      (gnosis-core-review-session-abort "session")
      (gnosis-core-review-attempt-begin "session")
      (gnosis-core-review-attempt-answer
       "attempt" '(:kind "texts" :value ["one" "two"]))
      (gnosis-core-review-attempt-check-cloze "attempt" ["one"])
      (gnosis-core-review-attempt-state "attempt")
      (gnosis-core-review-attempt-commit "attempt"))
    (should
     (equal
      `(("post_review_sessions" nil nil
         (:source ,source :target_count 12))
        ("get_review_sessions_active")
        ("post_review_sessions__id__resume" (("id" . "session")))
        ("post_review_sessions__id__quit" (("id" . "session")))
        ("post_review_sessions__id__abort" (("id" . "session")))
        ("post_review_sessions__id__attempts" (("id" . "session")))
        ("post_review_attempts__id__answer" (("id" . "attempt")) nil
         (:kind "texts" :value ["one" "two"]))
        ("post_review_attempts__id__cloze-check" (("id" . "attempt")) nil
         (:inputs ["one"]))
        ("get_review_attempt_state" (("id" . "attempt")))
        ("post_review_attempts__id__commit" (("id" . "attempt"))))
      (nreverse calls)))))

(ert-deftest gnosis-core-review-commit-validates-correlated-receipt ()
  (let ((valid '(:attempt_id "attempt" :event_id "event")))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) valid)))
      (should (eq valid (gnosis-core-review-attempt-commit "attempt")))))
  (dolist (result `((:attempt_id "other-attempt" :event_id "event")
                    (:attempt_id "attempt" :event_id "")
                    (:attempt_id "attempt" :event_id "bad\0id")
                    (:attempt_id "attempt" :event_id ,(make-string 129 ?x))))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) result)))
      (should-error (gnosis-core-review-attempt-commit "attempt")
                    :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-review-cloze-check-rejects-uncorrelated-response ()
  (let ((valid `(:matches [t] :all_entered_correct t
                 :complete ,gnosis-core--json-false
                 :question "one and (...)"
                 :cloze_spans
                 [(:start_byte 0 :end_byte 3 :state "correct")
                  (:start_byte 8 :end_byte 13 :state "concealed")]
                 :next_input_script "greek")))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) valid)))
      (should (eq valid
                  (gnosis-core-review-attempt-check-cloze
                   "attempt" ["one"])))))
  (dolist (response
           `((:matches [t] :all_entered_correct ,gnosis-core--json-false
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "correct")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t :complete t
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "correct")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question ""
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "correct")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "correct")]
              :next_input_script "Greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans 1
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "incorrect")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "unanswered")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans [(:start_byte 0 :end_byte 1 :state "future")]
              :next_input_script "greek")
             (:matches [t] :all_entered_correct t
              :complete ,gnosis-core--json-false
              :question "Question"
              :cloze_spans
              [(:start_byte 1 :end_byte 3 :state "correct")
               (:start_byte 2 :end_byte 4 :state "concealed")]
              :next_input_script "greek")))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _) response)))
      (should-error
       (gnosis-core-review-attempt-check-cloze "attempt" ["one"])
       :type 'gnosis-core-protocol-error))))

(ert-deftest gnosis-core-review-cloze-spans-reject-mid-codepoint-boundaries ()
  (dolist (spans '([(:start_byte 0 :end_byte 1 :state "concealed")]
                    [(:start_byte 1 :end_byte 2 :state "concealed")]))
    (should-not
     (gnosis-core--review-cloze-spans-valid-p spans "μx"))))

(ert-deftest gnosis-core-review-session-rejects-invalid-status-position ()
  (let ((invalid '(:session_id "session" :mode "online" :status "active"
                   :current_position 1 :queue_length 1 :started_at_us 1)))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _arguments) `(:session ,invalid))))
      (should-error (gnosis-core-review-session-active)
                    :type 'gnosis-core-protocol-error))
    (should-not
     (gnosis-core--review-session-valid-p
      '(:session_id "session" :mode "offline" :status "active"
        :current_position 0 :queue_length 1 :started_at_us 1)
      "active"))
    (should
     (gnosis-core--review-session-valid-p
      '(:session_id "session" :mode "online" :status "completed"
        :current_position 1 :queue_length 1 :started_at_us 1)
      "completed"))))

(ert-deftest gnosis-core-review-begin-accepts-graded-current-attempt ()
  (let* ((session '(:session_id "session" :mode "online" :status "active"
                    :current_position 0 :queue_length 1 :started_at_us 1))
         (attempt '(:attempt_id "attempt" :session_id "session" :thema_id "thema"
                    :status "graded"))
         (result (list :session session :attempt attempt
                       :start_position 0 :skipped [])))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _arguments) result)))
      (should (eq result (gnosis-core-review-attempt-begin "session"))))))

(ert-deftest gnosis-core-review-begin-rejects-uncorrelated-attempt ()
  (let ((session '(:session_id "session" :mode "online" :status "active"
                   :current_position 0 :queue_length 1 :started_at_us 1)))
    (dolist (result
             (list
              (list :session session :attempt
                    '(:attempt_id "attempt" :session_id "other"
                      :status "pending")
                    :start_position 0 :skipped [])
              (list :session session :attempt
                    '(:attempt_id "attempt" :session_id "session"
                      :status "committed")
                    :start_position 0 :skipped [])
              (list :session session :attempt gnosis-core--json-null
                    :start_position 0 :skipped [])
              (list :session session :attempt
                    '(:attempt_id "attempt" :session_id "session"
                      :status "pending")
                    :start_position 0
                    :skipped [(:position 0 :thema_id "thema"
                               :reason "unknown")])))
      (let ((gnosis-test-core--begin-response result))
        (cl-letf (((symbol-function 'gnosis-core--typed-request)
                   #'gnosis-test-core--return-begin-response))
          (should-error (gnosis-core-review-attempt-begin "session")
                        :type 'gnosis-core-mutation-uncertain))))))

(ert-deftest gnosis-core-review-begin-accepts-completed-skip-envelope ()
  (let* ((session '(:session_id "session" :mode "online" :status "completed"
                    :current_position 1 :queue_length 1 :started_at_us 1))
         (result
          (list :session session :attempt gnosis-core--json-null
                :start_position 0
                :skipped [(:position 0 :thema_id "thema"
                           :reason "unsafe_presentation")])))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _arguments) result)))
      (should (eq result (gnosis-core-review-attempt-begin "session"))))))

(ert-deftest gnosis-core-review-begin-rejects-invalid-skip-order-or-position ()
  (let ((session '(:session_id "session" :mode "online" :status "active"
                   :current_position 2 :queue_length 3 :started_at_us 1))
        (attempt '(:attempt_id "attempt" :session_id "session" :thema_id "thema"
                   :status "pending")))
    (dolist (skipped '([(:position 1 :thema_id "b" :reason "suspended")
                        (:position 0 :thema_id "a" :reason "archived")]
                       [(:position 0 :thema_id "a" :reason "archived")
                        (:position 0 :thema_id "b" :reason "suspended")]
                       [(:position 2 :thema_id "a" :reason "archived")]))
      (cl-letf (((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest _)
                   (list :session session :attempt attempt
                         :start_position 0 :skipped skipped))))
        (should-error (gnosis-core-review-attempt-begin "session")
                      :type 'gnosis-core-mutation-uncertain)))))

(ert-deftest gnosis-core-review-begin-correlates-the-complete-skipped-range ()
  (let ((session '(:session_id "session" :mode "online" :status "active"
                   :current_position 3 :queue_length 4 :started_at_us 1))
        (attempt '(:attempt_id "attempt" :session_id "session" :thema_id "thema"
                   :status "pending")))
    (dolist (case '((0 [(:position 0 :thema_id "a" :reason "archived")])
                    (0 [(:position 0 :thema_id "a" :reason "archived")
                        (:position 2 :thema_id "c" :reason "suspended")])
                    (2 [(:position 1 :thema_id "b" :reason "archived")])
                    (-1 [(:position 0 :thema_id "a" :reason "archived")])
                    (4 [])
                    (nil [])))
      (cl-letf (((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest _)
                   (list :session session :attempt attempt
                         :start_position (car case) :skipped (cadr case)))))
        (should-error (gnosis-core-review-attempt-begin "session")
                      :type 'gnosis-core-mutation-uncertain)))
    (dolist (case '((3 [])
                    (2 [(:position 2 :thema_id "c" :reason "suspended")])
                    (1 [(:position 1 :thema_id "b" :reason "archived")
                        (:position 2 :thema_id "c" :reason "suspended")])
                    (0 [(:position 0 :thema_id "a" :reason "archived")
                        (:position 1 :thema_id "b" :reason "archived")
                        (:position 2 :thema_id "c" :reason "suspended")])))
      (let ((result (list :session session :attempt attempt
                          :start_position (car case) :skipped (cadr case))))
        (cl-letf (((symbol-function 'gnosis-core--typed-request)
                   (lambda (&rest _) result)))
          (should (eq result (gnosis-core-review-attempt-begin "session"))))))))

(ert-deftest gnosis-core-review-begin-bounds-uint32-positions ()
  (let ((attempt '(:attempt_id "attempt" :session_id "session"
                   :thema_id "thema" :status "pending")))
    (let* ((session `(:session_id "session" :mode "online" :status "completed"
                      :current_position ,gnosis-core--uint32-max
                      :queue_length ,gnosis-core--uint32-max :started_at_us 1))
           (result (list :session session :attempt gnosis-core--json-null
                         :start_position gnosis-core--uint32-max :skipped [])))
      (cl-letf (((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest _) result)))
        (should (eq result (gnosis-core-review-attempt-begin "session")))))
    (dolist
        (result
         (list
          `(:session
            (:session_id "session" :mode "online" :status "completed"
             :current_position ,gnosis-core--uint32-max
             :queue_length ,gnosis-core--uint32-max :started_at_us 1)
            :attempt ,gnosis-core--json-null
            :start_position ,#x100000000 :skipped [])
          `(:session
            (:session_id "session" :mode "online" :status "completed"
             :current_position ,#x100000000 :queue_length ,#x100000000
             :started_at_us 1)
            :attempt ,gnosis-core--json-null
            :start_position ,#x100000000 :skipped [])
          `(:session
            (:session_id "session" :mode "online" :status "active"
             :current_position 0 :queue_length ,#x100000000 :started_at_us 1)
            :attempt ,attempt :start_position 0 :skipped [])
          `(:session
            (:session_id "session" :mode "online" :status "completed"
             :current_position ,gnosis-core--uint32-max
             :queue_length ,gnosis-core--uint32-max :started_at_us 1)
            :attempt ,gnosis-core--json-null
            :start_position ,(1- gnosis-core--uint32-max)
            :skipped [(:position ,#x100000000 :thema_id "thema"
                       :reason "archived")])
          `(:session
            (:session_id "session" :mode "online" :status "completed"
             :current_position ,gnosis-core--uint32-max
             :queue_length ,gnosis-core--uint32-max :started_at_us 1)
            :attempt ,gnosis-core--json-null
            :start_position ,gnosis-core--uint32-max
            :skipped [(:position ,gnosis-core--uint32-max :thema_id "thema"
                       :reason "archived")])))
      (cl-letf (((symbol-function 'gnosis-core--typed-request)
                 (lambda (&rest _) result)))
        (should-error (gnosis-core-review-attempt-begin "session")
                      :type 'gnosis-core-mutation-uncertain)))))

(ert-deftest gnosis-core-review-begin-rejects-invalid-attempt-identities ()
  (let ((session '(:session_id "session" :mode "online" :status "active"
                   :current_position 0 :queue_length 1 :started_at_us 1)))
    (dolist (case `((:attempt_id "" :thema_id "thema")
                    (:attempt_id "bad\0id" :thema_id "thema")
                    (:attempt_id ,(make-string 129 ?x) :thema_id "thema")
                    (:attempt_id "attempt" :thema_id "")
                    (:attempt_id "attempt" :thema_id "bad\0id")
                    (:attempt_id "attempt" :thema_id ,(make-string 129 ?x))))
      (let ((attempt (append case '(:session_id "session" :status "pending"))))
        (cl-letf (((symbol-function 'gnosis-core--typed-request)
                   (lambda (&rest _)
                     (list :session session :attempt attempt
                           :start_position 0 :skipped []))))
          (should-error (gnosis-core-review-attempt-begin "session")
                        :type 'gnosis-core-mutation-uncertain))))))

(ert-deftest gnosis-core-review-session-rejects-invalid-id-or-start-time ()
  (dolist (session
           '((:session_id "" :mode "online" :status "active"
              :current_position 0 :queue_length 1 :started_at_us 1)
             (:session_id "session" :mode "online" :status "active"
              :current_position 0 :queue_length 1 :started_at_us 0)
             (:session_id "bad\0session" :mode "online" :status "active"
              :current_position 0 :queue_length 1 :started_at_us 1)
             (:session_id
              "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
              :mode "online" :status "active" :current_position 0
              :queue_length 1 :started_at_us 1)))
    (should-not (gnosis-core--review-session-valid-p session "active")))
  (should
   (gnosis-core--review-session-valid-p
    '(:session_id "session0" :mode "online" :status "active"
      :current_position 0 :queue_length 1 :started_at_us 1)
    "active")))

(ert-deftest gnosis-core-review-session-create-correlates-default-target-count ()
  (let ((source '(:kind "due")))
    (cl-letf (((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest _arguments)
                 `(:session_id "session" :mode "online" :status "active"
                   :source ,source :target_count 1 :current_position 0
                   :queue_length 1 :started_at_us 1))))
      (should-error (gnosis-core-review-session-create source)
                    :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-core-review-desired-state-correlates-response ()
  (let (calls result)
    (cl-letf (((symbol-function 'gnosis-core-operation-id)
               (lambda () "operation"))
              ((symbol-function 'gnosis-core--typed-request)
               (lambda (&rest arguments) (push arguments calls) result)))
      (setq result
            '(:version 1 :operation_id "operation" :changed t
              :attempt_revision 8
              :attempt (:attempt_id "attempt"
                        :accepted_success :gnosis-json-false)))
      (should
       (eq result
           (gnosis-core-review-attempt-set-accepted-success
            "attempt" 7 gnosis-core--json-false)))
      (should
       (equal
        `("put_review_attempt_accepted_success" (("id" . "attempt")) nil
          (:version 1 :operation_id "operation"
           :expected_attempt_revision 7
           :accepted_success ,gnosis-core--json-false))
        (car calls)))
      (setq result
            `(:version 1 :operation_id "operation"
              :changed ,gnosis-core--json-false :attempt_revision 9
              :attempt (:attempt_id "attempt" :desired_suspended t)))
      (should
       (eq result
           (gnosis-core-review-attempt-set-suspension "attempt" 8 t)))
      (setq result
            `(:version 1 :operation_id "operation"
              :changed ,gnosis-core--json-false :attempt_revision 8
              :attempt (:attempt_id "different" :desired_suspended t)))
      (should-error
       (gnosis-core-review-attempt-set-suspension "attempt" 8 t)
       :type 'gnosis-core-mutation-uncertain)
      (setq result
            `(:version 1 :operation_id "operation" :changed t
              :attempt_revision 8
              :attempt (:attempt_id "attempt" :accepted_success t)))
      (should-error
       (gnosis-core-review-attempt-set-accepted-success
        "attempt" 7 gnosis-core--json-false)
       :type 'gnosis-core-mutation-uncertain)
      (setq result
            `(:version 1 :operation_id "operation" :changed t
              :attempt_revision 9
              :attempt (:attempt_id "attempt"
                        :desired_suspended ,gnosis-core--json-false)))
      (should-error
       (gnosis-core-review-attempt-set-suspension "attempt" 8 t)
       :type 'gnosis-core-mutation-uncertain))))

(provide 'gnosis-test-core)
;;; gnosis-test-core.el ends here
