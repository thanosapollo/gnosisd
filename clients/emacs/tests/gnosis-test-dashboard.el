;;; gnosis-test-dashboard.el --- Dashboard summary tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis)
(require 'gnosis-dashboard)

(defvar gnosis-review-history-buffer-name)
(defvar gnosis-review-history--authority)
(defvar gnosis-review-history--next)
(defvar gnosis-review-history--status)

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
                     "Content: 12 themata, 3 archived, 2 documents, 7 nodes"
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

(ert-deftest gnosis-review-history-preserves-selection-on-refresh ()
  (let* ((first (gnosis-test-dashboard--history-event "event-b" 20260811 t))
         (second (gnosis-test-dashboard--history-event "event-a" 20260810 nil))
         (page `(:items [,first ,second] :next "event-a")))
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
      (should-not (string-search "private detail" (buffer-string))))))

(ert-deftest gnosis-dashboard-summary-renders-missing-success-rate ()
  (let* ((state (copy-tree (gnosis-test-dashboard--projection)))
         (window (plist-get state :window)))
    (setf (plist-get window :success_rate) gnosis-core--json-null)
    (should (string-match-p
             "success rate unavailable" (gnosis--dashboard-text state)))))

(ert-deftest gnosis-dashboard-keymap-is-direct-and-bounded ()
  (dolist (binding '(("g" . gnosis-dashboard-refresh)
                     ("r" . gnosis-dashboard-review)
                     ("n" . gnosis-nodes-find)
                     ("t" . gnosis-dashboard-themata)
                     ("v" . gnosis-themata-find-tag)
                     ("o" . gnosis-themata-sort)
                     (">" . gnosis-themata-widen-current-column)
                     ("<" . gnosis-themata-narrow-current-column)
                     ("e" . gnosis-themata-edit)
                     ("a" . gnosis-themata-add)
                     ("h" . gnosis-review-history)
                     ("T" . gnosis-review-topic)))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map (car binding))
                (cdr binding))))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "?")
              'gnosis-dashboard-mode-map-popup))
  (dolist (row (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
    (dolist (group row)
      (should (<= (length (plist-get group :entries)) 4))))
  (dolist (binding '(("n" . gnosis-review-history-next)
                     ("p" . gnosis-review-history-previous)
                     ("g" . gnosis-review-history-refresh)
                     ("q" . quit-window)))
    (should (eq (keymap-lookup gnosis-review-history-mode-map (car binding))
                (cdr binding))))
  (dolist (row (keymap-popup--meta gnosis-review-history-mode-map 'descriptions))
    (dolist (group row)
      (should (<= (length (plist-get group :entries)) 4)))))

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
                    ("3 archived" . shadow)
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
