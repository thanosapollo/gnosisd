;;; gnosis.el --- Emacs client for Gnosis  -*- lexical-binding: t; -*-

;; Copyright (C) 2022-2026  Free Software Foundation, Inc.
;; Author: Thanos Apollo <public@thanosapollo.org>
;; URL: https://thanosapollo.org/projects/gnosis
;; Version: 0.13.0
;; Package-Requires: ((emacs "29.1") (keymap-popup "0.4.0"))
;; Keywords: extensions, learning

;; This file is free software; you can redistribute it and/or modify it
;; under the terms of the GNU General Public License as published by the
;; Free Software Foundation, either version 3 of the License, or (at your
;; option) any later version.

;;; Commentary:

;; Gnosis presents server-owned dashboard, authoring, and review projections
;; from the authenticated gnosisd HTTP API.

;;; Code:

(require 'gnosis-core)
(require 'gnosis-nodes)
(require 'keymap-popup)
(require 'org)

(declare-function gnosis-review "gnosis-review")
(declare-function gnosis-review-resume "gnosis-review")
(declare-function gnosis-review-topic "gnosis-review")
(autoload 'gnosis-review "gnosis-review" nil t)
(autoload 'gnosis-review-resume "gnosis-review" nil t)
(autoload 'gnosis-review-topic "gnosis-review" nil t)

(defgroup gnosis nil
  "Emacs client for Gnosis."
  :group 'applications
  :prefix "gnosis-")

(defface gnosis-face-correct
  '((t :inherit success))
  "Face reserved for successful review content."
  :group 'gnosis)

(defface gnosis-face-false
  '((t :inherit error))
  "Face reserved for unsuccessful review content."
  :group 'gnosis)

(defface gnosis-face-hint
  '((t :inherit warning))
  "Face for Gnosis hints and metadata."
  :group 'gnosis)

(defface gnosis-face-separator
  '((((background light)) :strike-through "gray70" :foreground "gray70")
    (t :strike-through "gray30" :foreground "gray30"))
  "Face reserved for review separators."
  :group 'gnosis)

(defface gnosis-face-directions
  '((t :inherit underline))
  "Face reserved for review labels."
  :group 'gnosis)

(defface gnosis-face-next-review
  '((t :inherit bold))
  "Face reserved for the next review date."
  :group 'gnosis)

(defface gnosis-face-dashboard-header
  '((t :inherit (bold font-lock-constant-face)))
  "Face for the Gnosis dashboard header."
  :group 'gnosis)

(defcustom gnosis-center-content-during-review t
  "Non-nil means center content in future review buffers."
  :type 'boolean
  :group 'gnosis)

(defcustom gnosis-latex-preview nil
  "Non-nil means render LaTeX fragments in Gnosis content."
  :type 'boolean
  :group 'gnosis)

(defvar gnosis-dashboard-buffer-name "*Gnosis Dashboard*"
  "Gnosis dashboard buffer name.")

(defvar-local gnosis-center-content t
  "Non-nil means center content in the current Gnosis buffer.")

(defvar-local gnosis-dashboard--projection nil
  "Current validated server dashboard projection.")

(defun gnosis-dashboard--active-session-p ()
  "Return non-nil when the dashboard has an active review session."
  (and gnosis-dashboard--projection
       (not (eq (plist-get gnosis-dashboard--projection :active_session)
                gnosis-core--json-null))))

(defun gnosis-dashboard--popup-description ()
  "Return context text for dashboard popup help."
  (if (gnosis-dashboard--active-session-p)
      "Gnosis Dashboard — active review"
    "Gnosis Dashboard — no active review"))

(keymap-popup-define gnosis-dashboard-mode-map
  "Keymap for `gnosis-dashboard-mode'."
  :popup-key "?"
  :exit-key "C-g"
  :description #'gnosis-dashboard--popup-description
  :group "Review"
  "r" ("Review due" gnosis-review)
  "t" ("Review topic" gnosis-review-topic)
  "R" ("Resume active review" gnosis-review-resume
       :if #'gnosis-dashboard--active-session-p)
  :group "View"
  "g" ("Refresh" gnosis-dashboard-refresh :stay-open t)
  "q" ("Quit" quit-window)
  "?" ("Actions" gnosis-dashboard-mode-map-popup))

(define-derived-mode gnosis-dashboard-mode special-mode "Gnosis"
  "Major mode for the Gnosis foundation dashboard."
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-dashboard-refresh))))

(defun gnosis--display-link-descriptions (string)
  "Return STRING with bracket links replaced by their descriptions."
  (replace-regexp-in-string
   "\\[\\[[^]]+\\]\\[\\([^]]+\\)\\]\\]" "\\1" (or string "") t))

(defun gnosis--materialize-display-overlays ()
  "Copy display overlays in the current buffer to text properties."
  (dolist (overlay (overlays-in (point-min) (point-max)))
    (when-let* ((display (overlay-get overlay 'display)))
      (put-text-property (overlay-start overlay) (overlay-end overlay)
                         'display display))
    (delete-overlay overlay)))

(defun gnosis-org-format-string (string)
  "Return STRING fontified for display like Org content."
  (with-temp-buffer
    (org-mode)
    (setq-local org-pretty-entities t)
    (insert (gnosis--display-link-descriptions string))
    (font-lock-ensure)
    (when gnosis-latex-preview
      (org-latex-preview '(16))
      (gnosis--materialize-display-overlays))
    (buffer-string)))

(defun gnosis-center-string (string)
  "Return STRING centered for the selected window width."
  (let* ((width (max 1 (window-body-width)))
         (padding (max 0 (/ (- width (string-width string)) 2))))
    (concat (make-string padding ?\s) string)))

(defun gnosis-format-string (string)
  "Return STRING prepared for the current Gnosis presentation buffer."
  (let ((formatted (gnosis-org-format-string string)))
    (if gnosis-center-content
        (mapconcat #'gnosis-center-string (split-string formatted "\n") "\n")
      formatted)))

(defun gnosis--dashboard-session-text (session)
  "Return dashboard text for server-projected SESSION metadata."
  (if (eq session gnosis-core--json-null)
      (concat "Active session: " (propertize "none" 'face 'shadow))
    (format "Active session: %s, %s, position %d, queue length %d"
            (plist-get session :status)
            (plist-get session :mode)
            (plist-get session :current_position)
            (plist-get session :queue_length))))

(defun gnosis--face-dashboard-match (text face)
  "Apply FACE to first dashboard match for TEXT after point."
  (when (search-forward text nil t)
    (add-face-text-property (- (point) (length text)) (point) face)))

(defun gnosis--dashboard-actions (active-session)
  "Return dashboard action help for ACTIVE-SESSION state."
  (string-join
   (append '("r Review due" "t Review topic")
           (and (not (eq active-session gnosis-core--json-null))
                '("R Resume review"))
           '("g Refresh" "? Actions" "q Quit"))
   "   "))

(defun gnosis--dashboard-success-rate (window)
  "Return display text for WINDOW success rate."
  (let ((rate (plist-get window :success_rate)))
    (if (eq rate gnosis-core--json-null)
        "unavailable"
      (format "%.2f" rate))))

(defun gnosis--dashboard-text (state)
  "Return display text for server dashboard projection STATE."
  (let ((today (plist-get state :today))
        (window (plist-get state :window))
        (queue (plist-get state :queue))
        (content (plist-get state :content)))
    (string-join
     (list
      "Gnosis" ""
      (format "Review day: %d" (plist-get state :review_day))
      (format "Today: %d reviews, %d successful, %d failed"
              (plist-get today :review_count)
              (plist-get today :success_count)
              (plist-get today :failure_count))
      (format "Queue: %d new, %d due, %d overdue, %d suspended"
              (plist-get queue :new_count) (plist-get queue :due_count)
              (plist-get queue :overdue_count) (plist-get queue :suspended_count))
      (format "Content: %d themata, %d archived, %d documents, %d nodes"
              (plist-get content :active_themata)
              (plist-get content :archived_themata)
              (plist-get content :active_documents)
              (plist-get content :active_nodes))
      (format "Last %d days: %d reviews, success rate %s"
              (plist-get window :days)
              (plist-get (plist-get window :totals) :review_count)
              (gnosis--dashboard-success-rate window))
      (gnosis--dashboard-session-text (plist-get state :active_session))
      ""
      (gnosis--dashboard-actions (plist-get state :active_session)))
     "\n")))

(defun gnosis--render-dashboard (state)
  "Render server dashboard projection STATE in the current buffer."
  (let ((inhibit-read-only t)
        (old-point (point))
        (old-window-start
         (and (eq (window-buffer (selected-window)) (current-buffer))
              (window-start))))
    (erase-buffer)
    (insert (gnosis-format-string (gnosis--dashboard-text state)))
    (goto-char (point-min))
    (add-face-text-property
     (point-min) (line-end-position) 'gnosis-face-dashboard-header)
    (let ((today (plist-get state :today))
          (queue (plist-get state :queue))
          (content (plist-get state :content))
          (window (plist-get state :window)))
      (dolist (spec
               `((,(number-to-string (plist-get state :review_day))
                  . font-lock-type-face)
                 (,(format "%d reviews" (plist-get today :review_count))
                  . font-lock-type-face)
                 (,(format "%d successful" (plist-get today :success_count))
                  . success)
                 (,(format "%d failed" (plist-get today :failure_count)) . error)
                 (,(format "%d new" (plist-get queue :new_count))
                  . font-lock-constant-face)
                 (,(format "%d due" (plist-get queue :due_count)) . warning)
                 (,(format "%d overdue" (plist-get queue :overdue_count)) . error)
                 (,(format "%d suspended" (plist-get queue :suspended_count)) . shadow)
                 (,(format "%d themata" (plist-get content :active_themata))
                  . font-lock-type-face)
                 (,(format "%d archived" (plist-get content :archived_themata)) . shadow)
                 (,(format "%d documents" (plist-get content :active_documents))
                  . font-lock-type-face)
                 (,(format "%d nodes" (plist-get content :active_nodes))
                  . font-lock-type-face)
                 (,(format "%d reviews"
                           (plist-get (plist-get window :totals) :review_count))
                  . font-lock-type-face)
                 (,(gnosis--dashboard-success-rate window)
                  . ,(if (eq (plist-get window :success_rate)
                            gnosis-core--json-null)
                         'shadow
                       'success))))
        (goto-char (point-min))
        (gnosis--face-dashboard-match (car spec) (cdr spec))))
    (goto-char (point-min))
    (while (re-search-forward
            "\\(?:Review day\\|Today\\|Queue\\|Content\\|Last 30 days\\|Active session\\):"
            nil t)
      (add-face-text-property (match-beginning 0) (match-end 0) 'shadow))
    (when (eq (plist-get state :active_session) gnosis-core--json-null)
      (goto-char (point-min))
      (gnosis--face-dashboard-match "none" 'shadow))
    (unless (eq (plist-get state :active_session) gnosis-core--json-null)
      (goto-char (point-min))
      (search-forward "Active session: ")
      (add-face-text-property (point) (line-end-position) 'font-lock-type-face))
    (goto-char (min old-point (point-max)))
    (when old-window-start
      (set-window-start (selected-window)
                        (min old-window-start (point-max)) t))))

(defun gnosis-dashboard-refresh ()
  "Refresh the current Gnosis dashboard from the server projection."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
    (user-error "Not in a Gnosis dashboard"))
  (let ((state (gnosis-core-dashboard)))
    (gnosis--render-dashboard state)
    (setq gnosis-dashboard--projection state)))

(defun gnosis-dashboard ()
  "Open the server-projected Gnosis dashboard."
  (interactive)
  (let ((state (gnosis-core-dashboard))
        (buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (gnosis-dashboard-mode)
    (gnosis--render-dashboard state)
    (setq gnosis-dashboard--projection state)))

;;;###autoload
(defun gnosis ()
  "Start a server-owned due review."
  (interactive)
  (call-interactively #'gnosis-review))

(provide 'gnosis)
;;; gnosis.el ends here
