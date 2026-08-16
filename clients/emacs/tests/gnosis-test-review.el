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
        (list :kind "basic" :question "Question?" :cloze_spans []
              :hint gnosis-core--json-null
              :response (or prompt
                            (list :kind "text"
                                  :input_script gnosis-core--json-null))
              :review_image gnosis-core--json-null)
        :revealed_answer (if (equal status "pending")
                             gnosis-core--json-null ["Answer"])
        :revealed_parathema gnosis-core--json-null
        :response (if (equal status "pending")
                      gnosis-core--json-null
                    '(:kind "text" :value "response"))
        :detected_success (if (equal status "pending")
                              gnosis-core--json-null t)
        :accepted_success (if (equal status "pending")
                              gnosis-core--json-null t)
        :desired_suspended gnosis-core--json-false
        :proposed_transition gnosis-core--json-null
        :status (or status "pending")))

(defun gnosis-test-review--begin-result (&optional attempt session)
  "Return a begin envelope for ATTEMPT and SESSION."
  (let ((session (or session (gnosis-test-review--session))))
    (list :session session
          :attempt (or attempt (gnosis-test-review--attempt "pending"))
          :start_position (plist-get session :current_position)
          :skipped [])))


(ert-deftest gnosis-review-map-is-direct-state-aware-and-discoverable ()
  (dolist (case `(("pending" "RET" gnosis-review--answer)
                  ("pending" "e" gnosis-review--edit)
                  ("graded" "n" gnosis-review--commit-next)
                  ("graded" "o" gnosis-review--override)
                  ("graded" "s" gnosis-review--toggle-suspension)
                  ("pending" "g" gnosis-review--refresh)
                  ("pending" "q" gnosis-review--quit)
                  ("pending" "?" gnosis-review-mode-map-popup)))
    (with-temp-buffer
      (setq gnosis-review--attempt (gnosis-test-review--attempt (nth 0 case))
            gnosis-review--attempt-revision 1)
      (should (eq (keymap-lookup gnosis-review-mode-map (nth 1 case))
                  (nth 2 case)))))
  (should (equal "C-g" (keymap-popup--meta gnosis-review-mode-map 'exit-key)))
  (let ((rows (keymap-popup--meta gnosis-review-mode-map 'descriptions)))
    (should (equal
             '(("Review" "State" "Session"))
             (mapcar
              (lambda (row)
                (mapcar (lambda (group) (plist-get group :name)) row))
              rows)))
    (dolist (row rows)
      (dolist (group row)
        (should (<= (length (plist-get group :entries)) 4))))
    (should-not
     (seq-some (lambda (entry) (eq (plist-get entry :type) 'switch))
               (mapcan (lambda (row)
                         (mapcan (lambda (group)
                                   (copy-sequence (plist-get group :entries)))
                                 row))
                       rows)))))

(ert-deftest gnosis-review-actions-preserve-popup-only-for-local-state ()
  (let ((rows (keymap-popup--meta gnosis-review-mode-map 'descriptions)))
    (dolist (key '("RET" "e" "n" "g"))
      (should-not
       (plist-get (keymap-popup--find-entry-by-key rows key) :stay-open)))
    (dolist (key '("o" "s"))
      (should
       (plist-get
        (keymap-popup--find-entry-by-key rows key) :stay-open)))))

(ert-deftest gnosis-review-pending-attempt-keeps-mutations-visible-but-inapt ()
  (with-temp-buffer
    (setq gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (dolist (key '("n" "o" "s"))
      (should (commandp (keymap-lookup gnosis-review-mode-map key))))
    (let* ((rows (keymap-popup--meta gnosis-review-mode-map 'descriptions))
           (attempt-group (car (car rows))))
      (dolist (entry (cdr (plist-get attempt-group :entries)))
        (should (eq (plist-get entry :inapt-if)
                    #'gnosis-review--pending-p))
        (should-not (plist-get entry :if))))))

(ert-deftest gnosis-review-committed-attempt-disables-mutations ()
  (with-temp-buffer
    (setq gnosis-review--attempt (gnosis-test-review--attempt "committed")
          gnosis-review--attempt-revision 2)
    (dolist (command
             (list (lambda ()
                     (gnosis-review--finish-authorized
                      (plist-get gnosis-review--attempt :accepted_success)))
                   #'gnosis-review--override-current
                   #'gnosis-review--toggle-suspension-current))
      (let (mutations)
        (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                   (lambda (&rest _) (push 'accepted mutations)))
                  ((symbol-function 'gnosis-core-review-attempt-set-suspension)
                   (lambda (&rest _) (push 'suspended mutations)))
                  ((symbol-function 'gnosis-core-review-attempt-commit)
                   (lambda (&rest _) (push 'committed mutations))))
          (should-error (funcall command) :type 'user-error)
          (should-not mutations))))))

(ert-deftest gnosis-review-refresh-advances-committed-attempt-from-authority ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "committed")
          gnosis-review--attempt-revision 2)
    (let ((next-session (gnosis-test-review--session))
          (next-attempt (gnosis-test-review--attempt "pending"))
          mutations)
      (setf (plist-get next-session :current_position) 1
            (plist-get next-attempt :attempt_id) "attempt-2")
      (cl-letf (((symbol-function 'gnosis-core-review-session-active)
                 (lambda (&rest _) (list :session next-session)))
                ((symbol-function 'gnosis-core-review-attempt-begin)
                 (lambda (&rest _)
                   (gnosis-test-review--begin-result next-attempt next-session)))
                ((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (&rest _) (push 'accepted mutations)))
                ((symbol-function 'gnosis-core-review-attempt-set-suspension)
                 (lambda (&rest _) (push 'suspended mutations)))
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (&rest _) (push 'committed mutations)))
                ((symbol-function 'gnosis-review--answer) #'ignore)
                ((symbol-function 'display-images-p) (lambda () nil)))
        (gnosis-review--refresh-current)
        (should (equal "attempt-2" (plist-get gnosis-review--attempt :attempt_id)))
        (should-not mutations)))))

(ert-deftest gnosis-review-header-and-popup-omit-private-state ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
    (cl-letf (((symbol-function 'window-body-width) (lambda (&rest _) 30)))
      (let ((header (gnosis-review--header-line))
            (description (gnosis-review--popup-description)))
        (should (equal "          Review 1/2"
                       (substring-no-properties header)))
        (should (eq (get-text-property 17 'face header)
                    'font-lock-type-face))
        (should (equal "Review 1/2 — answer" description))
        (should-not
         (string-match-p "session\\|attempt\\|thema" description))))))

(ert-deftest gnosis-review-header-converts-zero-based-session-position ()
  (with-temp-buffer
    (setq gnosis-review--session (gnosis-test-review--session))
    (setf (plist-get gnosis-review--session :current_position) 1)
    (should
     (string-prefix-p
      "Review 2/2" (string-trim-left (gnosis-review--header-line))))))

(ert-deftest gnosis-review-complete-does-not-crash-header-after-clearing-session ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt))
    (cl-letf (((symbol-function 'message)
               (lambda (&rest _)
                 (gnosis-review--header-line)
                 (gnosis-review--popup-description)))
              ((symbol-function 'kill-buffer) #'ignore))
      (gnosis-review--complete)
      (should-not gnosis-review--session)
      (should (equal "" (gnosis-review--header-line)))
      (should (string-prefix-p "Review" (gnosis-review--popup-description))))))

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

(ert-deftest gnosis-review-begin-skips-to-completion-without-opening-buffer ()
  (let ((session (gnosis-test-review--session))
        (completed (gnosis-test-review--session "completed"))
        message
        popped)
    (setf (plist-get completed :current_position) 2)
    (cl-letf (((symbol-function 'gnosis-core-review-attempt-begin)
               (lambda (&rest _)
                 (gnosis-test-review--begin-result
                  gnosis-core--json-null completed)))
              ((symbol-function 'pop-to-buffer-same-window)
               (lambda (&rest _) (setq popped t)))
              ((symbol-function 'message)
               (lambda (format &rest arguments)
                 (setq message (apply #'format format arguments)))))
      (gnosis-review--open session)
      (should-not popped)
      (should (equal message "No Gnosis review items are available")))))

(ert-deftest gnosis-review-empty-queue-is-a-normal-user-outcome ()
  (let (message)
    (cl-letf (((symbol-function 'gnosis-core-review-session-create)
                 (lambda (&rest _arguments)
                   (signal 'gnosis-core-not-found '("empty"))))
                ((symbol-function 'message)
                 (lambda (format &rest arguments)
                   (setq message (apply #'format format arguments))))
                ((symbol-function 'gnosis-review--open)
                 (lambda (&rest _arguments) (ert-fail "must not open"))))
      (should-not
       (gnosis-review--create (gnosis-review--source "due") nil))
      (should (equal message "No Gnosis review items are available")))))

(ert-deftest gnosis-review-active-session-action-uses-completion ()
  (let (arguments)
    (cl-letf (((symbol-function 'completing-read)
               (lambda (&rest values)
                 (setq arguments values)
                 "Resume session")))
      (should
       (equal "Resume session"
              (gnosis-review--read-active-session-action)))
      (should
       (equal
        '("Active session found: "
          ("Resume session" "Start new" "Cancel")
          nil t nil nil "Resume session")
        arguments)))))

(ert-deftest gnosis-review-start-resumes-active-session ()
  (let ((session (gnosis-test-review--session))
        events)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-review--active-session)
               (lambda () session))
              ((symbol-function 'gnosis-review--read-active-session-action)
               (lambda () "Resume session"))
              ((symbol-function 'gnosis-review--resume-session)
               (lambda (session-id)
                 (push (list 'resume session-id) events)
                 session))
              ((symbol-function 'gnosis-review--open)
               (lambda (value) (push (list 'open value) events)))
              ((symbol-function 'gnosis-review--create)
               (lambda (&rest _) (ert-fail "must not create"))))
      (gnosis-review--start (gnosis-review--source "due") 20)
      (should
       (equal `((resume "session") (open ,session))
              (nreverse events))))))

(ert-deftest gnosis-review-start-replaces-active-session-atomically ()
  (let ((source (gnosis-review--source "due"))
        events)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-review--active-session)
               (lambda () (gnosis-test-review--session)))
              ((symbol-function 'gnosis-review--read-active-session-action)
               (lambda () "Start new"))
              ((symbol-function 'gnosis-review--quit-session)
               (lambda (&rest _) (ert-fail "must not quit separately")))
              ((symbol-function 'gnosis-review--create)
               (lambda (value count replace-session-id)
                 (push (list 'replace value count replace-session-id) events))))
      (gnosis-review--start source 20)
      (should
       (equal `((replace ,source 20 "session"))
              (nreverse events))))))

(ert-deftest gnosis-review-empty-replacement-never-opens-cached-session ()
  (let ((session (gnosis-test-review--session))
        (source (gnosis-review--source "due"))
        events)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-review--active-session)
               (lambda () session))
              ((symbol-function 'gnosis-review--read-active-session-action)
               (lambda () "Start new"))
              ((symbol-function 'gnosis-review--quit-session)
               (lambda (&rest _) (ert-fail "must not quit separately")))
              ((symbol-function 'gnosis-core-review-session-replace)
               (lambda (replace-session-id value count)
                 (push (list 'replace value count replace-session-id) events)
                 (signal 'gnosis-core-not-found '("empty"))))
              ((symbol-function 'gnosis-review--resume-session)
               (lambda (&rest _) (ert-fail "must remain active")))
              ((symbol-function 'gnosis-review--open)
               (lambda (&rest _) (ert-fail "must not open cached session")))
              ((symbol-function 'message) #'ignore))
      (gnosis-review--start source 20)
      (should
       (equal `((replace ,source 20 "session"))
              (nreverse events))))))

(ert-deftest gnosis-review-start-cancel-does-not-mutate ()
  (let (mutated)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-review--active-session)
               (lambda () (gnosis-test-review--session)))
              ((symbol-function 'gnosis-review--read-active-session-action)
               (lambda () "Cancel"))
              ((symbol-function 'gnosis-review--quit-session)
               (lambda (&rest _) (setq mutated t)))
              ((symbol-function 'gnosis-review--resume-session)
               (lambda (&rest _) (setq mutated t)))
              ((symbol-function 'gnosis-review--create)
               (lambda (&rest _) (setq mutated t)))
              ((symbol-function 'message) #'ignore))
      (gnosis-review--start (gnosis-review--source "due") nil)
      (should-not mutated))))

(ert-deftest gnosis-review-quit-uncertainty-confirms-inactive-session ()
  (let (checked)
    (cl-letf (((symbol-function 'gnosis-core-review-session-quit)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-review--active-session)
               (lambda ()
                 (setq checked t)
                 nil)))
      (gnosis-review--quit-session "session")
      (should checked))))

(ert-deftest gnosis-review-quit-uncertainty-preserves-active-session ()
  (cl-letf (((symbol-function 'gnosis-core-review-session-quit)
             (lambda (&rest _)
               (signal 'gnosis-core-mutation-uncertain '("unknown"))))
            ((symbol-function 'gnosis-review--active-session)
             (lambda () (gnosis-test-review--session))))
    (should-error (gnosis-review--quit-session "session")
                  :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-review-abort-uncertainty-requires-target-absence ()
  (dolist (active (list nil (list :session_id "replacement")))
    (cl-letf (((symbol-function 'gnosis-core-review-session-abort)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-review--active-session)
               (lambda () active)))
      (should-not (gnosis-review--abort-session "session"))))
  (cl-letf (((symbol-function 'gnosis-core-review-session-abort)
             (lambda (&rest _)
               (signal 'gnosis-core-mutation-uncertain '("unknown"))))
            ((symbol-function 'gnosis-review--active-session)
             (lambda () (gnosis-test-review--session))))
    (should-error (gnosis-review--abort-session "session")
                  :type 'gnosis-core-mutation-uncertain)))

(ert-deftest gnosis-review-abort-uncertainty-survives-failed-authority-read ()
  (cl-letf (((symbol-function 'gnosis-core-review-session-abort)
             (lambda (&rest _)
               (signal 'gnosis-core-mutation-uncertain '("unknown"))))
            ((symbol-function 'gnosis-review--active-session)
             (lambda () (signal 'gnosis-core-unavailable '("offline")))))
    (should-error (gnosis-review--abort-session "session")
                  :type 'gnosis-core-mutation-uncertain)))

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
                    ((symbol-function 'pop-to-buffer-same-window)
                     (lambda (&rest _) (setq popped t))))
            (should-error
             (gnosis-review--open (gnosis-test-review--session))
             :type 'gnosis-core-unavailable))
          (should-not popped)
          (with-current-buffer buffer
            (should (equal "keep" (buffer-string)))))
      (kill-buffer buffer))))

(ert-deftest gnosis-review-rejects-authority-drift-before-answer ()
  (with-temp-buffer
    (gnosis-review-mode)
    (let* ((token-a (make-string 40 ?a))
           (token-b (make-string 40 ?b))
           (buffer (current-buffer))
           (authority-a
            (let ((gnosis-core--base-url-override "https://a.example"))
              (gnosis-core--authenticated-authority token-a)))
           (reads 0)
           (posts 0)
           (state-reads 0)
           (window-effects 0))
      (setq gnosis-review--session (gnosis-test-review--session)
            gnosis-review--attempt (gnosis-test-review--attempt "pending")
            gnosis-review--authority authority-a)
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda ()
                   (cl-incf reads)
                   '(:kind "text" :value "private answer")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf posts)
                   (gnosis-test-review--attempt "graded")))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (&rest _)
                   (cl-incf state-reads)
                   (list :attempt_revision 2
                         :attempt (gnosis-test-review--attempt "graded"))))
                ((symbol-function 'pop-to-buffer-same-window)
                 (lambda (&rest _) (cl-incf window-effects))))
        (let ((gnosis-core--base-url-override "https://b.example")
              (gnosis-core--token-override token-b))
          (should-error (gnosis-review--answer)
                        :type 'gnosis-core-authorization-error)
          (should-error (gnosis-review--edited
                         buffer authority-a "session" "attempt" nil)
                        :type 'gnosis-core-authorization-error))
        (should (= reads 0))
        (should (= posts 0))
        (should (= state-reads 0))
        (should (= window-effects 0))))))

(ert-deftest gnosis-review-missing-authority-rejects-all-buffer-actions ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 2
          gnosis-review--authority nil)
    (let ((buffer (current-buffer))
          (effects 0)
          (window-effects 0))
      (cl-letf (((symbol-function 'gnosis-review--next-current)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--answer-authorized)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--finish-authorized)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--override-current)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--toggle-suspension-current)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--refresh-current)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'gnosis-review--edit-current)
                 (lambda (&rest _) (cl-incf effects)))
                ((symbol-function 'pop-to-buffer-same-window)
                 (lambda (&rest _) (cl-incf window-effects))))
        (dolist (action
                 (list #'gnosis-review--next
                       #'gnosis-review--answer
                       (lambda () (gnosis-review--finish t))
                       #'gnosis-review--commit-next
                       #'gnosis-review--override
                       #'gnosis-review--toggle-suspension
                       #'gnosis-review--refresh
                       #'gnosis-review--edit))
          (should-error (funcall action)
                        :type 'gnosis-core-authorization-error))
        (should-not
         (gnosis-review--edited
          buffer '(:captured t) "session" "attempt" nil))
        (should (= effects 0))
        (should (= window-effects 0))))))

(ert-deftest gnosis-review-stale-answer-reconciles-to-next-attempt ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (let ((next-session (gnosis-test-review--session))
          (next-attempt (gnosis-test-review--attempt "pending"))
          presented)
      (setf (plist-get next-session :current_position) 1
            (plist-get next-attempt :attempt_id) "attempt-2")
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "answer")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (signal 'gnosis-core-conflict
                           '("Review attempt changed" "stale_attempt"))))
                ((symbol-function 'gnosis-review--active-session)
                 (lambda () next-session))
                ((symbol-function 'gnosis-core-review-attempt-begin)
                 (lambda (_)
                   (gnosis-test-review--begin-result next-attempt next-session)))
                ((symbol-function 'gnosis-review--present)
                 (lambda (&rest _) (setq presented t))))
        (gnosis-review--answer-authorized)
        (should presented)
        (should (equal "attempt-2"
                       (plist-get gnosis-review--attempt :attempt_id)))
        (should-not gnosis-review--answer-locked-p)))))

(ert-deftest gnosis-review-malformed-stale-error-does-not-advance ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (let (advanced)
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "answer")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (gnosis-core--signal-http-error
                    412
                    '(:error (:code "stale_attempt" :message "changed"
                              :meta :gnosis-json-null)
                      :error (:code "review_state_conflict" :message "other"
                              :meta :gnosis-json-null)))))
                ((symbol-function 'gnosis-review--next-current)
                 (lambda () (setq advanced t))))
        (should-error (gnosis-review--answer-authorized)
                      :type 'gnosis-core-protocol-error)
        (should-not advanced)))))

(ert-deftest gnosis-review-stale-finish-reconciles-without-commit ()
  (with-temp-buffer
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 2)
    (let (committed reconciled)
      (cl-letf (((symbol-function
                  'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (&rest _)
                   (signal 'gnosis-core-conflict
                           '("Review attempt changed" "stale_attempt"))))
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (&rest _) (setq committed t)))
                ((symbol-function 'gnosis-review--next-current)
                 (lambda () (setq reconciled t))))
        (gnosis-review--finish-authorized t)
        (should reconciled)
        (should-not committed)))))

(ert-deftest gnosis-review-open-pending-attempt-can-answer-immediately-once ()
  (let* ((buffer (get-buffer-create gnosis-review-buffer-name))
         (token (make-string 40 ?a))
         (gnosis-core--base-url-override "https://test.example")
         (gnosis-core--token-override token)
         (gnosis-review--opening-authority
          (gnosis-core--authenticated-authority token))
         (posts 0)
         events)
    (unwind-protect
        (cl-letf (((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup)
                   (lambda (&rest _) (push 'popup events)))
                  ((symbol-function 'gnosis-core-review-attempt-begin)
                   (lambda (_)
                     (gnosis-test-review--begin-result
                      (gnosis-test-review--attempt "pending"))))
                  ((symbol-function 'gnosis-review--read-answer)
                   (lambda ()
                     (push 'input events)
                     '(:kind "text" :value "response")))
                  ((symbol-function 'gnosis-core-review-attempt-answer)
                   (lambda (&rest _)
                     (setq posts (1+ posts))
                     (gnosis-test-review--attempt "graded")))
                  ((symbol-function 'gnosis-core-review-attempt-state)
                   (lambda (_)
                     (list :attempt_revision 2
                           :attempt (gnosis-test-review--attempt "graded")))))
          (gnosis-review--open (gnosis-test-review--session))
          (should (= posts 1))
          (should (equal '(input popup) (nreverse events)))
          (should (equal gnosis-review--opening-authority
                         gnosis-review--authority))
          (should (eq (keymap-lookup gnosis-review-mode-map "RET")
                      nil)))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-review-next-attempt-opens-input-by-default ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "committed"))
    (let ((answers 0))
      (cl-letf (((symbol-function 'gnosis-review--active-session)
                 (lambda () (gnosis-test-review--session)))
                ((symbol-function 'gnosis-core-review-attempt-begin)
                 (lambda (&rest _)
                   (gnosis-test-review--begin-result
                    (gnosis-test-review--attempt "pending"))))
                ((symbol-function 'gnosis-review--answer)
                 (lambda (&optional _) (setq answers (1+ answers))))
                ((symbol-function 'display-images-p) (lambda () nil)))
        (gnosis-review--next-current)
        (should (= answers 1))))))

(ert-deftest gnosis-review-mode-disables-line-numbers ()
  (with-temp-buffer
    (setq display-line-numbers 'relative)
    (gnosis-review-mode)
    (should-not display-line-numbers)))

(ert-deftest gnosis-review-reopening-preserves-local-state-and-opens-popup ()
  (let ((buffer (get-buffer-create gnosis-review-buffer-name))
        popups)
    (unwind-protect
        (cl-letf (((symbol-function 'pop-to-buffer-same-window)
                   (lambda (target &rest _) (set-buffer target)))
                  ((symbol-function 'keymap-popup)
                   (lambda (map) (push map popups)))
                  ((symbol-function 'gnosis-core-review-attempt-begin)
                   (lambda (_)
                     (gnosis-test-review--begin-result
                      (gnosis-test-review--attempt "graded"))))
                  ((symbol-function 'display-images-p) (lambda () nil)))
          (gnosis-review--open (gnosis-test-review--session))
          (with-current-buffer buffer
            (setq-local gnosis-center-content nil)
            (setq gnosis-review--answer-locked-p t))
          (gnosis-review--open (gnosis-test-review--session))
          (with-current-buffer buffer
            (should-not gnosis-center-content)
            (should-not gnosis-review--answer-locked-p))
          (should (equal popups
                         (list gnosis-review-mode-map
                               gnosis-review-mode-map))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-review-edit-reconciles-changed-thema-through-begin ()
  (let* ((buffer (generate-new-buffer " *gnosis-review-edit-test*"))
         (token (make-string 40 ?a))
         (gnosis-core--base-url-override "https://test.example")
         (gnosis-core--token-override token)
         callback
         begun-session
         opened-id
         presented-buffer
         returned-buffer
         (active-session (gnosis-test-review--session))
         (next-session (gnosis-test-review--session))
         (next-attempt (gnosis-test-review--attempt "pending")))
    (setf (plist-get next-session :current_position) 1
          (plist-get next-attempt :attempt_id) "attempt-2")
    (unwind-protect
        (with-current-buffer buffer
          (gnosis-review-mode)
          (setq gnosis-review--session active-session
                gnosis-review--attempt
                (gnosis-test-review--attempt "pending")
                gnosis-review--authority
                (gnosis-core--authenticated-authority token))
          (cl-letf (((symbol-function 'gnosis-edit-thema)
                     (lambda (thema-id after-save)
                       (setq opened-id thema-id
                             callback after-save)))
                    ((symbol-function 'pop-to-buffer-same-window)
                     (lambda (target &rest _)
                       (setq returned-buffer target)))
                    ((symbol-function 'gnosis-review--refresh)
                     (lambda () (ert-fail "must re-enter through begin")))
                    ((symbol-function 'gnosis-review--active-session)
                     (lambda () active-session))
                    ((symbol-function 'gnosis-core-review-attempt-begin)
                     (lambda (session-id)
                       (setq begun-session session-id)
                       (gnosis-test-review--begin-result
                        next-attempt next-session)))
                    ((symbol-function 'gnosis-review--present)
                     (lambda (&rest _)
                       (setq presented-buffer (current-buffer)))))
            (should (fboundp 'gnosis-review--edit))
            (funcall 'gnosis-review--edit)
            (should (equal "thema" opened-id))
            (should (functionp callback))
            (funcall callback '(:thema_id "thema" :revision 2))
            (should (eq buffer returned-buffer))
            (should (equal "session" begun-session))
            (should (eq buffer presented-buffer))
            (should (equal "attempt-2"
                           (plist-get gnosis-review--attempt :attempt_id)))
            (should (= 1 (plist-get gnosis-review--session
                                    :current_position)))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-review-edit-ignores-callback-for-replacement-state ()
  (let* ((buffer (generate-new-buffer " *gnosis-review-stale-edit-test*"))
         (token (make-string 40 ?a))
         (token-b (make-string 40 ?b))
         (gnosis-core--base-url-override "https://test.example")
         (gnosis-core--token-override token)
         (authority-b
          (let ((gnosis-core--base-url-override "https://other.example"))
            (gnosis-core--authenticated-authority token-b)))
         (session-a (gnosis-test-review--session))
         (attempt-a (gnosis-test-review--attempt "pending"))
         (session-b (gnosis-test-review--session))
         (attempt-a-in-b (gnosis-test-review--attempt "pending"))
         (attempt-b-in-a (gnosis-test-review--attempt "pending"))
         (attempt-b (gnosis-test-review--attempt "pending"))
         callback
         (base-reads 0)
         (token-reads 0)
         (pop-count 0)
         (next-count 0))
    (setf (plist-get session-a :session_id) "session-a"
          (plist-get attempt-a :session_id) "session-a"
          (plist-get attempt-a :attempt_id) "attempt-a"
          (plist-get session-b :session_id) "session-b"
          (plist-get attempt-a-in-b :session_id) "session-b"
          (plist-get attempt-a-in-b :attempt_id) "attempt-a"
          (plist-get attempt-b-in-a :session_id) "session-a"
          (plist-get attempt-b-in-a :attempt_id) "attempt-b"
          (plist-get attempt-b :session_id) "session-b"
          (plist-get attempt-b :attempt_id) "attempt-b")
    (unwind-protect
        (with-current-buffer buffer
          (gnosis-review-mode)
          (setq gnosis-review--session session-a
                gnosis-review--attempt attempt-a
                gnosis-review--authority
                (gnosis-core--authenticated-authority token))
          (cl-letf (((symbol-function 'gnosis-core--validated-base-url)
                     (lambda ()
                       (setq base-reads (1+ base-reads))
                       gnosis-core--base-url-override))
                    ((symbol-function 'gnosis-core--token)
                     (lambda ()
                       (setq token-reads (1+ token-reads))
                       gnosis-core--token-override))
                    ((symbol-function 'gnosis-edit-thema)
                     (lambda (_thema-id after-save)
                       (setq callback after-save)))
                    ((symbol-function 'pop-to-buffer-same-window)
                     (lambda (&rest _) (setq pop-count (1+ pop-count))))
                    ((symbol-function 'gnosis-review--next-current)
                     (lambda () (setq next-count (1+ next-count)))))
            (gnosis-review--edit)
            (should (functionp callback))
            (setq base-reads 0
                  token-reads 0)
            (dolist (replacement
                     (list (list session-b attempt-a-in-b)
                           (list session-a attempt-b-in-a)
                           (list session-b attempt-b)))
              (setq gnosis-review--session (car replacement)
                    gnosis-review--attempt (cadr replacement))
              (funcall callback '(:thema_id "thema" :revision 2))
              (should (= 0 base-reads))
              (should (= 0 token-reads))
              (should (= 0 pop-count))
              (should (= 0 next-count)))
            (setq gnosis-review--session session-a
                  gnosis-review--attempt attempt-a
                  gnosis-review--authority authority-b
                  gnosis-core--base-url-override "https://other.example"
                  gnosis-core--token-override token-b)
            (funcall callback '(:thema_id "thema" :revision 2))
            (should (= 0 base-reads))
            (should (= 0 token-reads))
            (should (= 0 pop-count))
            (should (= 0 next-count))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-review-reads-server-directed-structured-values ()
  (let ((gnosis-review--attempt
         (gnosis-test-review--attempt
          "pending" '(:kind "text" :input_script "greek")))
        arguments)
    (cl-letf (((symbol-function 'gnosis-review--read-string)
               (lambda (&rest args)
                 (setq arguments args)
                 "logos")))
      (should (equal '(:kind "text" :value "logos")
                     (gnosis-review--read-answer)))
      (should (equal '("Answer (Greek): " "greek") arguments))))
  (let ((gnosis-review--attempt
          (gnosis-test-review--attempt
          "pending" (list :kind "texts"
                          :input_scripts
                          (vector gnosis-core--json-null
                                  gnosis-core--json-null))))
        (answers '("beta" "alpha"))
        checks
        prompts
        scripts
        questions)
    (cl-letf (((symbol-function 'gnosis-review--read-string)
               (lambda (prompt script)
                 (push prompt prompts)
                 (push script scripts)
                 (pop answers)))
              ((symbol-function 'gnosis-core-review-attempt-check-cloze)
               (lambda (_attempt-id inputs)
                 (push inputs checks)
                 (if (= (length inputs) 1)
                     '(:matches [t] :all_entered_correct t :complete nil
                       :question "(...) and beta"
                       :cloze_spans
                       [(:start_byte 0 :end_byte 5 :state "concealed")
                        (:start_byte 10 :end_byte 14 :state "correct")]
                       :next_input_script "greek")
                   `(:matches [t t] :all_entered_correct t :complete t
                     :question "alpha and beta"
                     :cloze_spans
                     [(:start_byte 0 :end_byte 5 :state "correct")
                      (:start_byte 10 :end_byte 14 :state "correct")]
                     :next_input_script ,gnosis-core--json-null))))
              ((symbol-function 'gnosis-review--render)
               (lambda (&rest _)
                 (push (plist-get
                        (plist-get gnosis-review--attempt :presentation)
                        :question)
                       questions))))
      (should
       (equal '(:kind "texts" :value ["beta" "alpha"])
              (gnosis-review--read-answer)))
      (should (equal '(["beta"] ["beta" "alpha"]) (nreverse checks)))
      (should (equal '("Answer 1: " "Answer 2 (Greek): ")
                     (nreverse prompts)))
      (should (equal (list gnosis-core--json-null "greek")
                     (nreverse scripts)))
      (should (equal '("(...) and beta" "alpha and beta")
                     (nreverse questions)))))
  (let ((gnosis-review--attempt
         (gnosis-test-review--attempt
          "pending" (list :kind "choice" :choices ["A" "B"])))
        arguments)
    (cl-letf (((symbol-function 'gnosis-completing-read)
               (lambda (&rest args) (setq arguments args) "B")))
      (should (equal '(:kind "choice" :value "B")
                     (gnosis-review--read-answer)))
      (should (listp (nth 1 arguments)))
      (should (eq t (nth 2 arguments))))))

(ert-deftest gnosis-review-stops-progressive-cloze-input-on-first-failure ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt
          (gnosis-test-review--attempt
           "pending" (list :kind "texts"
                           :input_scripts
                           (vector gnosis-core--json-null
                                   gnosis-core--json-null))))
    (let ((reads 0))
      (cl-letf (((symbol-function 'gnosis-review--read-string)
                 (lambda (&rest _) (cl-incf reads) "wrong"))
                ((symbol-function 'gnosis-core-review-attempt-check-cloze)
                 (lambda (&rest _)
                   `(:matches [,gnosis-core--json-false]
                     :all_entered_correct ,gnosis-core--json-false
                     :complete ,gnosis-core--json-false
                     :question "(...) and (...)"
                     :cloze_spans
                     [(:start_byte 0 :end_byte 5 :state "concealed")
                      (:start_byte 10 :end_byte 15 :state "concealed")]
                     :next_input_script ,gnosis-core--json-null)))
                ((symbol-function 'gnosis-review--render) #'ignore))
        (should (equal '(:kind "texts" :value ["wrong"])
                       (gnosis-review--read-answer)))
        (should (= reads 1))))))

(ert-deftest gnosis-review-cancelled-progressive-cloze-restores-presentation ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt
          (gnosis-test-review--attempt
           "pending" (list :kind "texts"
                           :input_scripts
                           (vector gnosis-core--json-null
                                   gnosis-core--json-null)))
          gnosis-review--answer-locked-p nil)
    (let* ((presentation (plist-get gnosis-review--attempt :presentation))
           (original-question "(...) and (...)")
           (original-spans
            [(:start_byte 0 :end_byte 5 :state "concealed")
             (:start_byte 10 :end_byte 15 :state "concealed")])
           (reads 0)
           answer-sent)
      (setf (plist-get presentation :question) original-question
            (plist-get presentation :cloze_spans) original-spans)
      (cl-letf (((symbol-function 'gnosis-review--read-string)
                 (lambda (&rest _)
                   (cl-incf reads)
                   (if (= reads 1) "alpha" (signal 'quit nil))))
                ((symbol-function 'gnosis-core-review-attempt-check-cloze)
                 (lambda (&rest _)
                   `(:matches [t] :all_entered_correct t :complete nil
                     :question "alpha and (...)"
                     :cloze_spans
                     [(:start_byte 0 :end_byte 5 :state "correct")
                      (:start_byte 10 :end_byte 15 :state "concealed")]
                     :next_input_script ,gnosis-core--json-null)))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _) (setq answer-sent t)))
                ((symbol-function 'gnosis-review--render) #'ignore))
        (should
         (eq 'quit
             (condition-case nil
                 (progn (gnosis-review--answer-current) 'returned)
               (quit 'quit)))))
      (should (= reads 2))
      (should-not answer-sent)
      (should-not gnosis-review--answer-locked-p)
      (should (equal "pending" (plist-get gnosis-review--attempt :status)))
      (should (equal original-question
                     (plist-get
                      (plist-get gnosis-review--attempt :presentation)
                      :question)))
      (should (equal original-spans
                     (plist-get
                      (plist-get gnosis-review--attempt :presentation)
                      :cloze_spans))))))

(ert-deftest gnosis-review-render-never-grades-or-discloses-pending-answer ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (setf (plist-get gnosis-review--attempt :revealed_answer) ["SECRET"]
          (plist-get gnosis-review--attempt :revealed_parathema) "PRIVATE-HINT"
          (plist-get gnosis-review--attempt :response)
          '(:kind "text" :value "PRIVATE-RESPONSE")
          (plist-get gnosis-review--attempt :detected_success) t
          (plist-get gnosis-review--attempt :accepted_success) t
          (plist-get gnosis-review--attempt :desired_suspended) t)
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil))
              ((symbol-function 'gnosis-core-review-attempt-answer)
               (lambda (&rest _) (ert-fail "render answered")))
              ((symbol-function 'gnosis-core-review-attempt-commit)
               (lambda (&rest _) (ert-fail "render committed"))))
      (gnosis-review--render)
      (should-not (string-match-p "RET Answer\\|Actions" (buffer-string)))
      (should-not
       (string-match-p
        "SECRET\\|PRIVATE-HINT\\|PRIVATE-RESPONSE\\|success\\|suspend\\|session\\|attempt\\|thema"
        (buffer-string))))))

(ert-deftest gnosis-review-render-faces-unlabelled-hint-between-sections ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-center-content nil)
    (setf (plist-get (plist-get gnosis-review--attempt :presentation) :hint)
          "Remember this")
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (should-not (string-match-p "Hint:" (buffer-string)))
      (goto-char (point-min))
      (search-forward "Remember this")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-hint))
      (should
       (= 1
          (cl-loop for position from (point-min) below (point-max)
                   count (eq (get-text-property position 'face)
                             'gnosis-face-separator)))))))

(ert-deftest gnosis-review-render-faces-exact-cloze-spans ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-center-content nil)
    (setf (plist-get
           (plist-get gnosis-review--attempt :presentation) :question)
          "μυστικό and (hint) plus (literal)"
          (plist-get
           (plist-get gnosis-review--attempt :presentation) :cloze_spans)
          [(:start_byte 0 :end_byte 14 :state "correct")
           (:start_byte 19 :end_byte 25 :state "concealed")])
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "μυστικό")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-correct))
      (search-forward "(hint)")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-cloze))
      (search-forward "(literal)")
      (should-not (eq (get-text-property (1- (point)) 'face)
                      'gnosis-face-cloze)))))

(ert-deftest gnosis-review-rejects-mid-codepoint-cloze-spans ()
  (dolist (spans '([(:start_byte 0 :end_byte 1 :state "concealed")]
                    [(:start_byte 1 :end_byte 2 :state "concealed")]))
    (should-error
     (gnosis-review--question-text
      (list :question "μx" :cloze_spans spans))
     :type 'gnosis-core-protocol-error)))

(ert-deftest gnosis-review-render-reveals-failed-cloze-inline ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-center-content nil)
    (let ((presentation (plist-get gnosis-review--attempt :presentation)))
      (setf (plist-get presentation :kind) "cloze"
            (plist-get presentation :question) "alpha beta gamma"
            (plist-get presentation :cloze_spans)
            [(:start_byte 0 :end_byte 5 :state "incorrect")
             (:start_byte 6 :end_byte 10 :state "correct")
             (:start_byte 11 :end_byte 16 :state "unanswered")]))
    (setf (plist-get gnosis-review--attempt :accepted_success)
          gnosis-core--json-false)
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (dolist (expected '(("alpha" . gnosis-face-false)
                          ("beta" . gnosis-face-correct)
                          ("gamma" . gnosis-face-unanswered)))
        (goto-char (point-min))
        (search-forward (car expected))
        (should (eq (get-text-property (1- (point)) 'face)
                    (cdr expected))))
      (should-not (string-match-p "\nAnswer:" (buffer-string)))
      (should (string-match-p "Your answer: response" (buffer-string))))))

(ert-deftest gnosis-review-render-preserves-cloze-faces-through-org-links ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-center-content nil)
    (let ((presentation (plist-get gnosis-review--attempt :presentation)))
      (setf (plist-get presentation :kind) "cloze"
            (plist-get presentation :question)
            "[[id:one][alpha]] and (...)"
            (plist-get presentation :cloze_spans)
            [(:start_byte 10 :end_byte 15 :state "correct")
             (:start_byte 22 :end_byte 27 :state "concealed")]))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "alpha")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-correct))
      (search-forward "(...)")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-cloze)))))

(ert-deftest gnosis-review-render-keeps-whole-link-face-inside-span ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-center-content nil)
    (let ((presentation (plist-get gnosis-review--attempt :presentation)))
      (setf (plist-get presentation :kind) "cloze"
            (plist-get presentation :question) "[[id:one][alpha]] tail"
            (plist-get presentation :cloze_spans)
            [(:start_byte 0 :end_byte 17 :state "correct")]))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "alpha")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-correct))
      (search-forward " ")
      (should-not (eq (get-text-property (1- (point)) 'face)
                      'gnosis-face-correct)))))

(ert-deftest gnosis-review-render-separates-image-from-question ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-center-content nil)
    (setf (plist-get
           (plist-get gnosis-review--attempt :presentation) :review_image)
          "assets/example.png")
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (should
       (string-match-p
        (concat "\\`\\[Review image available\\]\n\n"
                (regexp-quote (gnosis--separator))
                "\n\n" (regexp-quote "Question?") "\\'")
        (buffer-string)))
      (should
       (= 1
          (cl-loop for position from (point-min) below (point-max)
                   count (eq (get-text-property position 'face)
                             'gnosis-face-separator)))))))

(ert-deftest gnosis-review-render-gives-graded-content-semantic-faces ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (dolist (expected '(("Answer:" . gnosis-face-directions)))
        (goto-char (point-min))
        (search-forward (car expected))
        (should (eq (get-text-property (1- (point)) 'face) (cdr expected))))
      (goto-char (point-min))
      (search-forward "Answer: Answer")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-correct))
      (should-not (string-match-p "•" (buffer-string)))
      (should-not (string-match-p "Your answer:" (buffer-string))))))

(ert-deftest gnosis-review-render-centers-every-visible-line ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (cl-letf (((symbol-function 'window-body-width) (lambda (&rest _) 80))
              ((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (dolist (line (split-string (buffer-string) "\n"))
        (unless (string-blank-p line)
          (let* ((text (string-trim-left line))
                 (padding (- (length line) (length text))))
            (should (= padding (/ (- 80 (string-width text)) 2)))))))))

(ert-deftest gnosis-review-render-wraps-long-lines-before-centering ()
  (with-temp-buffer
    (gnosis-review-mode)
    (let ((fill-column 20))
      (cl-letf (((symbol-function 'window-body-width) (lambda (&rest _) 40)))
        (let ((lines (split-string
                      (gnosis--center-text
                       "alpha beta gamma delta epsilon")
                      "\n")))
          (should (= 2 (length lines)))
          (dolist (line lines)
            (let* ((text (string-trim-left line))
                   (padding (- (length line) (length text))))
              (should (<= (string-width text) fill-column))
              (should (= padding (/ (- 40 (string-width text)) 2))))))))))

(ert-deftest gnosis-review-render-shows-centered-user-answer-on-failure ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (setf (plist-get gnosis-review--attempt :detected_success)
          gnosis-core--json-false
          (plist-get gnosis-review--attempt :accepted_success)
          gnosis-core--json-false)
    (cl-letf (((symbol-function 'window-body-width) (lambda (&rest _) 80))
              ((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "Your answer: response")
      (back-to-indentation)
      (should (= 29 (current-column)))
      (search-forward "response")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-false)))))

(ert-deftest gnosis-review-render-omits-empty-parathema-section ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (setf (plist-get gnosis-review--attempt :revealed_parathema) "")
    (cl-letf (((symbol-function 'window-body-width) (lambda (&rest _) 80))
              ((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (dolist (line (split-string (buffer-string) "\n"))
        (should-not (and (string-blank-p line) (> (length line) 1)))))))

(ert-deftest gnosis-review-render-gives-parathema-doc-face ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-center-content nil
          gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (setf (plist-get gnosis-review--attempt :revealed_parathema) "Context")
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "Context")
      (should (eq (get-text-property (1- (point)) 'face)
                  'font-lock-doc-face)))))

(ert-deftest gnosis-review-render-omits-operational-result-state ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "graded"))
    (setf (plist-get gnosis-review--attempt :accepted_success)
          gnosis-core--json-false)
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (dolist (label '("Detected:" "Accepted:" "Suspension:"))
        (should-not (string-match-p label (buffer-string)))))))

(ert-deftest gnosis-review-answer-submits-one-structured-mutation ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
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
                ((symbol-function 'display-images-p) (lambda () nil)))
        (gnosis-review--answer-authorized)
        (should (equal '(:kind "text" :value "response") submitted))
        (should (= 2 gnosis-review--attempt-revision))))))

(ert-deftest gnosis-review-answer-success-locks-retry-before-state-get ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
    (let ((posts 0) unavailable)
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "response")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf posts)
                   (gnosis-test-review--attempt "graded")))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (&rest _)
                   (if unavailable
                       (signal 'gnosis-core-unavailable '("state unavailable"))
                     (list :attempt (gnosis-test-review--attempt "graded")
                           :attempt_revision 2))))
                ((symbol-function 'display-images-p) (lambda () nil)))
        (setq unavailable t)
        (should-error (gnosis-review--answer-authorized) :type 'gnosis-core-unavailable)
        (should (= posts 1))
        (should (equal "pending" (plist-get gnosis-review--attempt :status)))
        (should (= 1 gnosis-review--attempt-revision))
        (should-not (keymap-lookup gnosis-review-mode-map "RET"))
        (should-error (gnosis-review--answer-authorized) :type 'user-error)
        (should (= posts 1))
        (setq unavailable nil)
        (gnosis-review--refresh-current)
        (should (= 2 gnosis-review--attempt-revision))
        (should (= posts 1))))))

(ert-deftest gnosis-review-refresh-unlocks-authoritatively-pending-answer ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
    (let ((posts 0) (reads 0) (renders 0) unavailable)
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "response")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf posts)
                   (signal 'gnosis-core-mutation-uncertain '("unknown"))))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (&rest _)
                   (cl-incf reads)
                   (if unavailable
                       (signal 'gnosis-core-unavailable '("state unavailable"))
                     (list :attempt (gnosis-test-review--attempt "pending")
                           :attempt_revision 2))))
                ((symbol-function 'gnosis-review--render)
                 (lambda (&rest _) (cl-incf renders)))
                ((symbol-function 'keymap-popup) #'ignore))
        (setq unavailable t)
        (should-error (gnosis-review--answer-authorized) :type 'gnosis-core-unavailable)
        (should gnosis-review--answer-locked-p)
        (setq unavailable nil)
        (gnosis-review--refresh-current)
        (should (= posts 1))
        (should (= reads 2))
        (should (= renders 1))
        (should (= gnosis-review--attempt-revision 2))
        (should (gnosis-review--answerable-p))))))

(ert-deftest gnosis-review-answer-definite-rejection-preserves-state ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
    (let ((attempt gnosis-review--attempt))
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "response")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (signal 'gnosis-core-validation-error '("rejected")))))
        (should-error (gnosis-review--answer-authorized)
                      :type 'gnosis-core-validation-error)
        (should (eq attempt gnosis-review--attempt))
        (should (= 1 gnosis-review--attempt-revision))
        (should-not gnosis-review--answer-locked-p)))))

(ert-deftest gnosis-review-answer-quit-after-dispatch-keeps-retry-locked ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt (gnosis-test-review--attempt "pending"))
    (let ((posts 0))
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "x")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf posts)
                   (signal 'quit nil))))
        (condition-case nil
            (gnosis-review--answer-authorized)
          (quit nil))
        (should (= posts 1))
        (should gnosis-review--answer-locked-p)
        (should-not (keymap-lookup gnosis-review-mode-map "RET"))
        (should-error (gnosis-review--answer-authorized) :type 'user-error)
        (should (= posts 1))))))

(ert-deftest gnosis-review-locked-pending-popup-offers-refresh-only ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--answer-locked-p t)
    (gnosis-review--render)
    (should (string-suffix-p "refresh" (gnosis-review--popup-description)))
    (should-not (string-match-p "Refresh\\|RET Answer" (buffer-string)))))

(ert-deftest gnosis-review-answer-uncertainty-recovers-with-get-not-retry ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
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
        (gnosis-review--answer-authorized)
        (should (= answers 1))
        (should (= reads 1))
        (should (equal "graded" (plist-get gnosis-review--attempt :status)))))))

(ert-deftest gnosis-review-answer-uncertainty-pending-unlocks-after-get ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 0)
    (let ((posts 0) (reads 0) (renders 0))
      (cl-letf (((symbol-function 'gnosis-review--read-answer)
                 (lambda () '(:kind "text" :value "пилет")))
                ((symbol-function 'gnosis-core-review-attempt-answer)
                 (lambda (&rest _)
                   (cl-incf posts)
                   (signal 'gnosis-core-mutation-uncertain '("unknown"))))
                ((symbol-function 'gnosis-core-review-attempt-state)
                 (lambda (_id)
                   (cl-incf reads)
                   (list :attempt_revision 0
                         :attempt (gnosis-test-review--attempt "pending"))))
                ((symbol-function 'gnosis-review--render)
                 (lambda (&rest _) (cl-incf renders))))
        (should-error (gnosis-review--answer-authorized)
                      :type 'gnosis-core-mutation-uncertain)
        (should (= posts 1))
        (should (= reads 1))
        (should (= renders 1))
        (should-not gnosis-review--answer-locked-p)
        (should (gnosis-review--answerable-p))))))

(ert-deftest gnosis-review-answer-uncertainty-rejects-unrelated-response ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt (gnosis-test-review--attempt "pending")
          gnosis-review--attempt-revision 1)
    (cl-letf (((symbol-function 'gnosis-review--read-answer)
               (lambda () '(:kind "text" :value "mine")))
              ((symbol-function 'gnosis-core-review-attempt-answer)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-core-review-attempt-state)
               (lambda (_id)
                 (let ((attempt (gnosis-test-review--attempt "graded")))
                   (setf (plist-get attempt :response)
                         '(:kind "text" :value "other"))
                   (list :attempt_revision 2 :attempt attempt)))))
      (should-error (gnosis-review--answer-authorized)
                    :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-review-uncertain-no-op-requires-revision-advance ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 3)
    (cl-letf (((symbol-function 'gnosis-review--load-revision)
               (lambda ()
                 (setf (plist-get gnosis-review--attempt :accepted_success) t)
                 (list :attempt_revision 3 :attempt gnosis-review--attempt)))
              ((symbol-function 'gnosis-review--render) #'ignore))
      (should-error
       (gnosis-review--revisioned-operation
        (lambda () (signal 'gnosis-core-mutation-uncertain '("unknown")))
        :accepted_success t)
       :type 'gnosis-core-mutation-uncertain))))

(ert-deftest gnosis-review-public-target-validation-precedes-authority ()
  (let (events)
    (cl-letf (((symbol-function 'gnosis-review--call-with-current-authority)
               (lambda (_) (push 'authority events)))
              ((symbol-function 'gnosis-review--start)
               (lambda (&rest _) (push 'start events))))
      (dolist (invalid '(-1 501 1.5 "1"))
        (should-error (gnosis-review invalid) :type 'user-error)
        (should-error (gnosis-review-topic "topic" invalid) :type 'user-error)
        (should-not events)))))

(ert-deftest gnosis-review-topic-id-validation-precedes-authority ()
  (let (events)
    (cl-letf (((symbol-function 'gnosis-review--call-with-current-authority)
               (lambda (_) (push 'authority events)))
              ((symbol-function 'gnosis-review--start)
               (lambda (&rest _) (push 'start events))))
      (dolist (invalid '(nil 1 ""))
        (should-error (gnosis-review-topic invalid) :type 'user-error)
        (should-not events)))))

(ert-deftest gnosis-review-public-target-boundaries-preserve-defaults ()
  (let (started)
    (cl-letf (((symbol-function 'gnosis-review--call-with-current-authority)
               (lambda (function) (funcall function)))
              ((symbol-function 'gnosis-review--start)
               (lambda (source target-count)
                 (setq started (list source target-count)))))
      (dolist (case '((nil nil) (0 nil) (1 1) (500 500)))
        (let ((input (car case))
              (expected (cadr case)))
          (setq started nil)
          (gnosis-review input)
          (should (equal "due" (plist-get (car started) :kind)))
          (should (equal expected (cadr started)))
          (setq started nil)
          (gnosis-review-topic "topic" input)
          (should (equal "topic" (plist-get (car started) :kind)))
          (should (equal "topic" (plist-get (car started) :topic_node_id)))
          (should (equal expected (cadr started))))))))

(ert-deftest gnosis-review-target-count-validation-precedes-session-effects ()
  (let ((source (gnosis-review--source "due"))
        events)
    (should-not (gnosis-review--normalize-target-count nil))
    (should-not (gnosis-review--normalize-target-count 0))
    (should (= 1 (gnosis-review--normalize-target-count 1)))
    (should (= 500 (gnosis-review--normalize-target-count 500)))
    (cl-letf (((symbol-function 'gnosis-core-status)
               (lambda () (push 'status events)))
              ((symbol-function 'gnosis-review--active-session)
               (lambda () (push 'active events)
                 (gnosis-test-review--session)))
              ((symbol-function 'gnosis-review--read-active-session-action)
               (lambda () (push 'prompt events) "Start new"))
              ((symbol-function 'gnosis-review--quit-session)
               (lambda (_) (push 'quit events)))
              ((symbol-function 'gnosis-review--create)
               (lambda (&rest _) (push 'create events))))
      (dolist (invalid '(-1 501 1.5 "1"))
        (should-error (gnosis-review--start source invalid) :type 'user-error)
        (should-not events)))))

(ert-deftest gnosis-review-create-uncertainty-rejects-unrelated-session ()
  (let ((source (gnosis-review--source "due"))
        opened)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-core-review-session-create)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-review--active-session)
               (lambda ()
                 (append (gnosis-test-review--session)
                         (list :source (gnosis-review--source "topic")
                               :target_count 100))))
              ((symbol-function 'gnosis-review--open)
               (lambda (_) (setq opened t))))
      (should-error (gnosis-review--create source 20)
                    :type 'gnosis-core-mutation-uncertain)
      (should-not opened))))

(ert-deftest gnosis-review-create-uncertainty-accepts-effective-default ()
  (let ((source (gnosis-review--source "due"))
        opened)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-core-review-session-create)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-review--active-session)
               (lambda ()
                 (append (gnosis-test-review--session)
                         (list :source source :target_count 100))))
              ((symbol-function 'gnosis-review--open)
               (lambda (_) (setq opened t))))
      (should (gnosis-review--create source nil))
      (should opened))))

(ert-deftest gnosis-review-create-uncertainty-rejects-wrong-default ()
  (let ((source (gnosis-review--source "due"))
        opened)
    (cl-letf (((symbol-function 'gnosis-core-status) #'ignore)
              ((symbol-function 'gnosis-core-review-session-create)
               (lambda (&rest _)
                 (signal 'gnosis-core-mutation-uncertain '("unknown"))))
              ((symbol-function 'gnosis-review--active-session)
               (lambda ()
                 (append (gnosis-test-review--session)
                         (list :source source :target_count 1))))
              ((symbol-function 'gnosis-review--open)
               (lambda (_) (setq opened t))))
      (should-error (gnosis-review--create source nil)
                    :type 'gnosis-core-mutation-uncertain)
      (should-not opened))))

(ert-deftest gnosis-review-topic-supplies-valid-zero-depths ()
  (let ((gnosis-core--base-url-override "https://test.example")
        (gnosis-core--token-override (make-string 40 ?a))
        source)
    (cl-letf (((symbol-function 'gnosis-review--start)
               (lambda (value _target-count) (setq source value))))
      (gnosis-review-topic "topic")
      (should (equal "topic" (plist-get source :topic_node_id)))
      (should (equal 0 (plist-get source :topic_forward_depth)))
      (should (equal 0 (plist-get source :topic_backward_depth))))))

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
                ((symbol-function 'gnosis-review--next-current)
                 (lambda () (push 'next events))))
        (gnosis-review--finish-authorized gnosis-core--json-false)
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
        (should-error (gnosis-review--toggle-suspension-current) :type 'user-error)
        (should (equal '(render refresh) events))))))

(ert-deftest gnosis-review-finish-does-not-commit-mismatched-recovery ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt (gnosis-test-review--attempt "graded")
          gnosis-review--attempt-revision 3)
    (let ((commits 0))
      (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (&rest _)
                   (signal 'gnosis-core-mutation-uncertain '("mismatch"))))
                ((symbol-function 'gnosis-review--load-revision)
                 (lambda ()
                   (setq gnosis-review--attempt-revision 4)
                   (setf (plist-get gnosis-review--attempt :accepted_success) t)
                   (list :attempt_revision 4 :attempt gnosis-review--attempt)))
                ((symbol-function 'gnosis-review--render) #'ignore)
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (&rest _) (cl-incf commits))))
        (should-error (gnosis-review--finish-authorized gnosis-core--json-false)
                      :type 'gnosis-core-mutation-uncertain)
        (should (= commits 0))))))

(ert-deftest gnosis-review-override-updates-result-without-advancing ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--attempt
          (plist-put (gnosis-test-review--attempt "graded")
                     :accepted_success gnosis-core--json-false)
          gnosis-review--attempt-revision 4)
    (let (events)
      (cl-letf (((symbol-function 'gnosis-core-review-attempt-set-accepted-success)
                 (lambda (_ _ value)
                   (push (list 'set value) events)
                   (list :attempt_revision 5
                         :attempt (plist-put gnosis-review--attempt
                                             :accepted_success value))))
                ((symbol-function 'gnosis-core-review-attempt-commit)
                 (lambda (&rest _) (push 'commit events)))
                ((symbol-function 'gnosis-review--next-current)
                 (lambda () (push 'next events)))
                ((symbol-function 'gnosis-review--render)
                 (lambda () (push 'render events)))
                ((symbol-function 'message) #'ignore))
        (gnosis-review--override-current)
        (should (equal '((set t) render) (nreverse events)))
        (should (= 5 gnosis-review--attempt-revision))
        (should (eq t (plist-get gnosis-review--attempt
                                 :accepted_success)))))))

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
                ((symbol-function 'gnosis-review--next-current)
                 (lambda () (push 'next events))))
        (gnosis-review--finish-authorized t)
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
      (gnosis-review--next-current)
      (should-not resumed))))

(ert-deftest gnosis-review-close-is-local-and-leaves-session-active ()
  (let ((buffer (generate-new-buffer " *gnosis-review-close*"))
        mutated)
    (unwind-protect
        (with-current-buffer buffer
          (setq gnosis-review--session (gnosis-test-review--session "active"))
          (cl-letf (((symbol-function 'gnosis-core-review-session-quit)
                     (lambda (&rest _) (setq mutated t))))
            (gnosis-review--quit)
            (should-not mutated)
            (should-not (buffer-live-p buffer))))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(ert-deftest gnosis-review-resume-cancel-does-not-mutate ()
  (let ((gnosis-core--base-url-override "https://test.example")
        (gnosis-core--token-override (make-string 40 ?a))
        mutated)
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

(defun gnosis-test-review--with-next-review (attempt day)
  "Return ATTEMPT with server next-review DAY."
  (let ((copy (copy-sequence attempt)))
    (setf (plist-get copy :proposed_transition)
          (list :after (list :next_review_day day)))
    copy))

(ert-deftest gnosis-review-render-omits-next-review-while-pending ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt
          (gnosis-test-review--with-next-review
           (gnosis-test-review--attempt "pending") 20260820))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (should-not (string-match-p "Next review:" (buffer-string))))))

(ert-deftest gnosis-review-render-shows-archive-next-review-after-grade ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt
          (gnosis-test-review--with-next-review
           (gnosis-test-review--attempt "graded") 20260820))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (goto-char (point-min))
      (search-forward "Next review:")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-directions))
      (search-forward "2026/8/20")
      (should (eq (get-text-property (1- (point)) 'face)
                  'gnosis-face-next-review)))))

(ert-deftest gnosis-review-render-updates-next-review-after-override ()
  (with-temp-buffer
    (gnosis-review-mode)
    (setq gnosis-review--session (gnosis-test-review--session)
          gnosis-review--attempt
          (gnosis-test-review--with-next-review
           (gnosis-test-review--attempt "graded") 20260820))
    (cl-letf (((symbol-function 'display-images-p) (lambda () nil)))
      (gnosis-review--render)
      (setq gnosis-review--attempt
            (gnosis-test-review--with-next-review
             (gnosis-test-review--attempt "graded") 20260903))
      (setf (plist-get gnosis-review--attempt :accepted_success)
            gnosis-core--json-false)
      (gnosis-review--render)
      (should (string-match-p "Next review: 2026/9/3" (buffer-string)))
      (should-not (string-match-p "2026/8/20" (buffer-string))))))

(provide 'gnosis-test-review)
;;; gnosis-test-review.el ends here
