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

(ert-deftest gnosis-dashboard-renders-only-server-projected-values ()
  (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
    (kill-buffer buffer))
  (unwind-protect
      (cl-letf (((symbol-function 'gnosis-core-dashboard)
                 #'gnosis-test-dashboard--projection))
        (save-window-excursion (gnosis-dashboard))
        (with-current-buffer gnosis-dashboard-buffer-name
          (should (derived-mode-p 'gnosis-dashboard-mode))
          (should (eq 'gnosis-face-dashboard-header
                      (get-text-property (point-min) 'face)))
          (dolist (text '("Review day: 42" "Today: 8 reviews"
                          "Queue: 3 new, 4 due, 1 overdue, 2 suspended"
                          "Content: 12 themata, 3 archived, 2 documents, 7 nodes"
                          "30 days: 20 reviews, success rate 0.75"
                          "Active session: none"))
            (should (string-match-p (regexp-quote text) (buffer-string))))))
    (when-let* ((buffer (get-buffer gnosis-dashboard-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-entry-starts-due-review ()
  (let (called)
    (cl-letf (((symbol-function 'gnosis-review)
               (lambda () (interactive) (setq called t))))
      (call-interactively #'gnosis))
    (should called)))

(ert-deftest gnosis-dashboard-shows-session-metadata-without-id ()
  (let ((text
         (gnosis--dashboard-text
          (gnosis-test-dashboard--projection
           '(:session_id "private-session-id" :owner_device_id "owner"
             :mode "standard" :source (:kind "due") :target_count 10
             :status "active" :current_position 3 :queue_length 7
             :started_at_us 1)))))
    (should (string-match-p
             "Active session: active, standard, position 3, queue length 7"
                            text))
    (should-not (string-match-p "private-session-id\\|owner" text))))

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
      (should-error (gnosis-dashboard-refresh) :type 'gnosis-core-unavailable)
      (should (equal "existing projection" (buffer-string))))))

(ert-deftest gnosis-dashboard-keymap-is-direct-and-discoverable ()
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "g")
              #'gnosis-dashboard-refresh))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "r") 'gnosis-review))
  (with-temp-buffer
    (setq gnosis-dashboard--projection
          (list :active_session '(:status "active")))
    (should (eq (keymap-lookup gnosis-dashboard-mode-map "R")
                'gnosis-review-resume)))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "t")
              'gnosis-review-topic))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "q") #'quit-window))
  (should (eq (keymap-lookup gnosis-dashboard-mode-map "?")
              'gnosis-dashboard-mode-map-popup)))

(ert-deftest gnosis-dashboard-popup-describes-resume-exactly ()
  (let* ((rows (keymap-popup--meta gnosis-dashboard-mode-map 'descriptions))
         (review-group (car (car rows)))
         (entry (seq-find
                 (lambda (item) (equal (plist-get item :key) "R"))
                 (plist-get review-group :entries))))
    (should (equal (plist-get entry :description) "Resume review"))
    (should (eq (plist-get entry :if)
                #'gnosis-dashboard--active-session-p))))

(ert-deftest gnosis-dashboard-renders-native-sections-actions-and-state-faces ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard (gnosis-test-dashboard--projection))
    (dolist (text '("Review day" "Today" "Queue" "Content"
                    "Last 30 days" "Active session" "Review due"
                    "Refresh" "Actions" "Quit"))
      (should (string-search text (buffer-string))))
    (goto-char (point-min))
    (search-forward "6 successful")
    (should (eq (get-text-property (1- (point)) 'face) 'success))
    (search-forward "2 failed")
    (should (eq (get-text-property (1- (point)) 'face) 'error))
    (search-forward "Active session")
    (search-forward "none")
    (should (eq (get-text-property (1- (point)) 'face) 'shadow))))

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
                     #'gnosis-test-dashboard--projection))
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

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
