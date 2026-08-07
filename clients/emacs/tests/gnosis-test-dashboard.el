;;; gnosis-test-dashboard.el --- Dashboard projection tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)

(defun gnosis-test-dashboard--projection (&optional active-session)
  "Return a server dashboard projection with ACTIVE-SESSION."
  `(:generated_at_us 1 :review_day 42
    :today (:review_count 8 :new_count 2 :success_count 6
            :failure_count 2 :override_count 1)
    :window (:from_day 13 :through_day 42 :days 30 :active_days 4
             :totals (:review_count 20 :new_count 3 :success_count 15
                      :failure_count 5 :override_count 2)
             :average_per_calendar_day 0.67 :average_per_active_day 5.0
             :success_rate 0.75)
    :queue (:new_count 3 :due_count 4 :overdue_count 1 :suspended_count 2)
    :content (:active_themata 12 :archived_themata 3
              :active_documents 2 :active_nodes 7)
    :active_session ,(or active-session gnosis-core--json-null)))

(defun gnosis-test-dashboard--thema (&rest overrides)
  "Return a valid thema catalog record with OVERRIDES."
  (let ((record
         (copy-tree
          `(:thema_id "thema-a" :thema_revision 3 :state_revision 2
            :kind "basic" :keimenon "Question"
            :hypothesis [,gnosis-core--json-null]
            :answer ["Answer"] :tags ["tag"]
            :archived ,gnosis-core--json-false
            :suspended ,gnosis-core--json-false)
          t)))
    (while overrides
      (setq record (plist-put record (pop overrides) (pop overrides))))
    record))

(ert-deftest gnosis-dashboard-renders-only-server-projected-values ()
  (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
    (kill-buffer buffer))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-core-dashboard)
                 #'gnosis-test-dashboard--projection)
                ((symbol-function 'keymap-popup) #'ignore))
        (save-window-excursion (gnosis-dashboard))
        (with-current-buffer gnosis-dashboard-buffer-name
          (should (derived-mode-p 'gnosis-dashboard-mode))
          (should (eq 'gnosis-face-dashboard-header
                      (get-text-property (point-min) 'face)))
          (dolist (text '("Connection: Ready"
                          "Reviewed today: 8 (New: 2)"
                          "Due themata: 4 (Overdue: 1)"
                          "Content: 12 themata, 3 archived, 2 documents, 7 nodes"
                          "Last 30 days: 20 reviews, success rate 0.75"))
            (should (string-match-p (regexp-quote text) (buffer-string))))
          (should-not
           (string-match-p "Review day\\|Queue:\\|Active session"
                           (buffer-string)))))
    (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-entry-opens-dashboard ()
  (let (called)
    (cl-letf (((symbol-function 'gnosis-dashboard)
               (lambda () (interactive) (setq called t)))
              ((symbol-function 'gnosis-review)
               (lambda () (interactive)
                 (ert-fail "Gnosis started review instead of the dashboard"))))
      (call-interactively #'gnosis))
    (should called)))

(ert-deftest gnosis-dashboard-shows-only-active-session-warning ()
  (let ((text
         (gnosis--dashboard-text
          (gnosis-test-dashboard--projection
           '(:session_id "private-session-id" :owner_device_id "owner"
             :mode "standard" :source (:kind "due") :target_count 10
             :status "active" :current_position 3 :queue_length 7
             :started_at_us 1)))))
    (should (string-match-p "Active session in progress" text))
    (should (eq (get-text-property
                 (string-match "Active session in progress" text) 'face text)
                'warning))
    (should-not
     (string-match-p
      "private-session-id\\|owner\\|standard\\|position\\|queue length"
      text))))

(ert-deftest gnosis-dashboard-renders-missing-success-rate ()
  (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
         (window (plist-get state :window)))
    (setf (plist-get window :success_rate) gnosis-core--json-null)
    (with-temp-buffer
      (gnosis-dashboard-mode)
      (gnosis--render-dashboard state)
      (goto-char (point-min))
      (should (search-forward "success rate unavailable" nil t))
      (should (eq 'shadow (get-text-property (1- (point)) 'face))))))

(ert-deftest gnosis-dashboard-refresh-preserves-buffer-on-failure ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (let ((inhibit-read-only t)) (insert "existing projection"))
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               (lambda (&rest _) (signal 'gnosis-core-unavailable '("down")))))
      (let ((condition (should-error (gnosis-dashboard-refresh)
                                     :type 'user-error)))
        (should
         (equal (cadr condition)
                "gnosisd unavailable. Check service and gnosis-core-base-url.")))
      (should (equal "existing projection" (buffer-string))))))

(ert-deftest gnosis-dashboard-keymap-is-direct-and-discoverable ()
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "g")
              #'gnosis-dashboard-refresh))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "r")
              'gnosis-dashboard-review))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "n")
              'gnosis-nodes-find))
  (with-temp-buffer
    (setq gnosis-dashboard--projection
          (list :active_session '(:status "active")))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map "R")
                'gnosis-review-resume)))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "t")
              'gnosis-dashboard-themata))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "T")
              'gnosis-review-topic))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "?")
              'gnosis-dashboard-mode-map-popup)))

(ert-deftest gnosis-dashboard-popup-describes-start-resume-exactly ()
  (let* ((rows (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
         (review-group (car (car rows)))
         (entry (seq-find
                 (lambda (item) (equal (plist-get item :key) "r"))
                 (plist-get review-group :entries))))
    (should (eq (plist-get entry :description)
                #'gnosis-dashboard--review-description))
    (with-temp-buffer
      (should (equal "Start review" (gnosis-dashboard--review-description)))
      (setq gnosis-dashboard--projection
            (list :active_session '(:status "active")))
      (should (equal "Resume review" (gnosis-dashboard--review-description))))
    (dolist (row rows)
      (dolist (group row)
        (should (<= (length (plist-get group :entries)) 4))))))

(ert-deftest gnosis-dashboard-renders-minimal-summary-with-state-faces ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (dolist (text '("Connection" "Reviewed today" "Due themata"
                    "Content" "Last 30 days"))
      (should (string-search text (buffer-string))))
    (dolist (text '("Review day" "Queue:" "Active session"
                    "Review due" "Refresh" "Actions" "Quit"))
      (should-not (string-search text (buffer-string))))
    (goto-char (point-min))
    (search-forward "Connection:")
    (should (eq (get-text-property (1- (point)) 'face) 'shadow))
    (search-forward "Ready")
    (should (eq (get-text-property (1- (point)) 'face) 'success))
    (search-forward "6 successful")
    (should (eq (get-text-property (1- (point)) 'face) 'success))
    (search-forward "2 failed")
    (should (eq (get-text-property (1- (point)) 'face) 'error))
    (search-forward "4")
    (should (eq (get-text-property (1- (point)) 'face) 'warning))
    (search-forward "3 archived")
    (should (eq (get-text-property (1- (point)) 'face) 'shadow))))

(ert-deftest gnosis-dashboard-renders-separator-after-heading ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (goto-char (point-min))
    (search-forward "Gnosis Dashboard")
    (forward-line 1)
    (should (equal (get-text-property (point) 'display)
                   '(space :width text)))
    (should (eq (get-text-property (point) 'face)
                'gnosis-face-separator))))

(ert-deftest gnosis-dashboard-mode-disables-line-numbers ()
  (with-temp-buffer
    (setq display-line-numbers 'relative)
    (gnosis-dashboard-mode)
    (should-not display-line-numbers)))

(ert-deftest gnosis-dashboard-reopening-preserves-local-state ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   #'gnosis-test-dashboard--projection)
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup) #'ignore))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (setq-local gnosis-center-content nil))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (should-not gnosis-center-content)
            (goto-char (point-min))
            (should (looking-at "Gnosis Dashboard"))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-opens-command-popup ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name))
        popup)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   #'gnosis-test-dashboard--projection)
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup)
                   (lambda (map) (setq popup map))))
          (gnosis-dashboard)
          (should (eq popup gnosis-dashboard-mode-map)))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-faces-window-label-and-dynamic-day-value ()
  (dolist (days '(30 7))
    (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
           (window (plist-get state :window)))
      (setf (plist-get window :days) days)
      (with-temp-buffer
        (gnosis-dashboard-mode)
        (gnosis--render-dashboard state)
        (goto-char (point-min))
        (search-forward (format "Last %d days:" days))
        (let ((end (point))
              (start (match-beginning 0))
              (day-start (+ (match-beginning 0) 5)))
          (should (eq (get-text-property start 'face) 'shadow))
          (should (eq (get-text-property day-start 'face)
                      'font-lock-type-face))
          (should (eq (get-text-property (+ day-start
                                            (length (number-to-string days)))
                                         'face)
                      'shadow))
          (should (equal (buffer-substring-no-properties start end)
                         (format "Last %d days:" days))))))))

(ert-deftest gnosis-dashboard-initial-open-starts-at-top ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (save-window-excursion
          (with-current-buffer buffer
            (let ((inhibit-read-only t))
              (erase-buffer)
              (insert (make-string 200 ?x)))
            (goto-char 50))
          (set-window-buffer (selected-window) buffer)
          (set-window-start (selected-window) 25)
          (cl-letf (((symbol-function 'gnosis-core-dashboard)
                     #'gnosis-test-dashboard--projection)
                    ((symbol-function 'keymap-popup) #'ignore))
            (gnosis-dashboard))
          (should (= (with-current-buffer buffer (point)) (point-min)))
          (should (= (window-start (selected-window)) (point-min))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-refresh-preserves-numeric-point ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (goto-char 25)
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               #'gnosis-test-dashboard--projection))
      (gnosis-dashboard-refresh))
    (should (= (point) 25))))

(ert-deftest gnosis-dashboard-refresh-restores-window-start ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (let (restored)
      (cl-letf (((symbol-function 'selected-window) (lambda () 'window))
                ((symbol-function 'window-buffer)
                 (lambda (_window) (current-buffer)))
                ((symbol-function 'window-start) (lambda (&rest _) 11))
                ((symbol-function 'set-window-start)
                 (lambda (&rest arguments) (setq restored arguments)))
                ((symbol-function 'window-body-width) (lambda (&rest _) 80))
                ((symbol-function 'gnosis-core-dashboard)
                 #'gnosis-test-dashboard--projection))
        (gnosis-dashboard-refresh))
      (should (equal restored '(window 11 t))))))

(ert-deftest gnosis-dashboard-initial-failure-opens-safe-corrective-state ()
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-dashboard)
                   (lambda (&rest _)
                     (signal 'gnosis-core-authentication-error
                             '("secret server detail"))))
                  ((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup) #'ignore))
          (gnosis-dashboard)
          (with-current-buffer buffer
            (should (derived-mode-p 'gnosis-dashboard-mode))
            (should-not gnosis-dashboard--projection)
            (should
             (string-search
              "Gnosis authentication failed. Check auth-source token for gnosisd."
              (buffer-string)))
            (should-not (string-search "secret server detail" (buffer-string)))
            (goto-char (point-min))
            (search-forward "authentication failed")
            (should (eq (get-text-property (1- (point)) 'face) 'error))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-dashboard-primary-actions-dispatch-existing-journeys ()
  (let (review-arguments resumed)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda (&rest arguments) (setq review-arguments arguments)))
              ((symbol-function 'gnosis-review-resume)
               (lambda () (interactive) (setq resumed t))))
      (gnosis-dashboard-review)
      (with-temp-buffer
        (setq gnosis-dashboard--projection
              (list :active_session '(:status "active")))
        (call-interactively (keymap-lookup gnosis-dashboard-mode-map "R"))))
    (should (equal review-arguments '(nil)))
    (should resumed)))

(defun gnosis-test-dashboard--generation (position &optional server-id)
  "Return a generation cursor at POSITION for optional SERVER-ID."
  (base64url-encode-string
   (json-serialize `(:version 1 :server_id ,(or server-id "server-a")
                     :position ,position :token ,(format "token-%d" position)))
   t))

(defmacro gnosis-test-dashboard--with-authority (&rest body)
  "Evaluate BODY with deterministic dashboard authority and token."
  `(let ((gnosis-themata--authority 'test-authority))
     (cl-letf (((symbol-function 'gnosis-core--token)
                (lambda () "test-token"))
               ((symbol-function 'gnosis-core--authenticated-authority)
                (lambda (&optional _) 'test-authority)))
       ,@body)))

(ert-deftest gnosis-themata-cache-round-trips-complete-catalog-privately ()
  (let* ((directory (make-temp-file "gnosis-themata-cache-" t))
         (gnosis-themata-cache-file (expand-file-name "thema.json" directory))
         (generation (gnosis-test-dashboard--generation 3))
         (catalog `(:generation ,generation
                    :items [,(gnosis-test-dashboard--thema)])))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                   (lambda () ["https://test.example" "token-hash"])))
          (gnosis-themata--cache-write catalog)
          (should (equal catalog (gnosis-themata--cache-read)))
          (should (= #o600 (logand #o777 (file-modes gnosis-themata-cache-file))))
          (should (= #o700 (logand #o777 (file-modes directory)))))
      (delete-directory directory t))))

(ert-deftest gnosis-themata-cache-rejects-corruption-and-wrong-authority ()
  (let* ((directory (make-temp-file "gnosis-themata-cache-" t))
         (gnosis-themata-cache-file (expand-file-name "thema.json" directory))
         (generation (gnosis-test-dashboard--generation 3))
         (catalog `(:generation ,generation
                    :items [,(gnosis-test-dashboard--thema)])))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                   (lambda () (vector gnosis-core-base-url "token-hash"))))
          (gnosis-themata--cache-write catalog)
          (let ((gnosis-core-base-url "https://other.example"))
            (should-not (gnosis-themata--cache-read)))
          (with-temp-file gnosis-themata-cache-file (insert "not json"))
          (should-not (gnosis-themata--cache-read)))
      (delete-directory directory t))))

(ert-deftest gnosis-themata-cache-is-bound-to-credential-context ()
  (let* ((directory (make-temp-file "gnosis-themata-cache-" t))
         (gnosis-themata-cache-file (expand-file-name "thema.json" directory))
         (catalog `(:generation ,(gnosis-test-dashboard--generation 3)
                    :items [,(gnosis-test-dashboard--thema)]))
         (token "token-a"))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core--token) (lambda () token)))
          (gnosis-themata--cache-write catalog)
          (should (equal catalog (gnosis-themata--cache-read)))
          (setq token "token-b")
          (should-not (gnosis-themata--cache-read)))
      (delete-directory directory t))))

(ert-deftest gnosis-themata-disposable-cache-cannot-block-fresh-catalog ()
  (let ((catalog `(:generation ,(gnosis-test-dashboard--generation 3)
                   :items [,(gnosis-test-dashboard--thema)])))
    (cl-letf (((symbol-function 'gnosis-themata--cache-write)
               (lambda (_) (signal 'file-error '("read only")))))
      (should (equal catalog (gnosis-themata--cache-try-write catalog))))))

(ert-deftest gnosis-themata-reconciles-exact-authenticated-replica-projections ()
  (let* ((directory (make-temp-file "gnosis-themata-cache-" t))
         (gnosis-themata-cache-file (expand-file-name "thema.json" directory))
         (from (gnosis-test-dashboard--generation 3))
         (middle (gnosis-test-dashboard--generation 4))
         (target (gnosis-test-dashboard--generation 5))
         (catalog `(:generation ,from
                    :items [,(gnosis-test-dashboard--thema)]))
         calls)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                   (lambda () ["https://test.example" "token-hash"]))
                  ((symbol-function 'gnosis-core-sync-changes)
                   (lambda (cursor)
                     (push cursor calls)
                     `(:version 1 :from ,from :next ,target
                       :has_more ,gnosis-core--json-false
                       :items
                       [(:cursor ,middle :entity_kind "thema"
                         :projection
                         (:thema_id "thema-a" :revision 4 :kind "basic"
                          :keimenon "Updated" :hypothesis [] :answer ["A"]
                          :tags ["tag"] :archived ,gnosis-core--json-false))
                        (:cursor ,target :entity_kind "review_state"
                         :projection
                         (:thema_id "thema-a"
                          :state (:state_revision 3 :review_count 2
                                  :suspended t))) ]))))
          (let* ((result (gnosis-themata--reconcile-page catalog))
                 (updated (plist-get result :catalog))
                 (record (aref (plist-get updated :items) 0)))
            (gnosis-themata--cache-try-write updated)
            (should (equal target (plist-get updated :generation)))
            (should (equal "Updated" (plist-get record :keimenon)))
            (should (= 4 (plist-get record :thema_revision)))
            (should (= 3 (plist-get record :state_revision)))
            (should (eq t (plist-get record :suspended)))
            (should (equal updated (gnosis-themata--cache-read))))
          (should (equal (list from) calls)))
      (delete-directory directory t))))

(ert-deftest gnosis-themata-rejects-replica-cursor-gaps ()
  (let* ((from (gnosis-test-dashboard--generation 3))
         (item-cursor (gnosis-test-dashboard--generation 4))
         (next (gnosis-test-dashboard--generation 5))
         (catalog `(:generation ,from
                    :items [,(gnosis-test-dashboard--thema)])))
    (cl-letf (((symbol-function 'gnosis-core-sync-changes)
               (lambda (_)
                 `(:version 1 :from ,from :next ,next
                   :has_more ,gnosis-core--json-false
                   :items [(:cursor ,item-cursor :entity_kind "thema"
                            :projection
                            (:thema_id "thema-a" :revision 4 :kind "basic"
                             :keimenon "Gap" :hypothesis [] :answer ["A"]
                             :tags [] :archived ,gnosis-core--json-false))]))))
      (should-error (gnosis-themata--reconcile-page catalog)
                    :type 'gnosis-core-conflict))))

(ert-deftest gnosis-themata-rejects-empty-nonterminal-replica-page ()
  (let* ((cursor (gnosis-test-dashboard--generation 3))
         (catalog `(:generation ,cursor
                    :items [,(gnosis-test-dashboard--thema)]))
         (invalid `(:version 1 :from ,cursor :next ,cursor
                    :has_more t :items []))
         (terminal `(:version 1 :from ,cursor :next ,cursor
                     :has_more ,gnosis-core--json-false :items [])))
    (cl-letf (((symbol-function 'gnosis-core-sync-changes)
               (lambda (_) invalid)))
      (should-error (gnosis-themata--reconcile-page catalog)
                    :type 'gnosis-core-protocol-error))
    (cl-letf (((symbol-function 'gnosis-core-sync-changes)
               (lambda (_) terminal)))
      (should (plist-get (gnosis-themata--reconcile-page catalog) :done)))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (let ((run 7)
            (scheduled 0)
            (authority ["https://test.example" "token-hash"]))
        (setq gnosis-themata--authority authority
              gnosis-themata--refresh-run run
              gnosis-themata--refresh-state
              (list :kind 'reconcile :catalog catalog :selected nil))
        (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                   (lambda () authority))
                  ((symbol-function 'gnosis-core-sync-changes)
                   (lambda (_) invalid))
                  ((symbol-function 'run-at-time)
                   (lambda (&rest _)
                     (cl-incf scheduled)
                     'timer)))
          (gnosis-themata--refresh-step (current-buffer) run))
        (should (= 0 scheduled))
        (should-not gnosis-themata--refresh-state)))))

(ert-deftest gnosis-themata-cache-read-is-bounded-by-bytes-not-stale-stat ()
  (let* ((directory (make-temp-file "gnosis-themata-cache-" t))
         (gnosis-themata-cache-file (expand-file-name "thema.json" directory))
         (gnosis-themata--cache-maximum-bytes 10)
         (original (symbol-function 'insert-file-contents))
         observed-end)
    (unwind-protect
        (progn
          (with-temp-file gnosis-themata-cache-file
            (insert (make-string 100 ?x)))
          (set-file-modes gnosis-themata-cache-file #o600)
          (cl-letf (((symbol-function 'insert-file-contents)
                     (lambda (file &optional visit beg end replace)
                       (setq observed-end end)
                       (funcall original file visit beg end replace))))
            (should-not
             (gnosis-themata--cache-read
              ["https://test.example" "token-hash"])))
          (should (= 11 observed-end)))
      (delete-directory directory t))))

(ert-deftest gnosis-themata-bootstrap-rejects-byte-budget-before-retention ()
  (let* ((gnosis-themata--cache-envelope-reserve 0)
         (gnosis-themata--cache-maximum-bytes 100)
         (gnosis-themata--refresh-state
          (list :kind 'bootstrap :after nil :generation nil :records nil
                :bytes 0 :seen_ids (make-hash-table :test #'equal)
                :seen_cursors (make-hash-table :test #'equal)))
         (record (gnosis-test-dashboard--thema
                  :keimenon (make-string 200 ?q))))
    (cl-letf (((symbol-function 'gnosis-core-thema-catalog)
               (lambda (&rest _)
                 `(:generation ,(gnosis-test-dashboard--generation 1)
                   :items [,record] :next ,gnosis-core--json-null))))
      (should-error (gnosis-themata--bootstrap-step)
                    :type 'gnosis-core-protocol-error))
    (should-not (plist-get gnosis-themata--refresh-state :records))))

(ert-deftest gnosis-themata-reconciliation-reuses-record-table-across-pages ()
  (let* ((cursor-3 (gnosis-test-dashboard--generation 3))
         (cursor-4 (gnosis-test-dashboard--generation 4))
         (cursor-5 (gnosis-test-dashboard--generation 5))
         (catalog `(:generation ,cursor-3
                    :items [,(gnosis-test-dashboard--thema)]))
         (pages
          (list
           `(:from ,cursor-3 :next ,cursor-4 :has_more t
             :items [(:cursor ,cursor-4 :entity_kind "review_state"
                      :projection (:thema_id "thema-a"
                                   :state (:state_revision 3 :suspended t)))])
           `(:from ,cursor-4 :next ,cursor-5
             :has_more ,gnosis-core--json-false
             :items [(:cursor ,cursor-5 :entity_kind "review_state"
                      :projection (:thema_id "thema-a"
                                   :state (:state_revision 4
                                           :suspended ,gnosis-core--json-false)))])
           ))
         (original (symbol-function 'gnosis-themata--record-table))
         (table-builds 0))
    (cl-letf (((symbol-function 'gnosis-core-sync-changes)
               (lambda (_) (pop pages)))
              ((symbol-function 'gnosis-themata--record-table)
               (lambda (items)
                 (cl-incf table-builds)
                 (funcall original items))))
      (let* ((first (gnosis-themata--reconcile-page catalog))
             (second (gnosis-themata--reconcile-page
                      (plist-get first :catalog))))
        (should-not (plist-get first :done))
        (should (plist-get second :done))))
    (should (= 1 table-builds))))

(ert-deftest gnosis-themata-reconciliation-limits-before-table-mutation ()
  (let* ((cursor-3 (gnosis-test-dashboard--generation 3))
         (cursor-4 (gnosis-test-dashboard--generation 4))
         (record (gnosis-test-dashboard--thema))
         (records (gnosis-themata--record-table (vector record)))
         (bytes (gnosis-themata--records-byte-size (vector record)))
         (catalog `(:generation ,cursor-3 :record_table ,records :bytes ,bytes))
         (before (copy-tree record)))
    (let ((gnosis-themata--cache-maximum-bytes (+ bytes 10)))
      (cl-letf (((symbol-function 'gnosis-core-sync-changes)
                 (lambda (_)
                   `(:from ,cursor-3 :next ,cursor-4
                     :has_more ,gnosis-core--json-false
                     :items
                     [(:cursor ,cursor-4 :entity_kind "thema"
                       :projection
                       (:thema_id "thema-a" :revision 2 :kind "basic"
                        :keimenon ,(make-string 200 ?q)
                        :hypothesis [] :answer ["A"] :tags []
                        :archived ,gnosis-core--json-false))]))))
        (should-error (gnosis-themata--reconcile-page catalog)
                      :type 'gnosis-core-protocol-error)))
    (should (equal before (gethash "thema-a" records)))
    (let ((gnosis-themata--catalog-max-items 1))
      (cl-letf (((symbol-function 'gnosis-core-sync-changes)
                 (lambda (_)
                   `(:from ,cursor-3 :next ,cursor-4
                     :has_more ,gnosis-core--json-false
                     :items
                     [(:cursor ,cursor-4 :entity_kind "thema"
                       :projection
                       (:thema_id "thema-b" :revision 1 :kind "basic"
                        :keimenon "New" :hypothesis [] :answer ["B"]
                        :tags [] :archived ,gnosis-core--json-false))]))))
        (should-error (gnosis-themata--reconcile-page catalog)
                      :type 'gnosis-core-protocol-error)))
    (should (= 1 (hash-table-count records)))
    (should-not (gethash "thema-b" records))))

(ert-deftest gnosis-themata-keymap-hides-transport-pagination ()
  (should-not (memq (keymap-lookup gnosis-dashboard-themata-mode-map "n")
                    '(gnosis-themata-next-page gnosis-themata-previous-page)))
  (should-not (memq (keymap-lookup gnosis-dashboard-themata-mode-map "p")
                    '(gnosis-themata-next-page gnosis-themata-previous-page)))
  (should (eq (keymap-lookup gnosis-dashboard-themata-mode-map "/")
              'gnosis-themata-filter-search))
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (should (eq (command-remapping 'tabulated-list-sort)
                'gnosis-themata-sort))
    (should (eq (command-remapping 'tabulated-list-col-sort)
                'gnosis-themata-col-sort))))

(ert-deftest gnosis-themata-renders-large-catalog-in-cancellable-chunks ()
  (let* ((first (gnosis-test-dashboard--thema :thema_id "a" :keimenon "A"))
         (second (gnosis-test-dashboard--thema :thema_id "b" :keimenon "B"))
         (third (gnosis-test-dashboard--thema :thema_id "c" :keimenon "C"))
         (gnosis-themata--progressive-threshold 1)
         (gnosis-themata--render-chunk-size 1)
         (original-entry (symbol-function 'gnosis-themata--entry))
         (entry-calls 0)
         scheduled)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (cl-letf (((symbol-function 'run-at-time)
                 (lambda (_time _repeat function &rest arguments)
                   (setq scheduled (cons function arguments))
                   'timer))
                ((symbol-function 'gnosis-themata--entry)
                 (lambda (record)
                   (cl-incf entry-calls)
                   (funcall original-entry record))))
        (gnosis-themata--install
         `(:generation "generation" :items [,first ,second ,third]))
        (should (= 1 entry-calls))
        (should (= 1 (how-many "^[^\n]" (point-min) (point-max))))
        (should scheduled)
        (apply (car scheduled) (cdr scheduled))
        (should (= 2 entry-calls))
        (should (= 2 (how-many "^[^\n]" (point-min) (point-max))))))))

(ert-deftest gnosis-themata-progressive-render-respects-question-sort ()
  (let ((zeta (gnosis-test-dashboard--thema :thema_id "a" :keimenon "Zeta"))
        (alpha (gnosis-test-dashboard--thema :thema_id "b" :keimenon "Alpha"))
        (gnosis-themata--progressive-threshold 1)
        (gnosis-themata--render-chunk-size 1))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (cl-letf (((symbol-function 'run-at-time) (lambda (&rest _) 'timer)))
        (gnosis-themata--install
         `(:generation "generation" :items [,zeta ,alpha]))
        (goto-char (point-min))
        (should (equal "b" (tabulated-list-get-id)))))))

(ert-deftest gnosis-themata-progressive-sort-restarts-complete-render ()
  (let ((a (gnosis-test-dashboard--thema :thema_id "a" :keimenon "Alpha"))
        (b (gnosis-test-dashboard--thema :thema_id "b" :keimenon "Beta"))
        (c (gnosis-test-dashboard--thema :thema_id "c" :keimenon "Gamma"))
        (gnosis-themata--progressive-threshold 1)
        (gnosis-themata--render-chunk-size 1)
        scheduled)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (cl-letf (((symbol-function 'run-at-time)
                 (lambda (_time _repeat function &rest arguments)
                   (setq scheduled
                         (append scheduled (list (cons function arguments))))
                   'timer)))
        (gnosis-themata--install `(:generation "g" :items [,a ,b ,c]))
        (should (= 1 (how-many "^[^\n]" (point-min) (point-max))))
        (let ((current-prefix-arg 0))
          (call-interactively #'gnosis-themata-sort))
        (while scheduled
          (let ((job (pop scheduled)))
            (apply (car job) (cdr job))))
        (should (= 3 (how-many "^[^\n]" (point-min) (point-max))))
        (goto-char (point-min))
        (should (equal '("c" "b" "a")
                       (cl-loop until (eobp)
                                collect (tabulated-list-get-id)
                                do (forward-line 1))))
        (let ((current-prefix-arg -1))
          (call-interactively #'gnosis-themata-sort))
        (while scheduled
          (let ((job (pop scheduled)))
            (apply (car job) (cdr job))))
        (should-not tabulated-list-sort-key)
        (goto-char (point-min))
        (should (equal '("a" "b" "c")
                       (cl-loop until (eobp)
                                collect (tabulated-list-get-id)
                                do (forward-line 1))))))))

(ert-deftest gnosis-themata-mode-reentry-rejects-obsolete-render-callback ()
  (let ((old-a (gnosis-test-dashboard--thema :thema_id "old-a" :keimenon "Old A"))
        (old-b (gnosis-test-dashboard--thema :thema_id "old-b" :keimenon "Old B"))
        (new (gnosis-test-dashboard--thema :thema_id "new" :keimenon "New"))
        (gnosis-themata--progressive-threshold 1)
        (gnosis-themata--render-chunk-size 1)
        scheduled)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (cl-letf (((symbol-function 'run-at-time)
                 (lambda (_time _repeat function &rest arguments)
                   (setq scheduled (cons function arguments))
                   'timer)))
        (gnosis-themata--install
         `(:generation "old" :items [,old-a ,old-b]))
        (let ((obsolete scheduled))
          (gnosis-dashboard-themata-mode)
          (gnosis-themata--install `(:generation "new" :items [,new]))
          (apply (car obsolete) (cdr obsolete))
          (should (string-search "New" (buffer-string)))
          (should-not (string-search "Old B" (buffer-string))))))))

(ert-deftest gnosis-themata-buffer-displays-archive-and-suspension-state ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema
                 :archived t :suspended t)]))
    (should (derived-mode-p 'gnosis-dashboard-themata-mode))
    (should (string-search "Question" (buffer-string)))
    (should (string-search "Answer" (buffer-string)))
    (goto-char (point-min))
    (should (= 2 (how-many "\\_<Yes\\_>" (point-min) (point-max))))))

(ert-deftest gnosis-themata-confirmed-mutations-recheck-live-authority ()
  (dolist (command '(gnosis-themata-toggle-suspension
                     gnosis-themata-toggle-archive))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install
       `(:generation "g" :items [,(gnosis-test-dashboard--thema)]))
      (gnosis-themata--goto-id "thema-a")
      (let ((live 'authority-a)
            (gnosis-themata--authority 'authority-a)
            (mutations 0))
        (cl-letf (((symbol-function 'gnosis-core--token)
                   (lambda () "live-token"))
                  ((symbol-function 'gnosis-core--authenticated-authority)
                   (lambda (&optional _) live))
                  ((symbol-function 'y-or-n-p)
                   (lambda (&rest _)
                     (setq live 'authority-b)
                     t))
                  ((symbol-function 'gnosis-core-thema-set-suspended)
                   (lambda (&rest _) (cl-incf mutations)))
                  ((symbol-function 'gnosis-core-thema-set-archived)
                   (lambda (&rest _) (cl-incf mutations))))
          (should-error (funcall command)
                        :type 'gnosis-core-authorization-error))
        (should (= 0 mutations))))))

(ert-deftest gnosis-themata-bulk-mutation-pins-one-token ()
  (let ((first (gnosis-test-dashboard--thema
                :thema_id "first" :tags ["tag"] :state_revision 1))
        (second (gnosis-test-dashboard--thema
                 :thema_id "second" :tags ["tag"] :state_revision 2))
        (token-calls 0)
        requests)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install `(:generation "g" :items [,first ,second]))
      (setq-local gnosis-themata--tag-filter "tag")
      (let ((gnosis-themata--authority 'authority-a))
        (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-core--token)
                   (lambda ()
                     (cl-incf token-calls)
                     "pinned-token"))
                  ((symbol-function 'gnosis-core--authenticated-authority)
                   (lambda (&optional token)
                     (when token
                       (should (equal "pinned-token" token)))
                     'authority-a))
                  ((symbol-function 'gnosis-core-thema-set-suspended)
                   (lambda (id revision desired)
                     (should (equal "pinned-token" gnosis-core--token-override))
                     (push id requests)
                     `(:thema_id ,id
                       :state (:state_revision ,(1+ revision)
                               :suspended ,desired)))))
          (gnosis-themata-suspend-tag)))
      (should (= 1 token-calls))
      (should (equal '("first" "second") (nreverse requests))))))

(ert-deftest gnosis-themata-suspension-mutates-only-after-authority ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema)]))
    (gnosis-themata--goto-id "thema-a")
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
              ((symbol-function 'gnosis-core-thema-set-suspended)
               (lambda (thema-id revision desired &optional _operation-id)
                 (should (equal thema-id "thema-a"))
                 (should (= revision 2))
                 (should desired)
                 '(:thema_id "thema-a"
                   :state (:state_revision 3 :suspended t)))))
      (gnosis-test-dashboard--with-authority
       (gnosis-themata-toggle-suspension)))
    (let ((record (gethash "thema-a" gnosis-themata--records)))
      (should (= 3 (plist-get record :state_revision)))
      (should (eq t (plist-get record :suspended))))
    (should (string-search "Yes" (buffer-string)))
    (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
              ((symbol-function 'gnosis-core-thema-set-suspended)
               (lambda (_thema-id revision desired &optional _operation-id)
                 (should (= revision 3))
                 (should-not desired)
                 `(:thema_id "thema-a"
                   :state (:state_revision 4
                           :suspended ,gnosis-core--json-false)))))
      (gnosis-test-dashboard--with-authority
       (gnosis-themata-toggle-suspension)))
    (should (string-search "No" (buffer-string)))
    (should
     (eq gnosis-core--json-false
         (plist-get (gethash "thema-a" gnosis-themata--records)
                    :suspended)))))

(ert-deftest gnosis-themata-filters-preserve-selected-authoritative-row ()
  (let* ((alpha (gnosis-test-dashboard--thema
                 :thema_id "alpha" :keimenon "Alpha question"))
         (beta (gnosis-test-dashboard--thema
                :thema_id "beta" :keimenon "Beta question" :archived t))
         (catalog `(:generation "g" :items [,alpha ,beta])))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install catalog)
      (gnosis-themata--goto-id "alpha")
      (gnosis-themata-filter-search "Alpha")
      (should (equal "alpha" (tabulated-list-get-id)))
      (should (= 1 (length tabulated-list-entries)))
      (gnosis-themata-filter-review-state :all)
      (should (equal "alpha" (tabulated-list-get-id))))))

(ert-deftest gnosis-themata-archive-and-tag-actions-use-authority ()
  (let* ((record (gnosis-test-dashboard--thema
                  :thema_id "thema-a" :thema_revision 3 :tags ["tag"]))
         archived)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (setq-local gnosis-themata--records (make-hash-table :test #'equal))
      (puthash "thema-a" record gnosis-themata--records)
      (setq-local tabulated-list-entries (list (gnosis-themata--entry record)))
      (tabulated-list-print t)
      (goto-char (point-min))
      (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                ((symbol-function 'gnosis-core-thema-set-archived)
                 (lambda (id revision desired)
                   (setq archived (list id revision desired))
                   (plist-put (copy-tree record) :archived t)))
                )
        (gnosis-test-dashboard--with-authority
         (gnosis-themata-toggle-archive))
        (gnosis-themata-find-tag "tag"))
      (should (equal "tag" gnosis-themata--tag-filter)))
    (should (equal '("thema-a" 3 t) archived))))

(ert-deftest gnosis-themata-suspend-tag-freezes-complete-progressive-target-set ()
  (let ((a (gnosis-test-dashboard--thema :thema_id "a" :keimenon "Alpha"
                                          :tags ["tag"]))
        (b (gnosis-test-dashboard--thema :thema_id "b" :keimenon "Beta"
                                          :tags ["tag"]))
        (c (gnosis-test-dashboard--thema :thema_id "c" :keimenon "Gamma"
                                          :tags ["tag"]))
        (gnosis-themata--progressive-threshold 1)
        (gnosis-themata--render-chunk-size 1)
        mutated)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (setq gnosis-themata--tag-filter "tag")
      (cl-letf (((symbol-function 'run-at-time) (lambda (&rest _) 'timer))
                ((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                ((symbol-function 'gnosis-core-thema-set-suspended)
                 (lambda (thema-id revision suspended)
                   (setq mutated (append mutated (list thema-id)))
                   `(:thema_id ,thema-id
                     :state (:state_revision ,(1+ revision)
                             :suspended ,suspended)))))
        (gnosis-themata--install `(:generation "g" :items [,c ,a ,b]))
        (should (= 1 (length tabulated-list-entries)))
        (gnosis-test-dashboard--with-authority
         (gnosis-themata-suspend-tag)))
      (should (equal '("a" "b" "c") mutated)))))

(ert-deftest gnosis-themata-suspend-tag-mutates-visible-page-in-order ()
  (let ((first (gnosis-test-dashboard--thema
                :thema_id "first" :tags ["tag"] :state_revision 1))
        (second (gnosis-test-dashboard--thema
                 :thema_id "second" :tags ["tag"] :state_revision 2))
        calls)
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install `(:generation "g" :items [,first ,second]))
      (gnosis-themata-find-tag "tag")
      (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                ((symbol-function 'gnosis-core-thema-set-suspended)
                 (lambda (id revision desired)
                   (push (list id revision desired) calls)
                   `(:thema_id ,id
                     :state (:state_revision ,(1+ revision) :suspended t)))))
        (gnosis-test-dashboard--with-authority
         (gnosis-themata-suspend-tag)))
      (should (equal '(("first" 1 t) ("second" 2 t)) (nreverse calls)))
      (should (eq t (plist-get (gethash "first" gnosis-themata--records)
                               :suspended)))
      (should (eq t (plist-get (gethash "second" gnosis-themata--records)
                               :suspended))))))

(ert-deftest gnosis-themata-mutation-failures-preserve-visible-row ()
  (dolist (error '(gnosis-core-authorization-error
                   gnosis-core-conflict
                   gnosis-core-validation-error
                   gnosis-core-mutation-uncertain))
    (with-temp-buffer
      (gnosis-dashboard-themata-mode)
      (gnosis-themata--install
       `(:generation "feed-3"
         :items [,(gnosis-test-dashboard--thema)]))
      (gnosis-themata--goto-id "thema-a")
      (let ((before (buffer-string)))
        (cl-letf (((symbol-function 'y-or-n-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-core-thema-set-suspended)
                   (lambda (&rest _) (signal error '("failure")))))
          (should-error
           (gnosis-test-dashboard--with-authority
            (gnosis-themata-toggle-suspension))
           :type error))
        (should (equal before (buffer-string)))
        (should
         (eq gnosis-core--json-false
             (plist-get (gethash "thema-a" gnosis-themata--records)
                        :suspended)))))))

(ert-deftest gnosis-themata-refresh-preserves-buffer-on-failure ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation ,(gnosis-test-dashboard--generation 3)
       :items [,(gnosis-test-dashboard--thema)]))
    (let ((before (buffer-string))
          (generation gnosis-themata--generation)
          scheduled)
      (cl-letf (((symbol-function 'run-at-time)
                 (lambda (_time _repeat function &rest arguments)
                   (setq scheduled (cons function arguments))
                   'timer)))
        (gnosis-themata-refresh))
      (cl-letf (((symbol-function 'gnosis-core-sync-changes)
                 (lambda (&rest _)
                   (signal 'gnosis-core-unavailable '("offline")))))
        (apply (car scheduled) (cdr scheduled)))
      (should (equal before (buffer-string)))
      (should (equal generation gnosis-themata--generation)))))

(ert-deftest gnosis-themata-edit-opens-shared-editor-and-updates-one-row ()
  (with-temp-buffer
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install
     `(:generation "feed-3"
       :items [,(gnosis-test-dashboard--thema)]))
    (gnosis-themata--goto-id "thema-a")
    (let (callback)
      (cl-letf (((symbol-function 'gnosis-edit-thema)
                 (lambda (thema-id after-save)
                   (should (equal "thema-a" thema-id))
                   (setq callback after-save))))
        (gnosis-themata-edit))
      (funcall callback
               `(:thema_id "thema-a" :revision 4 :kind "basic"
                 :keimenon "Edited question" :hypothesis []
                 :answer ["Edited answer"] :tags ["new"]
                 :archived ,gnosis-core--json-false)))
    (let ((record (gethash "thema-a" gnosis-themata--records)))
      (should (= 4 (plist-get record :thema_revision)))
      (should (= 2 (plist-get record :state_revision))))
    (should (string-search "Edited question" (buffer-string)))
    (should (string-search "Edited answer" (buffer-string)))))

(ert-deftest gnosis-themata-edit-callback-cannot-cross-buffer-incarnation ()
  (with-temp-buffer
    (let ((authority ["https://a" "hash-a"])
          callback)
      (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                 (lambda () authority))
                ((symbol-function 'gnosis-edit-thema)
                 (lambda (_id after-save) (setq callback after-save))))
        (gnosis-dashboard-themata-mode)
        (setq gnosis-themata--authority authority)
        (gnosis-themata--install
         `(:generation "first"
           :items [,(gnosis-test-dashboard--thema :keimenon "First")]))
        (gnosis-themata--goto-id "thema-a")
        (gnosis-themata-edit)
        (setq authority ["https://b" "hash-b"])
        (gnosis-dashboard-themata-mode)
        (setq gnosis-themata--authority authority)
        (gnosis-themata--install
         `(:generation "second"
           :items [,(gnosis-test-dashboard--thema :keimenon "Second")]))
        (funcall callback
                 `(:thema_id "thema-a" :revision 4 :kind "basic"
                   :keimenon "Stale" :hypothesis [] :answer ["A"]
                   :tags [] :archived ,gnosis-core--json-false))
        (should
         (equal "Second"
                (plist-get (gethash "thema-a" gnosis-themata--records)
                           :keimenon)))))))

(ert-deftest gnosis-themata-edit-callback-rejects-live-authority-drift ()
  (with-temp-buffer
    (let ((live-authority ["https://a" "hash-a"])
          callback)
      (cl-letf (((symbol-function 'gnosis-themata--current-authority)
                 (lambda () live-authority))
                ((symbol-function 'gnosis-edit-thema)
                 (lambda (_id after-save) (setq callback after-save))))
        (gnosis-dashboard-themata-mode)
        (setq gnosis-themata--authority live-authority)
        (gnosis-themata--install
         `(:generation "first"
           :items [,(gnosis-test-dashboard--thema :keimenon "First")]))
        (gnosis-themata--goto-id "thema-a")
        (gnosis-themata-edit)
        (setq live-authority ["https://b" "hash-b"])
        (funcall callback
                 `(:thema_id "thema-a" :revision 4 :kind "basic"
                   :keimenon "Stale" :hypothesis [] :answer ["A"]
                   :tags [] :archived ,gnosis-core--json-false))
        (should
         (equal "First"
                (plist-get (gethash "thema-a" gnosis-themata--records)
                           :keimenon)))))))

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
