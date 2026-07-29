;;; gnosis-test-core-integration.el --- Live foundation tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-core)
(require 'gnosis-nodes)
(require 'gnosis-review)

(defvar gnosis-core--generated-openapi-operations)
(defvar gnosis-core--generated-openapi-operation-count)
(defvar gnosis-core--generated-openapi-path-count)

(defun gnosis-test-core-integration--authenticated-get-paths ()
  "Return parameter-free authenticated GET paths from the generated contract."
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (cl-loop for operation in gnosis-core--generated-openapi-operations
           for method = (plist-get (cdr operation) :method)
           for path = (plist-get (cdr operation) :path)
           for authenticated = (plist-get (cdr operation) :authenticated)
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

(defmacro gnosis-test-core-integration--with-token (token &rest body)
  "Evaluate BODY using explicit integration TOKEN."
  (declare (indent 1) (debug t))
  `(cl-letf (((symbol-function 'auth-source-search)
              (lambda (&rest _) (list (list :secret ,token)))))
     ,@body))

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

(ert-deftest gnosis-core-integration-dashboard-projection ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((gnosis-core--contract-cache nil)
           (projection (gnosis-core-dashboard)))
      (should (integerp (plist-get projection :review_day)))
      (should (proper-list-p (plist-get projection :today)))
      (should (proper-list-p (plist-get projection :queue)))
      (should (proper-list-p (plist-get projection :content))))))

(ert-deftest gnosis-core-integration-zz-thema-management-journey ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((gnosis-core--contract-cache nil)
           (created
            (gnosis-core-thema-create
             (gnosis-core-operation-id)
             '(:kind "basic" :keimenon "Managed question"
               :answer ["Managed answer"])))
           (thema-id (plist-get created :thema_id))
           (operation-id (gnosis-core-operation-id))
           (suspended
            (gnosis-core-thema-set-suspended
             thema-id 0 t operation-id)))
      (should (= 1 (plist-get (plist-get suspended :state)
                              :state_revision)))
      (should (eq t (plist-get (plist-get suspended :state) :suspended)))
      (should
       (equal suspended
              (gnosis-core-thema-set-suspended
               thema-id 0 t operation-id)))
      (should-error
       (gnosis-core-thema-set-suspended
        thema-id 0 nil (gnosis-core-operation-id))
       :type 'gnosis-core-conflict)
      (let* ((write (gnosis-core-thema-write-view created))
             (edited
              (gnosis-core-thema-update
               thema-id (gnosis-core-operation-id) 1
               (plist-put write :keimenon "Managed edit")))
             (unsuspended
              (gnosis-core-thema-set-suspended
               thema-id 1 nil (gnosis-core-operation-id)))
             (catalog (gnosis-core-thema-catalog nil nil 500))
             (record
              (seq-find
               (lambda (item) (equal thema-id (plist-get item :thema_id)))
               (append (plist-get catalog :items) nil))))
        (should (= 2 (plist-get edited :revision)))
        (should (= 2 (plist-get (plist-get unsuspended :state)
                                :state_revision)))
        (should record)
        (should (= 2 (plist-get record :thema_revision)))
        (should (= 2 (plist-get record :state_revision)))
        (should (equal "Managed edit" (plist-get record :keimenon)))
        (should (eq gnosis-core--json-false
                    (plist-get record :suspended)))))))

(ert-deftest gnosis-core-integration-review-journey ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((gnosis-core--contract-cache nil)
           (thema
            (gnosis-core--typed-request
             "post_themata" nil nil
             '(:kind "basic" :keimenon "Question" :answer ["Answer"])
             (list (gnosis-core--idempotency-key-header
                    (gnosis-core-operation-id)))))
           (thema-id
            (plist-get (plist-get (plist-get thema :authoritative) :value)
                       :thema_id))
           (source
            (list :kind "manual" :tags [] :exclude_tags []
                  :topic_node_id gnosis-core--json-null
                  :topic_forward_depth gnosis-core--json-null
                  :topic_backward_depth gnosis-core--json-null
                  :manual_ids (vector thema-id)))
           (session (gnosis-core-review-session-create source))
           (attempt
            (gnosis-core-review-attempt-begin
             (plist-get session :session_id)))
           (attempt-id (plist-get attempt :attempt_id))
           state result)
      (setq attempt
            (gnosis-core-review-attempt-answer
             attempt-id '(:kind "text" :value "Answer")))
      (should (equal "graded" (plist-get attempt :status)))
      (setq state (gnosis-core-review-attempt-state attempt-id)
            result
            (gnosis-core-review-attempt-set-accepted-success
             attempt-id (plist-get state :attempt_revision)
             gnosis-core--json-false))
      (should (eq gnosis-core--json-false
                  (plist-get (plist-get result :attempt) :accepted_success)))
      (let ((revision (plist-get result :attempt_revision)))
        (setq result
              (gnosis-core-review-attempt-set-accepted-success
               attempt-id revision gnosis-core--json-false))
        (should (eq gnosis-core--json-false (plist-get result :changed)))
        (should (= (1+ revision) (plist-get result :attempt_revision))))
      (setq result
            (gnosis-core-review-attempt-set-suspension
             attempt-id (plist-get result :attempt_revision) t))
      (should (eq t
                  (plist-get (plist-get result :attempt)
                             :desired_suspended)))
      (should (stringp
               (plist-get (gnosis-core-review-attempt-commit attempt-id)
                          :event_id)))
      (should (equal "committed"
                     (plist-get
                      (plist-get (gnosis-core-review-attempt-state attempt-id)
                                 :attempt)
                      :status)))
      (let ((gnosis-review--session session)
            killed)
        (cl-letf (((symbol-function 'kill-buffer)
                   (lambda (&rest _) (setq killed t)))
                  ((symbol-function 'message) #'ignore))
          (gnosis-review--next)
          (should killed)
          (should-not gnosis-review--session))))))

(ert-deftest gnosis-core-integration-progressive-cloze-check ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((gnosis-core--contract-cache nil)
           (thema
            (gnosis-core--typed-request
             "post_themata" nil nil
             '(:kind "cloze" :keimenon "alpha and beta"
               :answer ["alpha" "beta"]
               :hypothesis ["Greek hint" "Cyrillic hint"])
             (list (gnosis-core--idempotency-key-header
                    (gnosis-core-operation-id)))))
           (thema-id
            (plist-get (plist-get (plist-get thema :authoritative) :value)
                       :thema_id))
           (source
            (list :kind "manual" :tags [] :exclude_tags []
                  :topic_node_id gnosis-core--json-null
                  :topic_forward_depth gnosis-core--json-null
                  :topic_backward_depth gnosis-core--json-null
                  :manual_ids (vector thema-id)))
           (session (gnosis-core-review-session-create source))
           (attempt
            (gnosis-core-review-attempt-begin
             (plist-get session :session_id)))
           (attempt-id (plist-get attempt :attempt_id))
           (gnosis-review--attempt attempt)
           (answers '("beta" "alpha"))
           questions)
      (cl-letf (((symbol-function 'gnosis-review--read-string)
                 (lambda (&rest _) (pop answers)))
                ((symbol-function 'gnosis-review--render)
                 (lambda ()
                   (push (plist-get
                          (plist-get gnosis-review--attempt :presentation)
                          :question)
                         questions))))
        (setq attempt
              (gnosis-core-review-attempt-answer
               attempt-id
               (gnosis-review--read-cloze-answer
                (plist-get (plist-get attempt :presentation)
                           :response)))))
      (should (equal '("(Greek hint) and beta" "alpha and beta")
                     (nreverse questions)))
      (should (equal "graded" (plist-get attempt :status)))
      (let* ((state (gnosis-core-review-attempt-state attempt-id))
             (gnosis-review--attempt attempt)
             (gnosis-review--attempt-revision
              (plist-get state :attempt_revision)))
        (cl-letf (((symbol-function 'gnosis-review--render) #'ignore)
                  ((symbol-function 'message) #'ignore))
          (gnosis-review--override))
        (setq state (gnosis-core-review-attempt-state attempt-id))
        (should (equal "graded"
                       (plist-get (plist-get state :attempt) :status)))
        (should (eq gnosis-core--json-false
                    (plist-get (plist-get state :attempt)
                               :accepted_success))))
      (should (stringp
               (plist-get (gnosis-core-review-attempt-commit attempt-id)
                          :event_id))))))

(ert-deftest gnosis-core-integration-generated-manifest ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (load-file (getenv "GNOSIS_OPENAPI_GENERATED"))
  (should (> gnosis-core--generated-openapi-path-count 10))
  (should (= (length gnosis-core--generated-openapi-operations)
             gnosis-core--generated-openapi-operation-count))
  (should (> (length gnosis-core--generated-openapi-operations) 10))
  (dolist (operation gnosis-core--generated-openapi-operations)
    (let ((identity (car operation))
          (method (plist-get (cdr operation) :method))
          (path (plist-get (cdr operation) :path))
          (authenticated (plist-get (cdr operation) :authenticated)))
      (should (stringp identity))
      (should (member method '("DELETE" "GET" "PATCH" "POST" "PUT")))
      (should (string-prefix-p "/v1/" path))
      (should (memq authenticated '(t nil))))))

(ert-deftest gnosis-core-integration-local-org-sync-journey ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((root (make-temp-file "gnosis-org-sync-" t))
         (gnosis-nodes-dir root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (path (format "integration/%s.org" (gnosis-core-operation-id)))
         (prefix ":PROPERTIES:\n:ID: integration-root\n:END:\n")
         (initial (string-to-unibyte (concat prefix "#+TITLE: Initial\n")))
         (local (string-to-unibyte (concat prefix "#+TITLE: Local\n")))
         (remote (string-to-unibyte (concat prefix "#+TITLE: Remote\n")))
         (conflicting-remote
          (string-to-unibyte (concat prefix "#+TITLE: Remote conflict\n")))
         create document-id)
    (unwind-protect
        (progn
          (setq create
                (gnosis-core-document-create
                 (gnosis-core-operation-id) path initial)
                document-id
                (plist-get (plist-get create :authoritative) :document_id))
          (gnosis-nodes-bootstrap)
          (should (equal initial (gnosis-nodes--read-bytes
                                  (gnosis-nodes--safe-path root path))))
          (gnosis-nodes--write-document path local)
          (gnosis-nodes-sync)
          (should (= 2 (plist-get (gnosis-nodes--base
                                   (gnosis-nodes--read-state) document-id path)
                                  :revision)))
          (gnosis-core-document-replace
           document-id (gnosis-core-operation-id) 2 path remote)
          (gnosis-nodes-sync)
          (should (equal remote (gnosis-nodes--read-bytes
                                 (gnosis-nodes--safe-path root path))))
          (gnosis-nodes--write-document
           "integration/restart.org"
           ":PROPERTIES:\n:ID: integration-restart\n:END:\n#+TITLE: Restart\n")
          (gnosis-nodes--write-state
           (gnosis-nodes--enroll-file
            (gnosis-nodes--read-state)
            (gnosis-nodes--safe-path root "integration/restart.org")))
          (gnosis-nodes--send-outbox (gnosis-nodes--read-state))
          (should (seq-some
                   (lambda (entry) (eq :applied (plist-get entry :status)))
                   (append (plist-get (gnosis-nodes--read-state) :outbox) nil)))
          (gnosis-nodes--write-document
           path (concat prefix "#+TITLE: Local conflict\n"))
          (gnosis-core-document-replace
           document-id (gnosis-core-operation-id) 3 path conflicting-remote)
          (gnosis-nodes-sync)
          (should (equal (string-to-unibyte
                          (concat prefix "#+TITLE: Local conflict\n"))
                         (gnosis-nodes--read-bytes
                          (gnosis-nodes--safe-path root path))))
          (should (= 1 (length (plist-get (gnosis-nodes--read-state)
                                          :conflicts)))))
      (delete-directory root t)))))

(ert-deftest gnosis-core-integration-two-client-rename-restart ()
  (skip-unless (gnosis-test-core-integration--available-p))
  (gnosis-test-core-integration--with-client
    (let* ((root-a (make-temp-file "gnosis-org-client-a-" t))
           (root-b (make-temp-file "gnosis-org-client-b-" t))
           (nonce (gnosis-core-operation-id))
           (local-path (format "integration/%s-local.org" nonce))
           (remote-path (format "integration/%s-remote.org" nonce))
           (initial (string-to-unibyte
                     ":PROPERTIES:\n:ID: two-client\n:END:\n#+TITLE: Initial\n"))
           (local (string-to-unibyte
                   ":PROPERTIES:\n:ID: two-client\n:END:\n#+TITLE: Local\n"))
           (digest (gnosis-nodes--digest initial))
           (token-b (getenv "GNOSIS_HTTP_TEST_TOKEN_2"))
           create document-id)
      (should (stringp token-b))
      (unwind-protect
          (progn
            (setq create
                  (gnosis-core-document-create
                   (gnosis-core-operation-id) local-path initial)
                  document-id
                  (plist-get (plist-get create :authoritative) :document_id))
            (let ((gnosis-nodes-dir root-a)
                  (user-emacs-directory (file-name-as-directory root-a))
                  (gnosis-nodes-state-file nil))
              (gnosis-nodes-bootstrap))
            (gnosis-test-core-integration--with-token token-b
              (let ((gnosis-nodes-dir root-b)
                    (user-emacs-directory (file-name-as-directory root-b))
                    (gnosis-nodes-state-file nil))
                (gnosis-nodes-bootstrap)))
            (gnosis-core-document-rename
             document-id (gnosis-core-operation-id) 1 remote-path digest)
            (gnosis-test-core-integration--with-token token-b
              (let ((gnosis-nodes-dir root-b)
                    (user-emacs-directory (file-name-as-directory root-b))
                    (gnosis-nodes-state-file nil))
                (gnosis-nodes--write-document local-path local)
                (gnosis-nodes-sync nil t)
                (let* ((state (gnosis-nodes--read-state))
                       (record (aref (plist-get state :conflicts) 0)))
                  (should (eq :rename (plist-get record :kind)))
                  (setq state
                        (gnosis-nodes--keep-local-conflict state record local))
                  (gnosis-nodes--write-state state)
                  (gnosis-nodes--send-outbox state))
                (should
                 (seq-some
                  (lambda (entry) (eq :applied (plist-get entry :status)))
                  (append (plist-get (gnosis-nodes--read-state) :outbox) nil)))))
            ;; Re-enter with only durable client state after the applied rename.
            (gnosis-test-core-integration--with-token token-b
              (let ((gnosis-nodes-dir root-b)
                    (user-emacs-directory (file-name-as-directory root-b))
                    (gnosis-nodes-state-file nil))
                (gnosis-nodes-sync nil t)
                (let ((state (gnosis-nodes--read-state)))
                  (should (= 0 (length (plist-get state :conflicts))))
                  (should (= 0 (length (plist-get state :outbox)))))
                (should (equal local
                               (gnosis-nodes--working-bytes
                                (gnosis-nodes--safe-path root-b local-path))))
                (should-not (file-exists-p
                             (gnosis-nodes--safe-path root-b remote-path)))))
            (let ((gnosis-nodes-dir root-a)
                  (user-emacs-directory (file-name-as-directory root-a))
                  (gnosis-nodes-state-file nil))
              (gnosis-nodes-sync nil t)
              (should (equal local
                             (gnosis-nodes--working-bytes
                              (gnosis-nodes--safe-path root-a local-path))))
              (should-not (file-exists-p
                           (gnosis-nodes--safe-path root-a remote-path)))))
        (delete-directory root-a t)
        (delete-directory root-b t)))))

(provide 'gnosis-test-core-integration)
;;; gnosis-test-core-integration.el ends here
