;;; gnosis-review.el --- Online Gnosis review UI  -*- lexical-binding: t; -*-

;; Copyright (C) 2022-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Present server-owned review views and submit explicit operator responses.
;; Matching, grading, scheduling, persistence, and recovery remain in gnosisd.

;;; Code:

(require 'cl-lib)
(require 'gnosis)
(require 'gnosis-core)
(require 'gnosis-core-assets)
(require 'gnosis-completion)
(require 'subr-x)

(defvar gnosis-review-buffer-name "*gnosis*"
  "Buffer name reserved for Gnosis review.")

(defvar-local gnosis-review--session nil)
(defvar-local gnosis-review--attempt nil)
(defvar-local gnosis-review--attempt-revision nil)
(defvar-local gnosis-review--answer-locked-p nil)

(defcustom gnosis-review-input-method-alist
  '(("greek" . "greek")
    ("cyrillic" . "cyrillic-translit"))
  "Map server input-script names to Emacs input methods."
  :type '(alist :key-type string :value-type string)
  :group 'gnosis)

(defun gnosis-review--pending-p ()
  "Return non-nil when the current attempt awaits an answer."
  (equal "pending" (plist-get gnosis-review--attempt :status)))

(defun gnosis-review--answerable-p ()
  "Return non-nil when the current attempt may be answered safely."
  (and (gnosis-review--pending-p)
       (not gnosis-review--answer-locked-p)))

(defun gnosis-review--graded-p ()
  "Return non-nil when the current attempt can be mutated."
  (equal "graded" (plist-get gnosis-review--attempt :status)))

(defun gnosis-review--session-label ()
  "Return a compact progress label for the current session."
  (format "Review %d/%d"
          (1+ (plist-get gnosis-review--session :current_position))
          (plist-get gnosis-review--session :queue_length)))

(defun gnosis-review--popup-description ()
  "Return public-safe context text for review popup help."
  (format "%s — %s" (gnosis-review--session-label)
          (pcase (plist-get gnosis-review--attempt :status)
            ("pending" (if (gnosis-review--answerable-p)
                           "answer"
                         "refresh"))
            ("graded" "result")
            ("committed" "continue")
            (_ "refresh"))))

(defun gnosis-review--suspension-description ()
  "Return current server-backed suspension action label."
  (if (eq (plist-get gnosis-review--attempt :desired_suspended) t)
      "Unsuspend"
    "Suspend"))

(keymap-popup-define gnosis-review-mode-map
  "Keymap for `gnosis-review-mode'."
  :popup-key "?"
  :exit-key "C-g"
  :description #'gnosis-review--popup-description
  :group "Review"
  "RET" ("Answer" gnosis-review--answer :if #'gnosis-review--answerable-p)
  "n" ("Commit & next" gnosis-review--commit-next
       :inapt-if #'gnosis-review--pending-p)
  :group "State"
  "o" ("Override" gnosis-review--override
       :inapt-if #'gnosis-review--pending-p :stay-open t)
  "s" (#'gnosis-review--suspension-description gnosis-review--toggle-suspension
       :inapt-if #'gnosis-review--pending-p :stay-open t)
  :group "Session"
  "g" ("Refresh" gnosis-review--refresh)
  "q" ("Close review" gnosis-review--quit))

(define-derived-mode gnosis-review-mode special-mode "Gnosis-Review"
  "Major mode for server-authoritative online Gnosis review."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines nil
              gnosis-center-content gnosis-center-content-during-review
              header-line-format '(:eval (gnosis-review--header-line))))

(defun gnosis-review--source (kind)
  "Return a closed review source for KIND."
  (list :kind kind :tags [] :exclude_tags []
        :topic_node_id gnosis-core--json-null
        :topic_forward_depth gnosis-core--json-null
        :topic_backward_depth gnosis-core--json-null
        :manual_ids []))

(defun gnosis-review--header-line ()
  "Return semantic one-based review progress for the header line."
  (gnosis-center-string
   (concat "Review "
           (propertize
            (number-to-string
             (1+ (plist-get gnosis-review--session :current_position)))
            'face 'font-lock-type-face)
           (propertize "/" 'face 'shadow)
           (propertize
            (number-to-string (plist-get gnosis-review--session :queue_length))
            'face 'font-lock-type-face))))

(defun gnosis-review--session-complete-p (session)
  "Return non-nil when SESSION has no remaining review item."
  (equal (plist-get session :status) "completed"))

(defun gnosis-review--insert-image (reference)
  "Insert managed image REFERENCE when display support exists."
  (when (stringp reference)
    (if (not (display-images-p))
        (insert (gnosis--center-text
                 (propertize "[Review image available]"
                             'face 'gnosis-face-hint)))
      (condition-case nil
          (if-let* ((image (create-image
                            (gnosis-core-assets-resolve reference))))
              (progn
                (when gnosis-center-content
                  (insert
                   (propertize
                    " " 'display
                    `(space :align-to (- center (0.5 . ,image))))))
                (insert-image image))
            (insert (gnosis--center-text
                     (propertize "[Review image unavailable]"
                                 'face 'gnosis-face-hint))))
        (error
         (insert (gnosis--center-text
                  (propertize "[Review image unavailable]"
                              'face 'gnosis-face-hint))))))
    t))

(defun gnosis-review--response-text (response)
  "Return display text for structured RESPONSE."
  (let ((value (plist-get response :value)))
    (if (vectorp value)
        (string-join (append value nil) "\n")
      value)))

(defun gnosis-review--styled-text (text &optional face)
  "Return Org-fontified TEXT, optionally with semantic FACE."
  (let ((formatted (string-trim-right (gnosis-org-format-string text))))
    (if face (propertize formatted 'face face) formatted)))

(defun gnosis-review--content-text (text &optional face)
  "Return centered display TEXT, optionally with semantic FACE."
  (gnosis--center-text (gnosis-review--styled-text text face)))

(defun gnosis-review--labeled-text (label text face)
  "Return centered LABEL followed by display TEXT using FACE."
  (gnosis--center-text
   (concat (propertize label 'face 'gnosis-face-directions)
           " "
           (gnosis-review--styled-text text face))))

(defun gnosis-review--answer-text (answers)
  "Return display text for revealed ANSWERS."
  (let ((items (append answers nil)))
    (if (cdr items)
        (mapconcat (lambda (item) (concat "• " item)) items "\n")
      (or (car items) ""))))

(defun gnosis-review--insert-separator ()
  "Insert spacing and a separator between review sections."
  (insert "\n\n" (gnosis--separator) "\n\n"))

(defun gnosis-review--render (&optional start-at-top)
  "Render current authoritative view, starting at top when START-AT-TOP."
  (let* ((inhibit-read-only t)
         (old-point (point))
         (old-window-start
          (and (eq (window-buffer (selected-window)) (current-buffer))
               (window-start)))
         (presentation (plist-get gnosis-review--attempt :presentation))
         (pending (gnosis-review--pending-p)))
    (erase-buffer)
    (when (gnosis-review--insert-image
           (let ((value (plist-get presentation :review_image)))
             (unless (eq value gnosis-core--json-null) value)))
      (gnosis-review--insert-separator))
    (insert (gnosis-review--content-text (plist-get presentation :question)))
    (let ((hint (plist-get presentation :hint)))
      (unless (eq hint gnosis-core--json-null)
        (gnosis-review--insert-separator)
        (insert (gnosis-review--content-text hint 'gnosis-face-hint))))
    (unless pending
      (let* ((accepted (plist-get gnosis-review--attempt :accepted_success))
             (response (gnosis-review--response-text
                        (plist-get gnosis-review--attempt :response))))
        (gnosis-review--insert-separator)
        (insert
         (gnosis-review--labeled-text
          "Answer:"
          (gnosis-review--answer-text
           (plist-get gnosis-review--attempt :revealed_answer))
          'gnosis-face-correct))
        (unless (eq accepted t)
          (gnosis-review--insert-separator)
          (insert
           (gnosis-review--labeled-text
            "Your answer:" response 'gnosis-face-false)))
        (let ((parathema
               (plist-get gnosis-review--attempt :revealed_parathema)))
          (when (and (stringp parathema)
                     (not (string-blank-p parathema)))
            (gnosis-review--insert-separator)
            (insert
             (gnosis-review--content-text
              parathema 'gnosis-face-hint))))))
    (goto-char (if start-at-top (point-min) (min old-point (point-max))))
    (when old-window-start
      (set-window-start
       (selected-window)
       (if start-at-top (point-min) (min old-window-start (point-max))) t))))

(defun gnosis-review--present (&optional start-at-top)
  "Render the attempt and enter its default action.
When START-AT-TOP is non-nil, begin the presentation at the top."
  (gnosis-review--render start-at-top)
  (if (gnosis-review--answerable-p)
      (gnosis-review--answer t)
    (keymap-popup gnosis-review-mode-map)))

(defun gnosis-review--read-string (prompt script)
  "Read PROMPT with the input method requested by server SCRIPT."
  (let ((method (and (stringp script)
                     (alist-get script gnosis-review-input-method-alist
                               nil nil #'equal)))
        (previous current-input-method))
    (if (not method)
        (read-string prompt)
      (unwind-protect
          (progn
            (activate-input-method method)
            (read-string prompt nil nil nil t))
        (unless (equal current-input-method previous)
          (if previous
              (activate-input-method previous)
            (deactivate-input-method)))))))

(defun gnosis-review--read-answer ()
  "Read the structured answer requested by the current presentation."
  (let ((prompt (plist-get
                 (plist-get gnosis-review--attempt :presentation) :response)))
    (pcase (plist-get prompt :kind)
      ("text"
       (let ((script (plist-get prompt :input_script)))
         (list :kind "text" :value
               (gnosis-review--read-string
                (if (eq script gnosis-core--json-null)
                    "Answer: "
                  (format "Answer (%s): " script))
                script))))
      ("texts"
       (gnosis-review--read-cloze-answer prompt))
      ("choice"
       (list :kind "choice" :value
             (gnosis-completing-read
              "Answer: " (append (plist-get prompt :choices) nil) t)))
      (_ (user-error "Unsupported review response prompt")))))

(defun gnosis-review--read-cloze-answer (prompt)
  "Read and progressively check the cloze answer requested by PROMPT."
  (let ((attempt-id (plist-get gnosis-review--attempt :attempt_id))
        (script (aref (plist-get prompt :input_scripts) 0))
        (inputs [])
        continue)
    (setq continue t)
    (while continue
      (let* ((index (1+ (length inputs)))
             (input (gnosis-review--read-string
                     (if (eq script gnosis-core--json-null)
                         (format "Answer %d: " index)
                       (format "Answer %d (%s): " index script))
                     script))
             (next-inputs (vconcat inputs (vector input)))
             (check (gnosis-core-review-attempt-check-cloze
                     attempt-id next-inputs))
             (matches (plist-get check :matches)))
        (setq inputs next-inputs
              script (plist-get check :next_input_script)
              continue
              (and (eq t (aref matches (1- (length matches))))
                   (not (eq t (plist-get check :complete)))))
        (setf (plist-get
               (plist-get gnosis-review--attempt :presentation) :question)
              (plist-get check :question))
        (gnosis-review--render)))
    (list :kind "texts" :value inputs)))

(defun gnosis-review--load-revision ()
  "Refresh and return authoritative state for the current attempt."
  (let ((state (gnosis-core-review-attempt-state
                (plist-get gnosis-review--attempt :attempt_id))))
    (setq gnosis-review--attempt-revision (plist-get state :attempt_revision)
          gnosis-review--attempt (plist-get state :attempt))
    state))

(defun gnosis-review--revisioned-operation (operation key desired)
  "Call OPERATION and reconcile KEY with DESIRED after recoverable errors."
  (let ((expected-revision gnosis-review--attempt-revision))
    (condition-case condition
        (funcall operation)
      (gnosis-core-conflict
       (if (gnosis-review--stale-condition-p condition)
           (signal (car condition) (cdr condition))
         (gnosis-review--load-revision)
         (gnosis-review--render)
         (user-error "Review attempt changed on server; refreshed")))
      (gnosis-core-mutation-uncertain
       (let ((state (gnosis-review--load-revision)))
         (gnosis-review--render)
         (if (and (= gnosis-review--attempt-revision
                     (1+ expected-revision))
                  (equal desired (plist-get gnosis-review--attempt key)))
             state
           (signal (car condition) (cdr condition))))))))

(defun gnosis-review--active-session ()
  "Return the authoritative active session, or nil."
  (let ((session (plist-get (gnosis-core-review-session-active) :session)))
    (unless (eq session gnosis-core--json-null) session)))

(defun gnosis-review--resume-session (session-id)
  "Resume SESSION-ID, reconciling a delivery-unknown outcome with a GET."
  (condition-case condition
      (gnosis-core-review-session-resume session-id)
    (gnosis-core-mutation-uncertain
     (let ((session (gnosis-review--active-session)))
       (if (and session (equal session-id (plist-get session :session_id)))
           session
         (signal (car condition) (cdr condition)))))))

(defun gnosis-review--quit-session (session-id)
  "Quit SESSION-ID, reconciling a delivery-unknown outcome with a GET."
  (condition-case condition
      (gnosis-core-review-session-quit session-id)
    (gnosis-core-mutation-uncertain
     (if (null (gnosis-review--active-session))
         nil
       (signal (car condition) (cdr condition))))))

(defun gnosis-review--begin (session-id)
  "Return reconciled session and optional attempt for SESSION-ID."
  (let* ((result (gnosis-core-review-attempt-begin session-id))
         (attempt (plist-get result :attempt)))
    (list :session (plist-get result :session)
          :attempt (unless (eq attempt gnosis-core--json-null) attempt)
          :skipped (plist-get result :skipped))))

(defun gnosis-review--skipped-message (skipped complete-p)
  "Report SKIPPED review items, noting completion when COMPLETE-P."
  (let ((count (length skipped)))
    (when (> count 0)
      (message "%s; skipped %d unavailable review item%s"
               (if complete-p "Gnosis review complete" "Continuing review")
               count (if (= count 1) "" "s")))))

(defun gnosis-review--complete (&optional skipped)
  "Clear the current review, report SKIPPED items, and close its buffer."
  (setq gnosis-review--session nil
        gnosis-review--attempt nil
        gnosis-review--attempt-revision nil)
  (unless (gnosis-review--skipped-message skipped t)
    (message "Gnosis review complete"))
  (kill-buffer (current-buffer)))

(defun gnosis-review--next ()
  "Advance to the next server attempt or close a completed session."
  (let* ((session-id (plist-get gnosis-review--session :session_id))
         (session (gnosis-review--active-session)))
    (when (and session
               (not (equal session-id (plist-get session :session_id))))
      (gnosis-core--invalid "Gnosis active review session identity mismatch"))
    (if (null session)
        (gnosis-review--complete)
      (let* ((result (gnosis-review--begin session-id))
             (attempt (plist-get result :attempt)))
        (if (null attempt)
            (gnosis-review--complete (plist-get result :skipped))
          (gnosis-review--skipped-message (plist-get result :skipped) nil)
          (setq gnosis-review--session (plist-get result :session)
                gnosis-review--attempt attempt
                gnosis-review--attempt-revision nil
                gnosis-review--answer-locked-p nil)
          (gnosis-review--present t))))))

(defun gnosis-review--open (session)
  "Open a review buffer for active SESSION."
  (if (gnosis-review--session-complete-p session)
      (message "No Gnosis review items are available")
    (let* ((result (gnosis-review--begin (plist-get session :session_id)))
           (attempt (plist-get result :attempt)))
      (if (null attempt)
          (let ((skipped (plist-get result :skipped)))
            (if (> (length skipped) 0)
                (message "No reviewable items; skipped %d unavailable item%s"
                         (length skipped)
                         (if (= (length skipped) 1) "" "s"))
              (message "No Gnosis review items are available")))
        (gnosis-review--skipped-message (plist-get result :skipped) nil)
        (let ((buffer (get-buffer-create gnosis-review-buffer-name)))
          (pop-to-buffer-same-window buffer)
          (unless (derived-mode-p 'gnosis-review-mode)
            (gnosis-review-mode))
          (setq gnosis-review--session (plist-get result :session)
                gnosis-review--attempt attempt
                gnosis-review--attempt-revision nil
                gnosis-review--answer-locked-p nil)
          (gnosis-review--present t))))))

(defun gnosis-review--create (source target-count)
  "Create and open a review for SOURCE and optional TARGET-COUNT."
  (condition-case condition
      (gnosis-review--open
       (gnosis-core-review-session-create source target-count))
    (gnosis-core-not-found
     (message "No Gnosis review items are available"))
    (gnosis-core-mutation-uncertain
     (let ((session (gnosis-review--active-session))
           (expected-count target-count))
       (if (and session
                (equal source (plist-get session :source))
                (let ((effective (plist-get session :target_count)))
                  (equal effective
                         (or expected-count
                             gnosis-core--default-online-review-target-count))))
           (gnosis-review--open session)
         (signal (car condition) (cdr condition)))))))

(defun gnosis-review--read-active-session-action ()
  "Read the action to take when a review session is already active."
  (completing-read
   "Active session found: "
   '("Resume session" "Start new" "Cancel")
   nil t nil nil "Resume session"))

(defun gnosis-review--start (source target-count)
  "Start or resume a review for SOURCE and optional TARGET-COUNT."
  (gnosis-core-status)
  (if-let* ((session (gnosis-review--active-session)))
      (pcase (gnosis-review--read-active-session-action)
        ("Resume session"
         (gnosis-review--open
          (gnosis-review--resume-session
           (plist-get session :session_id))))
        ("Start new"
         (gnosis-review--quit-session (plist-get session :session_id))
         (gnosis-review--create source target-count))
        ("Cancel" (message "Gnosis review unchanged")))
    (gnosis-review--create source target-count)))

;;;###autoload
(defun gnosis-review (&optional target-count)
  "Start a server-owned due review of optional TARGET-COUNT.
When a review is active, prompt to resume it, replace it, or cancel."
  (interactive
   (list (let ((count (read-number "Target count (0 for server default): " 0)))
           (and (> count 0) count))))
  (gnosis-review--start (gnosis-review--source "due") target-count))

;;;###autoload
(defun gnosis-review-topic (topic-node-id &optional target-count)
  "Start a server-owned review for TOPIC-NODE-ID and TARGET-COUNT.
When a review is active, prompt to resume it, replace it, or cancel."
  (interactive
   (list (read-string "Topic node ID: ")
         (when current-prefix-arg (read-number "Target count: " 20))))
  (let ((source (gnosis-review--source "topic")))
    (setq source (plist-put source :topic_node_id topic-node-id))
    (setq source (plist-put source :topic_forward_depth 0))
    (setq source (plist-put source :topic_backward_depth 0))
    (gnosis-review--start source target-count)))

;;;###autoload
(defun gnosis-review-resume ()
  "Resume, abort, or leave unchanged the active server review."
  (interactive)
  (if-let* ((session (gnosis-review--active-session)))
      (pcase (read-char-choice
              "Active review: [r]esume [a]bort [c]ancel " '(?r ?a ?c))
        (?r (gnosis-review--open
             (gnosis-review--resume-session
              (plist-get session :session_id))))
        (?a (gnosis-core-review-session-abort
             (plist-get session :session_id))
            (message "Gnosis review aborted"))
        (?c (message "Gnosis review unchanged")))
    (user-error "No active Gnosis review session")))

(defun gnosis-review--stale-condition-p (condition)
  "Return non-nil when CONDITION reports a stale review attempt."
  (equal "stale_attempt" (nth 2 condition)))

(defun gnosis-review--answer-current (&optional show-popup)
  "Submit one response and show actions when SHOW-POPUP is non-nil."
  (unless (gnosis-review--answerable-p)
    (user-error "Attempt cannot be answered; refresh authoritative state"))
  (let* ((attempt-id (plist-get gnosis-review--attempt :attempt_id))
         (answer (gnosis-review--read-answer))
         answered-attempt
         uncertain
         state)
    (setq gnosis-review--answer-locked-p t)
    (condition-case condition
        (setq answered-attempt
              (gnosis-core-review-attempt-answer attempt-id answer))
      (gnosis-core-mutation-uncertain
       (setq uncertain condition))
      (gnosis-core-error
       (setq gnosis-review--answer-locked-p nil)
       (signal (car condition) (cdr condition))))
    (setq state (gnosis-core-review-attempt-state attempt-id))
    (let ((attempt (plist-get state :attempt)))
      (unless (equal attempt-id (plist-get attempt :attempt_id))
        (error "Authoritative attempt state changed identity"))
      (when (and answered-attempt
                 (equal "pending" (plist-get attempt :status)))
        (error "Authoritative attempt state did not advance"))
      (when (and uncertain
                 (or (equal "pending" (plist-get attempt :status))
                     (not (equal answer (plist-get attempt :response)))))
        (signal (car uncertain) (cdr uncertain)))
      (setq gnosis-review--attempt attempt
            gnosis-review--attempt-revision
            (plist-get state :attempt_revision))))
  (gnosis-review--render)
  (when show-popup
    (keymap-popup gnosis-review-mode-map)))

(defun gnosis-review--answer (&optional show-popup)
  "Submit one response and show actions when SHOW-POPUP is non-nil."
  (interactive '(t))
  (condition-case condition
      (gnosis-review--answer-current show-popup)
    (gnosis-core-conflict
     (if (gnosis-review--stale-condition-p condition)
         (progn
           (setq gnosis-review--answer-locked-p nil)
           (message "Review item changed; continuing")
           (gnosis-review--next))
       (signal (car condition) (cdr condition))))))

(defun gnosis-review--finish-current (accepted-success)
  "Set ACCEPTED-SUCCESS, commit the current attempt, and continue."
  (unless (gnosis-review--graded-p)
    (user-error "Current attempt is not graded"))
  (unless gnosis-review--attempt-revision (gnosis-review--load-revision))
  (let ((result
         (gnosis-review--revisioned-operation
          (lambda ()
            (gnosis-core-review-attempt-set-accepted-success
             (plist-get gnosis-review--attempt :attempt_id)
             gnosis-review--attempt-revision accepted-success))
          :accepted_success accepted-success)))
    (setq gnosis-review--attempt (plist-get result :attempt)
          gnosis-review--attempt-revision (plist-get result :attempt_revision)))
  (condition-case condition
      (gnosis-core-review-attempt-commit
       (plist-get gnosis-review--attempt :attempt_id))
    (gnosis-core-mutation-uncertain
     (gnosis-review--load-revision)
     (unless (equal "committed" (plist-get gnosis-review--attempt :status))
       (gnosis-review--render)
       (signal (car condition) (cdr condition)))))
  (gnosis-review--next))

(defun gnosis-review--finish (accepted-success)
  "Set ACCEPTED-SUCCESS, commit the current attempt, and continue."
  (condition-case condition
      (gnosis-review--finish-current accepted-success)
    (gnosis-core-conflict
     (if (gnosis-review--stale-condition-p condition)
         (progn
           (message "Review item changed; continuing")
           (gnosis-review--next))
       (signal (car condition) (cdr condition))))))

(defun gnosis-review--commit-next ()
  "Commit the current accepted result and continue."
  (interactive)
  (gnosis-review--finish
   (plist-get gnosis-review--attempt :accepted_success)))

(defun gnosis-review--override ()
  "Invert the current accepted result without committing the attempt."
  (interactive)
  (unless (gnosis-review--graded-p)
    (user-error "Current attempt is not graded"))
  (unless gnosis-review--attempt-revision (gnosis-review--load-revision))
  (let* ((desired
          (if (eq (plist-get gnosis-review--attempt :accepted_success) t)
              gnosis-core--json-false
            t))
         (result
          (gnosis-review--revisioned-operation
           (lambda ()
             (gnosis-core-review-attempt-set-accepted-success
              (plist-get gnosis-review--attempt :attempt_id)
              gnosis-review--attempt-revision desired))
           :accepted_success desired)))
    (setq gnosis-review--attempt (plist-get result :attempt)
          gnosis-review--attempt-revision (plist-get result :attempt_revision))
    (gnosis-review--render)
    (message "Gnosis result overridden")))

(defun gnosis-review--toggle-suspension ()
  "Toggle desired suspension for the current graded attempt."
  (interactive)
  (unless (gnosis-review--graded-p)
    (user-error "Current attempt is not graded"))
  (unless gnosis-review--attempt-revision (gnosis-review--load-revision))
  (let* ((desired (if (eq (plist-get gnosis-review--attempt :desired_suspended) t)
                      gnosis-core--json-false
                    t))
         (result
          (gnosis-review--revisioned-operation
           (lambda ()
             (gnosis-core-review-attempt-set-suspension
              (plist-get gnosis-review--attempt :attempt_id)
              gnosis-review--attempt-revision desired))
           :desired_suspended desired)))
    (setq gnosis-review--attempt (plist-get result :attempt)
          gnosis-review--attempt-revision (plist-get result :attempt_revision))
    (gnosis-review--render)
    (message "Gnosis suspension %s" (if (eq desired t) "enabled" "disabled"))))

(defun gnosis-review--refresh ()
  "Refresh the current attempt from gnosisd."
  (interactive)
  (if (equal "committed" (plist-get gnosis-review--attempt :status))
      (gnosis-review--next)
    (gnosis-review--load-revision)
    (gnosis-review--present)))

(defun gnosis-review--quit ()
  "Close the local review buffer without mutating its durable session."
  (interactive)
  (kill-buffer (current-buffer)))

(provide 'gnosis-review)
;;; gnosis-review.el ends here
