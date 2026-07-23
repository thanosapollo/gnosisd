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

(defcustom gnosis-review-input-method-alist
  '(("greek" . "greek")
    ("cyrillic" . "cyrillic-translit"))
  "Map server input-script names to Emacs input methods."
  :type '(alist :key-type string :value-type string)
  :group 'gnosis)

(defun gnosis-review--pending-p ()
  "Return non-nil when the current attempt awaits an answer."
  (equal "pending" (plist-get gnosis-review--attempt :status)))

(defun gnosis-review--session-label ()
  "Return a compact progress label for the current session."
  (format "Review %d/%d"
          (1+ (plist-get gnosis-review--session :current_position))
          (plist-get gnosis-review--session :queue_length)))

(defun gnosis-review--popup-description ()
  "Return public-safe context text for review popup help."
  (format "%s — %s" (gnosis-review--session-label)
          (if (gnosis-review--pending-p) "answer" "result")))

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
  :group "Attempt"
  "RET" ("Answer" gnosis-review--answer :if #'gnosis-review--pending-p)
  "n" ("Commit & next" gnosis-review--commit-next
       :inapt-if #'gnosis-review--pending-p)
  "o" ("Override & next" gnosis-review--override
       :inapt-if #'gnosis-review--pending-p)
  "s" (#'gnosis-review--suspension-description gnosis-review--toggle-suspension
       :inapt-if #'gnosis-review--pending-p :stay-open t)
  :group "Session"
  "g" ("Refresh" gnosis-review--refresh :stay-open t)
  "q" ("Close review" gnosis-review--quit)
  "?" ("Actions" gnosis-review-mode-map-popup))

(define-derived-mode gnosis-review-mode special-mode "Gnosis-Review"
  "Major mode for server-authoritative online Gnosis review."
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
  (concat "Review "
          (propertize
           (number-to-string
            (1+ (plist-get gnosis-review--session :current_position)))
           'face 'font-lock-type-face)
          (propertize "/" 'face 'shadow)
          (propertize
           (number-to-string (plist-get gnosis-review--session :queue_length))
           'face 'font-lock-type-face)))

(defun gnosis-review--session-complete-p (session)
  "Return non-nil when SESSION has no remaining review item."
  (equal (plist-get session :status) "completed"))

(defun gnosis-review--insert-image (reference)
  "Insert managed image REFERENCE when display support exists."
  (when (stringp reference)
    (if (not (display-images-p))
        (insert (propertize "[Review image available]\n\n"
                            'face 'gnosis-face-hint))
      (condition-case nil
          (if-let* ((image (create-image
                            (gnosis-core-assets-resolve reference))))
              (progn (insert-image image) (insert "\n\n"))
            (insert (propertize "[Review image unavailable]\n\n"
                                'face 'gnosis-face-hint)))
        (error
         (insert (propertize "[Review image unavailable]\n\n"
                             'face 'gnosis-face-hint)))))))

(defun gnosis-review--response-text (response)
  "Return display text for structured RESPONSE."
  (let ((value (plist-get response :value)))
    (if (vectorp value)
        (string-join (append value nil) "\n")
      value)))

(defun gnosis-review--content-text (text &optional face)
  "Return Org-fontified display TEXT, optionally with semantic FACE."
  (let ((formatted (string-trim-right (gnosis-format-string text))))
    (if face (propertize formatted 'face face) formatted)))

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
    (insert (propertize (make-string 40 ?─) 'face 'gnosis-face-separator)
            "\n\n")
    (gnosis-review--insert-image
     (let ((value (plist-get presentation :review_image)))
       (unless (eq value gnosis-core--json-null) value)))
    (insert (gnosis-review--content-text (plist-get presentation :question))
            "\n\n")
    (if pending
        (insert (propertize
                 "RET Answer   g Refresh   q Close review   ? Actions"
                 'face 'gnosis-face-hint))
      (let* ((detected (plist-get gnosis-review--attempt :detected_success))
             (accepted (plist-get gnosis-review--attempt :accepted_success))
             (result-face (if (eq detected t)
                              'gnosis-face-correct 'gnosis-face-false))
             (response (gnosis-review--response-text
                        (plist-get gnosis-review--attempt :response)))
             (suspended
              (eq (plist-get gnosis-review--attempt :desired_suspended) t)))
        (insert (propertize "Answer:" 'face 'gnosis-face-directions) "\n"
                (gnosis-review--content-text
                 (mapconcat (lambda (item) (concat "• " item))
                            (plist-get gnosis-review--attempt :revealed_answer)
                            "\n")
                 'gnosis-face-correct)
                "\n\n"
                (propertize "Your answer:" 'face 'gnosis-face-directions) "\n"
                (gnosis-review--content-text response result-face)
                "\n\n")
        (unless (eq (plist-get gnosis-review--attempt :revealed_parathema)
                    gnosis-core--json-null)
          (insert (gnosis-review--content-text
                   (plist-get gnosis-review--attempt :revealed_parathema)
                   'gnosis-face-hint)
                  "\n\n"))
        (insert (propertize
                 (if (eq detected t) "Detected: success" "Detected: failure")
                 'face result-face))
        (unless (equal detected accepted)
          (insert "\n"
                  (propertize
                   (if (eq accepted t) "Accepted: success" "Accepted: failure")
                   'face (if (eq accepted t)
                             'gnosis-face-correct 'gnosis-face-false))))
        (insert "\n" (propertize "Suspension: " 'face 'gnosis-face-directions)
                (propertize (if suspended "enabled" "disabled")
                            'face (if suspended 'warning 'shadow))
                "\n\nn Commit & next   o Override & next   s "
                (gnosis-review--suspension-description)
                "   g Refresh   q Close review   ? Actions\n")))
    (goto-char (if start-at-top (point-min) (min old-point (point-max))))
    (when old-window-start
      (set-window-start
       (selected-window)
       (if start-at-top (point-min) (min old-window-start (point-max))) t))))

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
       (list :kind "texts" :value
             (cl-loop for script across (plist-get prompt :input_scripts)
                      for index from 1
                      collect (gnosis-review--read-string
                               (if (eq script gnosis-core--json-null)
                                   (format "Answer %d: " index)
                                 (format "Answer %d (%s): " index script))
                               script)
                      into answers
                      finally return (vconcat answers))))
      ("choice"
       (list :kind "choice" :value
             (gnosis-completing-read
              "Answer: " (append (plist-get prompt :choices) nil) t)))
      (_ (user-error "Unsupported review response prompt")))))

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
       (gnosis-review--load-revision)
       (gnosis-review--render)
       (user-error "Review attempt changed on server; refreshed"))
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

(defun gnosis-review--next ()
  "Advance to the next server attempt or close a completed session."
  (let* ((session-id (plist-get gnosis-review--session :session_id))
         (session (gnosis-review--active-session)))
    (when (and session
               (not (equal session-id (plist-get session :session_id))))
      (gnosis-core--invalid "Gnosis active review session identity mismatch"))
    (if (null session)
        (progn
          (setq gnosis-review--session nil
                gnosis-review--attempt nil
                gnosis-review--attempt-revision nil)
          (message "Gnosis review complete")
          (kill-buffer (current-buffer)))
      (let ((attempt (gnosis-core-review-attempt-begin session-id)))
        (setq gnosis-review--session session
              gnosis-review--attempt attempt
              gnosis-review--attempt-revision nil)
        (gnosis-review--render t)))))

(defun gnosis-review--open (session)
  "Open a review buffer for active SESSION."
  (if (gnosis-review--session-complete-p session)
      (message "No Gnosis review items are available")
    (let ((attempt
           (gnosis-core-review-attempt-begin
            (plist-get session :session_id)))
          (buffer (get-buffer-create gnosis-review-buffer-name)))
      (pop-to-buffer buffer)
      (gnosis-review-mode)
      (setq gnosis-review--session session
            gnosis-review--attempt attempt
            gnosis-review--attempt-revision nil)
      (gnosis-review--render t))))

(defun gnosis-review--start (source target-count)
  "Start a review for SOURCE and optional TARGET-COUNT."
  (gnosis-core-status)
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

;;;###autoload
(defun gnosis-review (&optional target-count)
  "Start a server-owned due review of optional TARGET-COUNT."
  (interactive
   (list (let ((count (read-number "Target count (0 for server default): " 0)))
           (and (> count 0) count))))
  (gnosis-review--start (gnosis-review--source "due") target-count))

;;;###autoload
(defun gnosis-review-topic (topic-node-id &optional target-count)
  "Start a server-owned review for TOPIC-NODE-ID and TARGET-COUNT."
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

(defun gnosis-review--answer ()
  "Submit one structured response for the current pending attempt."
  (interactive)
  (unless (equal (plist-get gnosis-review--attempt :status) "pending")
    (user-error "Attempt already answered"))
  (let* ((attempt-id (plist-get gnosis-review--attempt :attempt_id))
         (answer (gnosis-review--read-answer))
         uncertain
         state)
    (condition-case condition
        (progn
          (gnosis-core-review-attempt-answer attempt-id answer)
          (setq state (gnosis-core-review-attempt-state attempt-id)))
      (gnosis-core-mutation-uncertain
       (setq uncertain condition
             state (gnosis-core-review-attempt-state attempt-id))))
    (let ((attempt (plist-get state :attempt)))
      (when (and uncertain
                 (or (equal "pending" (plist-get attempt :status))
                     (not (equal answer (plist-get attempt :response)))))
        (signal (car uncertain) (cdr uncertain)))
      (setq gnosis-review--attempt attempt
            gnosis-review--attempt-revision
            (plist-get state :attempt_revision))))
  (gnosis-review--render))

(defun gnosis-review--finish (accepted-success)
  "Set ACCEPTED-SUCCESS, commit the current attempt, and continue."
  (when (equal (plist-get gnosis-review--attempt :status) "pending")
    (user-error "Answer attempt before grading it"))
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

(defun gnosis-review--commit-next ()
  "Commit the current accepted result and continue."
  (interactive)
  (gnosis-review--finish
   (plist-get gnosis-review--attempt :accepted_success)))

(defun gnosis-review--override ()
  "Invert the current accepted result, commit it, and continue."
  (interactive)
  (gnosis-review--finish
   (if (eq (plist-get gnosis-review--attempt :accepted_success) t)
       gnosis-core--json-false
     t)))

(defun gnosis-review--toggle-suspension ()
  "Toggle desired suspension for the current graded attempt."
  (interactive)
  (when (equal (plist-get gnosis-review--attempt :status) "pending")
    (user-error "Answer attempt before changing suspension"))
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
  (gnosis-review--load-revision)
  (gnosis-review--render))

(defun gnosis-review--quit ()
  "Close the local review buffer without mutating its durable session."
  (interactive)
  (kill-buffer (current-buffer)))

(provide 'gnosis-review)
;;; gnosis-review.el ends here
