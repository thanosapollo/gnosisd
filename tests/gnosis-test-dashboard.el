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
        (save-window-excursion (gnosis))
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

(ert-deftest gnosis-dashboard-refresh-preserves-buffer-on-failure ()
  (with-temp-buffer
    (gnosis-dashboard-mode)
    (let ((inhibit-read-only t)) (insert "existing projection"))
    (cl-letf (((symbol-function 'gnosis-core-dashboard)
               (lambda (&rest _) (signal 'gnosis-core-unavailable '("down")))))
      (should-error (gnosis-dashboard-refresh) :type 'gnosis-core-unavailable)
      (should (equal "existing projection" (buffer-string))))))

(provide 'gnosis-test-dashboard)
;;; gnosis-test-dashboard.el ends here
