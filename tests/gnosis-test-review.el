;;; gnosis-test-review.el --- Online review UI tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'cl-lib)
(require 'gnosis-review)

(defun gnosis-test-review--session (&optional status)
  "Return a minimal session with STATUS."
  (list :session_id "session" :status (or status "active")
        :current_position 0 :queue_length 2))

(defun gnosis-test-review--attempt (&optional status prompt)
  "Return a minimal attempt with STATUS and response PROMPT."
  (list :attempt_id "attempt" :session_id "session" :thema_id "thema"
        :presentation
        (list :question "Question?"
              :response (or prompt
                            (list :kind "text"
                                  :input_script gnosis-core--json-null))
              :review_image gnosis-core--json-null)
        :revealed_answer (if (equal status "pending")
                             gnosis-core--json-null ["Answer"])
        :revealed_parathema gnosis-core--json-null
        :accepted_success (if (equal status "pending")
                              gnosis-core--json-null t)
        :desired_suspended gnosis-core--json-false
        :status (or status "pending")))

(ert-deftest gnosis-review-source-is-closed-due-view ()
  (let ((source (gnosis-review--source "due")))
    (should (equal "due" (plist-get source :kind)))
    (should (vectorp (plist-get source :tags)))
    (should (eq gnosis-core--json-null (plist-get source :topic_node_id)))
    (should (equal [] (plist-get source :manual_ids)))))

(ert-deftest gnosis-review-empty-session-does-not-begin-attempt ()
  (let ((session (list :session_id "session" :status "completed"
                       :current_position 0 :queue_length 0))
        began)
    (cl-letf (((symbol-function 'gnosis-core-review-attempt-begin)
               (lambda (&rest _) (setq began t)))
              ((symbol-function 'message) #'ignore))
      (gnosis-review--open session)
      (should-not began))))

(ert-deftest gnosis-review-open-preserves-buffer-when-begin-fails ()
  (let ((buffer (get-buffer-create gnosis-review-buffer-name))
        popped)
    (unwind-protect
        (progn
          (with-current-buffer buffer
            (erase-buffer)
            (insert "keep"))
          (cl-letf (((symbol-function 'gnosis-core-review-attempt-begin)
                     (lambda (&rest _)
                       (signal 'gnosis-core-unavailable '("down"))))
                    ((symbol-function 'pop-to-buffer)
                     (lambda (&rest _) (setq popped t))))
            (should-error
             (gnosis-review--open (gnosis-test-review--session))
             :type 'gnosis-core-unavailable))
          (should-not popped)
          (with-current-buffer buffer
            (should (equal "keep" (buffer-string)))))
      (kill-buffer buffer))))

(ert-deftest gnosis-review-reads-server-directed-structured-values ()
  (let ((gnosis-review--attempt
         (gnosis-test-review--attempt
          "pending" (list :kind "texts"
                          :input_scripts [gnosis-core--json-null "greek"])))
        (answers '("one" "δύο")))
    (cl-letf (((symbol-function 'gnosis-review--read-string)
               (lambda (&rest _) (pop answers))))
      (should
       (equal '(:kind "texts" :value ["one" "δύο"])
              (gnosis-review--read-answer)))))
  (let ((gnosis-review--attempt
         (gnosis-test-review--attempt
          "pending" (list :kind "choice" :choices ["A" "B"])))
        arguments)
    (cl-letf (((symbol-function 'gnosis-completing-read)
               (lambda (&rest args) (setq arguments args) "B")))
      (should (equal '(:kind "choice" :value "B")
                     (gnosis-review--read-answer)))
      (should (eq t (nth 2 arguments))))))

(ert-deftest gnosis-review-render-never-grades-or-discloses-pending-answer ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (setf (plist-get gnosis-review--attempt :revealed_answer) ["SECRET"])
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil))
              ((symbol-function 'gnosis-core-review-attempt-answer)
               (lambda (&rest _) (ert-fail "render answered")))
              ((symbol-function 'gnosis-core-review-attempt-commit)
               (lambda (&rest _) (ert-fail "render committed"))))
      (gnosis-review--render)
      (should (string-match-p "RET: answer" (buffer-string)))
      (should-not (string-match-p "SECRET" (buffer-string))))))

(ert-deftest gnosis-review-answer-submits-one-structured-mutation ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (let (submitted)
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "response")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (_id answer)
                   (setq submitted answer)
                   (gnosis-test-review--attempt "graded")))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (_id)
                   (list :attempt_revision 2
                         :attempt (gnosis-test-review--attempt "graded"))))
                ((symbol-function 'gnosis-review--render) #'ignore))
        (gnosis-review--answer)
        (should (equal '(:kind "text" :value "response") submitted))
        (should (= 2 gnosis-review--attempt-revision))))))

(ert-deftest gnosis-review-answer-uncertainty-recovers-with-get-not-retry ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (let ((answers 0) (reads 0))
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "response")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf answers)
                   (signal 'gnosis-core-mutation-uncertain '("unknown"))))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (_id)
                   (cl-incf reads)
                   (list :attempt_revision 2
                         :attempt (gnosis-test-review--attempt "graded"))))
                ((symbol-function 'gnosis-review--render) #'ignore))
        (gnosis-review--answer)
        (should (= answers 1))
        (should (= reads 1))
        (should (equal "graded" (plist-get gnosis-review--attempt :status)))))))

(ert-deftest gnosis-review-finish-orders-authoritative-state-before-commit ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 3)
    (let (events)
      (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (_id revision desired)
                   (push (list 'set revision desired) events)
                   (list :attempt_revision 4
                         :attempt (gnosis-test-review--attempt "graded"))))
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (_id) (push 'commit events)))
                ((symbol-function 'gnosis-review--next)
                 (lambda () (push 'next events))))
        (gnosis-review--finish gnosis-core--json-false)
        (should (equal `((set 3 ,gnosis-core--json-false) commit next)
                       (nreverse events)))))))

(ert-deftest gnosis-review-desired-state-conflict-refreshes-authority ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 3)
    (let (events)
      (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-suspension)
                 (lambda (&rest _)
                   (signal 'gnosis-core-conflict '("stale"))))
                ((symbol-function 'gnosis-review--load-revision)
                 (lambda () (push 'refresh events)))
                ((symbol-function 'gnosis-review--render)
                 (lambda () (push 'render events))))
        (should-error (gnosis-review--toggle-suspension) :type 'user-error)
        (should (equal '(render refresh) events))))))

(ert-deftest gnosis-review-commit-uncertainty-recovers-committed-attempt ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 3)
    (let ((commits 0) events)
      (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (&rest _)
                   (list :attempt_revision 3
                         :attempt (gnosis-test-review--attempt "graded"))))
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (_id)
                   (cl-incf commits)
                   (signal 'gnosis-core-mutation-uncertain '("unknown"))))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (_id)
                   (list :attempt_revision 4
                         :attempt (gnosis-test-review--attempt "committed"))))
                ((symbol-function 'gnosis-review--next)
                 (lambda () (push 'next events))))
        (gnosis-review--finish t)
        (should (= commits 1))
        (should (equal '(next) events))))))

(ert-deftest gnosis-review-next-observes-completion-without-resume-mutation ()
  (let ((gnosis-review--session (gnosis-test-review--session))
        resumed)
    (cl-letf (((symbol-function 'gnosis-review--active-session)
               (lambda () nil))
              ((symbol-function 'gnosis-review--resume-session)
               (lambda (&rest _) (setq resumed t)))
              ((symbol-function 'kill-buffer) #'ignore)
              ((symbol-function 'message) #'ignore))
      (gnosis-review--next)
      (should-not resumed))))

(ert-deftest gnosis-review-resume-cancel-does-not-mutate ()
  (let (mutated)
    (cl-letf (((symbol-function 'gnosis-core-review-session-active)
               (lambda () (list :session (gnosis-test-review--session))))
              ((symbol-function 'read-char-choice) (lambda (&rest _) ?c))
              ((symbol-function 'gnosis-core-review-session-resume)
               (lambda (&rest _) (setq mutated t)))
              ((symbol-function 'gnosis-core-review-session-abort)
               (lambda (&rest _) (setq mutated t)))
              ((symbol-function 'message) #'ignore))
      (gnosis-review-resume)
      (should-not mutated))))

(provide 'gnosis-test-review)
;;; gnosis-test-review.el ends here
