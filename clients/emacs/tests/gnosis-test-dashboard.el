;;; gnosis-test-dashboard.el --- Dashboard summary tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)
(require 'gnosis-dashboard)

(defvar gnosis-review-history-buffer-name)
(defvar gnosis-review-history--authority)
(defvar gnosis-review-history--cursor)
(defvar gnosis-review-history--next)
(defvar gnosis-review-history--previous)
(defvar gnosis-review-history--records)
(defvar gnosis-review-history--status)
(defvar gnosis-nodes-map)
(defvar gnosis-themata-map)
(defvar gnosis-dashboard-themata-mode-map)

(declare-function gnosis-dashboard-add-thema "gnosis-dashboard")
(declare-function gnosis-dashboard-health "gnosis-dashboard")
(declare-function gnosis-dashboard-nodes "gnosis-dashboard")
(declare-function gnosis-dashboard-search-themata "gnosis-dashboard")
(declare-function gnosis-dashboard-themata-by-tag "gnosis-dashboard")
(declare-function gnosis-dashboard-themata-new "gnosis-dashboard")

(defun gnosis-test-dashboard--projection (&optional active-session)
  "Return a server dashboard projection with ACTIVE-SESSION."
  `(:generated_at_us 1 :review_day 42
    :current_streak_days 3
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

(ert-deftest gnosis-entry-opens-dashboard ()
  (let (called)
    (cl-letf (((symbol-function 'gnosis-dashboard)
               (lambda () (interactive) (setq called t))))
      (call-interactively #'gnosis))
    (should called)))

(ert-deftest gnosis-dashboard-summary-uses-only-server-projected-values ()
  (let ((text (gnosis--dashboard-text (gnosis-test-dashboard--projection))))
    (dolist (value '("Connection: Ready"
                     "Reviewed today: 8 (New: 2)"
                     "Due themata: 4 (Overdue: 1)"
                     "Content: 12 themata, 2 documents, 7 nodes"
                     "Last 30 days: 20 reviews, success rate 0.75"
                     "Activity: 4 active days; 0.67/calendar day, 5.00/active day"
                     "Current streak: 3 days"))
      (should (string-match-p (regexp-quote value) text)))
    (should-not (string-match-p "Review day\\|Queue:\\|Active session" text))))

(ert-deftest gnosis-dashboard-summary-hides-active-session-identifiers ()
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
     (string-match-p "private-session-id\\|owner\\|standard" text))))

(defun gnosis-test-dashboard--history-event (id day success)
  "Return public review event ID on DAY with SUCCESS."
  (let ((success (if success t gnosis-core--json-false)))
    `(:event_id ,id :thema_id ,(concat "thema-" id)
      :session_id ,(concat "session-" id) :reviewed_at_us 1
      :review_day ,day :detected_success ,success :accepted_success ,success
      :override_count 0 :transition ,gnosis-core--json-null)))

(ert-deftest gnosis-review-history-renders-public-terminal-pages ()
  (let ((calls 0)
        (gnosis-core--base-url-override "https://example.invalid")
        (gnosis-core--token-override (make-string 40 ?a))
        (first (gnosis-test-dashboard--history-event "event-b" 20260811 t))
        (last (gnosis-test-dashboard--history-event "event-a" 20260810 nil)))
    (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
               (lambda (&rest _) ["https://example.invalid" "digest"]))
              ((symbol-function 'gnosis-core-review-history)
               (lambda (after limit)
                 (should (= limit 100))
                 (cl-incf calls)
                 (pcase after
                   ('nil `(:items [,first] :next "event-b"))
                   ("event-b" `(:items [,last] :next "event-a"))
                   ("event-a" `(:items [] :next ,gnosis-core--json-null)))))
              ((symbol-function 'pop-to-buffer-same-window)
               (lambda (buffer &rest _) (set-buffer buffer)))
              ((symbol-function 'keymap-popup) #'ignore))
      (unwind-protect
          (progn
            (gnosis-review-history)
            (should (derived-mode-p 'gnosis-review-history-mode))
            (should (string-search "Success" (buffer-string)))
            (should-not
             (string-match-p "event-b\\|thema-event-b\\|session-event-b"
                             (buffer-string)))
            (gnosis-review-history-next)
            (should (string-search "Failure" (buffer-string)))
            (gnosis-review-history-next)
            (should (= calls 3))
            (should (eq gnosis-review-history--next gnosis-core--json-null))
            (should (string-search "End of review history" (buffer-string)))
            (should (string-search "Failure" (buffer-string))))
        (when-let* ((buffer (get-buffer gnosis-review-history-buffer-name)))
          (kill-buffer buffer))))))

(ert-deftest gnosis-review-history-replacement-pins-one-authority-pair ()
  (let* ((base-a "https://a.example")
         (base-b "https://b.example")
         (token-a (make-string 40 ?a))
         (token-b (make-string 40 ?b))
         (authority-a (vector base-a (secure-hash 'sha256 token-a)))
         (authority-c ["https://c.example" "digest-c"])
         (event-a (gnosis-test-dashboard--history-event "event-a" 20260811 t))
         (event-c (gnosis-test-dashboard--history-event "event-c" 20260810 nil))
         (base-calls 0)
         (token-calls 0))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority authority-c
            gnosis-review-history--cursor "cursor-c"
            gnosis-review-history--next "next-c"
            gnosis-review-history--previous '("previous-c")
            gnosis-review-history--records (list event-c))
      (gnosis-review-history--render)
      (cl-letf (((symbol-function 'gnosis-core--validated-base-url)
                 (lambda ()
                   (or gnosis-core--base-url-override
                       (progn
                         (cl-incf base-calls)
                         (if (= base-calls 1) base-a base-b)))))
                ((symbol-function 'gnosis-core--token)
                 (lambda ()
                   (or gnosis-core--token-override
                       (progn
                         (cl-incf token-calls)
                         (if (= token-calls 1) token-a token-b)))))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _)
                   (should (equal base-a gnosis-core--base-url-override))
                   (should (equal token-a gnosis-core--token-override))
                   (should (equal authority-a gnosis-review-history--authority))
                   (should-not gnosis-review-history--cursor)
                   (should-not gnosis-review-history--next)
                   (should-not gnosis-review-history--previous)
                   (should-not gnosis-review-history--records)
                   (should (eq 'loading gnosis-review-history--status))
                   `(:items [,event-a] :next ,gnosis-core--json-null))))
        (should (eq 'installed (gnosis-review-history--load nil t)))
        (should (equal authority-a gnosis-review-history--authority))
        (should (equal (list event-a) gnosis-review-history--records))
        (should-not (string-search "Failure" (buffer-string)))
        (should (= base-calls 1))
        (should (= token-calls 1))))))

(ert-deftest gnosis-review-history-replacement-capture-failure-preserves-owner ()
  (let* ((authority-c ["https://c.example" "digest-c"])
         (event-c (gnosis-test-dashboard--history-event "event-c" 20260810 nil)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority authority-c
            gnosis-review-history--cursor "cursor-c"
            gnosis-review-history--next "next-c"
            gnosis-review-history--previous '("previous-c")
            gnosis-review-history--records (list event-c))
      (gnosis-review-history--render)
      (cl-letf (((symbol-function 'gnosis-core--validated-base-url)
                 (lambda ()
                   (signal 'gnosis-core-validation-error '("invalid base")))))
        (should-error (gnosis-review-history--load nil t)
                      :type 'gnosis-core-validation-error))
      (should (equal authority-c gnosis-review-history--authority))
      (should (equal "cursor-c" gnosis-review-history--cursor))
      (should (equal "next-c" gnosis-review-history--next))
      (should (equal '("previous-c") gnosis-review-history--previous))
      (should (equal (list event-c) gnosis-review-history--records))
      (should (string-search "Failure" (buffer-string))))))

(ert-deftest gnosis-review-history-pagination-authority-mismatch-preserves-page ()
  (let* ((authority-a ["https://a.example" "digest-a"])
         (authority-b ["https://b.example" "digest-b"])
         (event-a (gnosis-test-dashboard--history-event "event-a" 20260811 t))
         (gnosis-core--base-url-override "https://b.example")
         (gnosis-core--token-override (make-string 40 ?b)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority authority-a
            gnosis-review-history--cursor "cursor-a"
            gnosis-review-history--next "next-a"
            gnosis-review-history--previous '("previous-a")
            gnosis-review-history--records (list event-a))
      (gnosis-review-history--render)
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) authority-b)))
        (should-error (gnosis-review-history--load "next-a")
                      :type 'gnosis-core-authorization-error))
      (should (equal authority-a gnosis-review-history--authority))
      (should (equal "cursor-a" gnosis-review-history--cursor))
      (should (equal "next-a" gnosis-review-history--next))
      (should (equal '("previous-a") gnosis-review-history--previous))
      (should (equal (list event-a) gnosis-review-history--records))
      (should (string-search "Success" (buffer-string))))))

(ert-deftest gnosis-review-history-replacement-request-error-clears-old-page ()
  (let* ((authority-a ["https://a.example" "digest-a"])
         (authority-c ["https://c.example" "digest-c"])
         (event-c (gnosis-test-dashboard--history-event "event-c" 20260810 nil))
         (gnosis-core--base-url-override "https://a.example")
         (gnosis-core--token-override (make-string 40 ?a)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority authority-c
            gnosis-review-history--cursor "cursor-c"
            gnosis-review-history--next "next-c"
            gnosis-review-history--previous '("previous-c")
            gnosis-review-history--records (list event-c))
      (gnosis-review-history--render)
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) authority-a))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _)
                   (signal 'gnosis-core-unavailable '("private detail")))))
        (should (eq 'error (gnosis-review-history--load nil t))))
      (should (equal authority-a gnosis-review-history--authority))
      (should-not gnosis-review-history--cursor)
      (should-not gnosis-review-history--next)
      (should-not gnosis-review-history--previous)
      (should-not gnosis-review-history--records)
      (should (string-search "Review history unavailable. Press g to retry."
                             (buffer-string)))
      (should-not (string-search "Failure\|private detail" (buffer-string))))))

(ert-deftest gnosis-review-history-stale-result-cannot-overwrite-new-owner ()
  (let* ((authority-a ["https://a.example" "digest-a"])
         (authority-b ["https://b.example" "digest-b"])
         (event-a (gnosis-test-dashboard--history-event "event-a" 20260811 nil))
         (event-b (gnosis-test-dashboard--history-event "event-b" 20260812 t))
         (gnosis-core--base-url-override "https://a.example")
         (gnosis-core--token-override (make-string 40 ?a)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) authority-a))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _)
                   (setq gnosis-review-history--authority authority-b
                         gnosis-review-history--cursor "cursor-b"
                         gnosis-review-history--next "next-b"
                         gnosis-review-history--previous '("previous-b")
                         gnosis-review-history--records (list event-b)
                         gnosis-review-history--status 'loading)
                   (gnosis-review-history--render)
                   `(:items [,event-a] :next ,gnosis-core--json-null))))
        (should (eq 'stale (gnosis-review-history--load nil t))))
      (should (equal authority-b gnosis-review-history--authority))
      (should (equal "cursor-b" gnosis-review-history--cursor))
      (should (equal "next-b" gnosis-review-history--next))
      (should (equal '("previous-b") gnosis-review-history--previous))
      (should (equal (list event-b) gnosis-review-history--records))
      (should (eq 'loading gnosis-review-history--status))
      (should (string-search "Success" (buffer-string)))
      (should-not (string-search "Failure" (buffer-string))))))

(ert-deftest gnosis-review-history-preserves-selection-on-refresh ()
  (let* ((first (gnosis-test-dashboard--history-event "event-b" 20260811 t))
         (second (gnosis-test-dashboard--history-event "event-a" 20260810 nil))
         (page `(:items [,first ,second] :next "event-a"))
         (gnosis-core--base-url-override "https://example.invalid")
         (gnosis-core--token-override (make-string 40 ?a)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority ["https://example.invalid" "digest"])
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) gnosis-review-history--authority))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _) page)))
        (gnosis-review-history--load nil)
        (goto-char (point-min))
        (search-forward "Failure")
        (beginning-of-line)
        (should (equal "event-a" (tabulated-list-get-id)))
        (gnosis-review-history-refresh)
        (should (equal "event-a" (tabulated-list-get-id)))))))

(ert-deftest gnosis-review-history-empty-and-error-states-are-actionable ()
  (let ((gnosis-core--base-url-override "https://example.invalid")
        (gnosis-core--token-override (make-string 40 ?a)))
    (with-temp-buffer
      (gnosis-review-history-mode)
      (setq gnosis-review-history--authority ["https://example.invalid" "digest"])
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) gnosis-review-history--authority))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _)
                   (should (eq gnosis-review-history--status 'loading))
                   `(:items [] :next ,gnosis-core--json-null))))
        (gnosis-review-history--load nil)
        (should (string-search "No review history yet. Press g to refresh."
                               (buffer-string))))
      (cl-letf (((symbol-function 'gnosis-core--authenticated-authority)
                 (lambda (&rest _) gnosis-review-history--authority))
                ((symbol-function 'gnosis-core-review-history)
                 (lambda (&rest _)
                   (signal 'gnosis-core-unavailable '("private detail")))))
        (gnosis-review-history-refresh)
        (should (string-search "Review history unavailable. Press g to retry."
                               (buffer-string)))
        (should-not (string-search "private detail" (buffer-string)))))))

(ert-deftest gnosis-dashboard-summary-renders-missing-success-rate ()
  (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
         (window (plist-get state :window)))
    (setf (plist-get window :success_rate) gnosis-core--json-null)
    (should (string-match-p
             "success rate unavailable" (gnosis--dashboard-text state)))))

(ert-deftest gnosis-dashboard-keymap-restores-archived-navigation-hierarchy ()
  (dolist (binding '(("g" . gnosis-dashboard-refresh)
                     ("r" . gnosis-dashboard-review)
                     ("a" . gnosis-dashboard-add-thema)
                     ("SPC" . gnosis-dashboard-search-themata)
                     ("h" . gnosis-dashboard-health)
                     ("H" . gnosis-review-history)
                     ("T" . gnosis-review-topic)))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map (car binding))
                (cdr binding))))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "n")
              'gnosis-dashboard-mode-map--enter-gnosis-nodes-map))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "t")
              'gnosis-dashboard-mode-map--enter-gnosis-themata-map))
  (should (eq (keymap-lookup gnosis-nodes-map "a")
              #'gnosis-dashboard-nodes))
  (should (eq (keymap-lookup gnosis-nodes-map "t")
              #'gnosis-nodes-find-by-tag))
  (should (eq (keymap-lookup gnosis-themata-map "a")
              #'gnosis-dashboard-themata))
  (should (eq (keymap-lookup gnosis-themata-map "SPC")
              #'gnosis-dashboard-search-themata))
  (should (eq (keymap-lookup gnosis-themata-map "t")
              #'gnosis-dashboard-themata-by-tag))
  (should (eq (keymap-lookup gnosis-themata-map "n")
              #'gnosis-dashboard-themata-new))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (dolist (row (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
    (dolist (group row)
      (should (<= (length (plist-get group :entries)) 4)))))

(ert-deftest gnosis-dashboard-home-fetches-summary-without-thema-catalog ()
  (let ((catalog-calls 0))
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               #'gnosis-test-dashboard--projection)
              ((symbol-function 'gnosis-core-thema-catalog)
               (lambda (&rest _) (cl-incf catalog-calls)))
              ((symbol-function 'keymap-popup) #'ignore)
              ((symbol-function 'pop-to-buffer-same-window)
               (lambda (buffer &rest _) (set-buffer buffer))))
      (unwind-protect
          (progn
            (gnosis-dashboard)
            (with-current-buffer gnosis-dashboard-buffer-name
              (should (derived-mode-p 'gnosis-dashboard-mode))
              (should (string-search "Reviewed today: 8" (buffer-string)))
              (should-not (bound-and-true-p gnosis-themata--ewoc)))
            (should (= 0 catalog-calls)))
        (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
          (kill-buffer buffer))))))

(ert-deftest gnosis-dashboard-themata-mode-restores-archived-view-actions ()
  (dolist (binding '(("q" . gnosis-dashboard)
                     ("SPC" . gnosis-themata-filter-search)
                     ("n" . gnosis-themata-filter-max-reviews)
                     ("RET" . gnosis-themata-edit)
                     ("e" . gnosis-themata-edit)
                     ("a" . gnosis-themata-add)
                     ("s" . gnosis-themata-toggle-suspension)
                     ("S" . gnosis-themata-bulk-suspension)
                     ("d" . gnosis-themata-bulk-archive)
                     ("*" . gnosis-themata-mark)
                     ("u" . gnosis-themata-unmark)
                     ("M" . gnosis-themata-mark-all)
                     ("U" . gnosis-themata-unmark-all)
                     ("t" . gnosis-themata-find-tag)
                     ("f" . gnosis-themata-filter-review-state)
                     ("o" . gnosis-themata-sort)
                     ("g" . gnosis-themata-refresh)))
    (should (eq (keymap-lookup gnosis-dashboard-themata-mode-map (car binding))
                (cdr binding)))))

(ert-deftest gnosis-dashboard-new-themata-route-applies-zero-review-filter ()
  (let (arguments)
    (cl-letf (((symbol-function 'gnosis-dashboard-themata)
               (lambda (&rest supplied) (setq arguments supplied))))
      (gnosis-dashboard-themata-new)
      (should (equal '(nil nil 0) arguments)))))

(ert-deftest gnosis-dashboard-new-themata-prefix-prompts-for-maximum ()
  (let ((current-prefix-arg '(4))
        arguments)
    (cl-letf (((symbol-function 'read-number) (lambda (&rest _) 7))
              ((symbol-function 'gnosis-dashboard-themata)
               (lambda (&rest supplied) (setq arguments supplied))))
      (call-interactively #'gnosis-dashboard-themata-new)
      (should (equal '(nil nil 7) arguments)))))

(ert-deftest gnosis-dashboard-themata-edit-pins-one-authority ()
  (let* ((gnosis-core-base-url "https://test.example")
         (token-a (make-string 40 ?a))
         (token-b (make-string 40 ?b))
         (gnosis-themata--authority
          (vector gnosis-core-base-url (secure-hash 'sha256 token-a)))
         (secret-calls 0)
         observed-token)
    (cl-letf (((symbol-function 'auth-source-search)
               (lambda (&rest _)
                 (list (list :secret
                             (lambda ()
                               (cl-incf secret-calls)
                               (if (= secret-calls 1) token-a token-b))))))
              ((symbol-function 'gnosis-themata--record-at-point)
               (lambda ()
                 (gnosis-themata--ensure-authority)
                 '(:thema_id "thema-a")))
              ((symbol-function 'gnosis-edit-thema)
               (lambda (_id _callback)
                 (setq observed-token (gnosis-core--token)))))
      (gnosis-themata-edit)
      (should (equal token-a observed-token))
      (should (= 1 secret-calls)))))

(ert-deftest gnosis-dashboard-themata-add-pins-one-authority ()
  (let* ((gnosis-core-base-url "https://test.example")
         (token-a (make-string 40 ?a))
         (token-b (make-string 40 ?b))
         (gnosis-themata--authority
          (vector gnosis-core-base-url (secure-hash 'sha256 token-a)))
         (secret-calls 0)
         observed-token)
    (cl-letf (((symbol-function 'auth-source-search)
               (lambda (&rest _)
                 (list (list :secret
                             (lambda ()
                               (cl-incf secret-calls)
                               (if (= secret-calls 1) token-a token-b))))))
              ((symbol-function 'gnosis-edit-read-kind) (lambda () "basic"))
              ((symbol-function 'gnosis-add-thema)
               (lambda (_callback _kind)
                 (setq observed-token (gnosis-core--token)))))
      (gnosis-themata-add)
      (should (equal token-a observed-token))
      (should (= 1 secret-calls)))))

(ert-deftest gnosis-dashboard-popup-describes-start-or-resume ()
  (with-temp-buffer
    (should (equal "Start review" (gnosis-dashboard--review-description)))
    (setq gnosis-dashboard--projection
          (list :active_session '(:status "active")))
    (should (equal "Resume review" (gnosis-dashboard--review-description)))))

(ert-deftest gnosis-dashboard-summary-renders-semantic-faces ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (dolist (pair '(("Connection:" . shadow)
                    ("Ready" . success)
                    ("6 successful" . success)
                    ("2 failed" . error)
                    ("4 active days" . font-lock-type-face)
                    ("3 days" . success)))
      (goto-char (point-min))
      (search-forward (car pair))
      (should (eq (get-text-property (1- (point)) 'face) (cdr pair))))
    (goto-char (point-min))
    (search-forward "Gnosis Dashboard")
    (forward-line 1)
    (should (equal (get-text-property (point) 'display) '(space :width text)))
    (should (eq (get-text-property (point) 'face) 'gnosis-face-separator))))

(ert-deftest gnosis-dashboard-mode-has-native-special-buffer-lifecycle ()
  (with-temp-buffer
    (setq display-line-numbers 'relative)
    (gnosis-dashboard-mode)
    (should (derived-mode-p 'special-mode))
    (should-not display-line-numbers)
    (should (functionp revert-buffer-function))))

(ert-deftest gnosis-dashboard-refresh-preserves-buffer-on-summary-failure ()
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

(ert-deftest gnosis-dashboard-failure-messages-are-public-safe ()
  (should
   (equal (gnosis-dashboard--failure-message
           '(gnosis-core-authentication-error "secret server detail"))
          "Gnosis authentication failed. Check auth-source token for gnosisd."))
  (should
   (equal (gnosis-dashboard--failure-message
           '(gnosis-core-authorization-error "secret server detail"))
          "Gnosis access denied. Check gnosisd token scopes.")))

(ert-deftest gnosis-dashboard-primary-review-action-dispatches-existing-journey ()
  (let (arguments)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda (&rest value) (setq arguments value))))
      (gnosis-dashboard-review))
    (should (equal arguments '(nil)))))

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
