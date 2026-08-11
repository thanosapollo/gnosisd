;;; gnosis-test-dashboard.el --- Dashboard summary tests  -*- lexical-binding: t; -*-

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
                     "Last 30 days: 20 reviews, success rate 0.75"))
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
                     ("T" . gnosis-review-topic)))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map (car binding))
                (cdr binding))))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "?")
              'gnosis-dashboard-mode-map-popup))
  (dolist (row (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
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
                    ("3 archived" . shadow)))
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
