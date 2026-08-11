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
(require 'seq)

(declare-function gnosis-add-thema "gnosis-edit")
(declare-function gnosis-edit-read-kind "gnosis-edit")
(declare-function gnosis-edit-thema "gnosis-edit")
(declare-function gnosis-review "gnosis-review")
(declare-function gnosis-review-resume "gnosis-review")
(declare-function gnosis-review-topic "gnosis-review")
(autoload 'gnosis-review "gnosis-review" nil t)
(autoload 'gnosis-review-resume "gnosis-review" nil t)
(autoload 'gnosis-review-topic "gnosis-review" nil t)
(autoload 'gnosis-add-thema "gnosis-edit" nil t)
(autoload 'gnosis-edit-read-kind "gnosis-edit")
(autoload 'gnosis-edit-thema "gnosis-edit" nil t)

(defgroup gnosis nil
  "Emacs client for Gnosis."
  :group 'applications
  :prefix "gnosis-")

(defface gnosis-face-correct
  '((t :inherit success))
  "Face reserved for successful review content."
  :group 'gnosis)

(defface gnosis-face-cloze
  '((t :inherit (highlight italic)))
  "Face for concealed clozes."
  :group 'gnosis)

(defface gnosis-face-false
  '((t :inherit error))
  "Face reserved for unsuccessful review content."
  :group 'gnosis)

(defface gnosis-face-unanswered
  '((t :inherit (italic underline)))
  "Face for clozes revealed after an incorrect answer."
  :group 'gnosis)

(defface gnosis-face-hint
  '((t :inherit warning))
  "Face for Gnosis hints and metadata."
  :group 'gnosis)

(defface gnosis-face-separator
  '((default :inherit org-hide)
    (((background light)) :strike-through "gray70")
    (t :strike-through "gray30"))
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
  (cond ((not gnosis-dashboard--projection) "Gnosis Dashboard — unavailable")
        ((gnosis-dashboard--active-session-p) "Gnosis Dashboard — active review")
        (t "Gnosis Dashboard — ready")))

(defun gnosis-dashboard--review-description ()
  "Return the review action appropriate for the dashboard projection."
  (if (gnosis-dashboard--active-session-p) "Resume review" "Start review"))

(defun gnosis-dashboard-review ()
  "Start or resume the server-owned due review flow."
  (interactive)
  (gnosis-review nil))

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
  "Return STRING filled and centered for the selected window width."
  (if (string-blank-p string)
      ""
    (let* ((width (max 1 (window-body-width)))
           (wrapped
            (with-temp-buffer
              (insert (string-trim string))
              (let ((fill-column (min fill-column width)))
                (fill-region (point-min) (point-max)))
              (buffer-string))))
      (mapconcat
       (lambda (line)
         (let ((padding (max 0 (/ (- width (string-width line)) 2))))
           (concat (make-string padding ?\s) line)))
       (split-string wrapped "\n") "\n"))))

(defun gnosis--center-text (text)
  "Return TEXT centered line by line when centering is enabled."
  (if gnosis-center-content
      (mapconcat #'gnosis-center-string (split-string text "\n") "\n")
    text))

(defun gnosis-format-string (string)
  "Return STRING prepared for the current Gnosis presentation buffer."
  (gnosis--center-text (gnosis-org-format-string string)))

(defun gnosis--separator ()
  "Return a full-width Gnosis separator."
  (propertize " " 'display '(space :width text)
              'face 'gnosis-face-separator))

(defun gnosis--dashboard-style (text face)
  "Return dashboard TEXT displayed with FACE."
  (propertize text 'face face))

(defun gnosis--dashboard-success-rate (window)
  "Return display text for WINDOW success rate."
  (let ((rate (plist-get window :success_rate)))
    (if (eq rate gnosis-core--json-null)
        (gnosis--dashboard-style "unavailable" 'shadow)
      (gnosis--dashboard-style (format "%.2f" rate) 'success))))

(defun gnosis--dashboard-connection-text ()
  "Return the successful authenticated connection state."
  (format "%s %s"
          (gnosis--dashboard-style "Connection:" 'shadow)
          (gnosis--dashboard-style "Ready" 'success)))

(defun gnosis--dashboard-today-text (today)
  "Return the dashboard summary for TODAY."
  (format "%s %s (New: %s); %s, %s"
          (gnosis--dashboard-style "Reviewed today:" 'shadow)
          (gnosis--dashboard-style
           (number-to-string (plist-get today :review_count))
           'font-lock-type-face)
          (gnosis--dashboard-style
           (number-to-string (plist-get today :new_count))
           'font-lock-type-face)
          (gnosis--dashboard-style
           (format "%d successful" (plist-get today :success_count))
           'success)
          (gnosis--dashboard-style
           (format "%d failed" (plist-get today :failure_count))
           'error)))

(defun gnosis--dashboard-queue-text (queue)
  "Return review availability text for server-projected QUEUE."
  (let ((due (plist-get queue :due_count))
        (overdue (plist-get queue :overdue_count)))
    (format "%s %s (Overdue: %s)"
            (gnosis--dashboard-style "Due themata:" 'shadow)
            (gnosis--dashboard-style
             (number-to-string due) (if (> due 0) 'warning 'success))
            (gnosis--dashboard-style
             (number-to-string overdue) (if (> overdue 0) 'error 'success)))))

(defun gnosis--dashboard-content-text (content)
  "Return the dashboard summary for CONTENT."
  (format "%s %s, %s, %s, %s"
          (gnosis--dashboard-style "Content:" 'shadow)
          (gnosis--dashboard-style
           (format "%d themata" (plist-get content :active_themata))
           'font-lock-type-face)
          (gnosis--dashboard-style
           (format "%d archived" (plist-get content :archived_themata))
           'shadow)
          (gnosis--dashboard-style
           (format "%d documents" (plist-get content :active_documents))
           'font-lock-type-face)
          (gnosis--dashboard-style
           (format "%d nodes" (plist-get content :active_nodes))
           'font-lock-type-face)))

(defun gnosis--dashboard-window-text (window)
  "Return the dashboard activity summary for WINDOW."
  (format "%s%s%s %s, success rate %s"
          (gnosis--dashboard-style "Last " 'shadow)
          (gnosis--dashboard-style
           (number-to-string (plist-get window :days))
           'font-lock-type-face)
          (gnosis--dashboard-style " days:" 'shadow)
          (gnosis--dashboard-style
           (format "%d reviews"
                   (plist-get (plist-get window :totals) :review_count))
           'font-lock-type-face)
          (gnosis--dashboard-success-rate window)))

(defun gnosis--dashboard-session-text (session)
  "Return a warning when server-projected SESSION is active."
  (unless (eq session gnosis-core--json-null)
    (gnosis--dashboard-style "Active session in progress" 'warning)))

(defun gnosis--dashboard-text (state)
  "Return display text for server dashboard projection STATE."
  (let ((today (plist-get state :today))
        (window (plist-get state :window))
        (queue (plist-get state :queue))
        (content (plist-get state :content))
        (session (plist-get state :active_session)))
    (string-join
     (append
      (list (gnosis--dashboard-connection-text)
            (gnosis--dashboard-today-text today)
            (gnosis--dashboard-queue-text queue)
            (gnosis--dashboard-content-text content)
            (gnosis--dashboard-window-text window))
      (when-let* ((warning (gnosis--dashboard-session-text session)))
        (list warning)))
     "\n")))

(defun gnosis--render-dashboard (state &optional start-at-top)
  "Render dashboard STATE, starting at top when START-AT-TOP."
  (let ((inhibit-read-only t)
        (old-point (point))
        (old-window-start
         (and (eq (window-buffer (selected-window)) (current-buffer))
              (window-start)))
        (text
         (concat
          (propertize (gnosis--center-text "Gnosis Dashboard")
                      'face 'gnosis-face-dashboard-header)
          "\n" (gnosis--separator) "\n\n"
          (gnosis--center-text (gnosis--dashboard-text state)))))
    (erase-buffer)
    (insert text)
    (goto-char (if start-at-top (point-min) (min old-point (point-max))))
    (when old-window-start
      (set-window-start (selected-window)
                        (if start-at-top
                            (point-min)
                          (min old-window-start (point-max)))
                        t))))

(defun gnosis-dashboard--failure-message (condition)
  "Return public-safe corrective text for typed CONDITION."
  (pcase (car condition)
    ('gnosis-core-authentication-error
     "Gnosis authentication failed. Check auth-source token for gnosisd.")
    ('gnosis-core-authorization-error
     "Gnosis access denied. Check gnosisd token scopes.")
    ((or 'gnosis-core-unavailable 'gnosis-core-timeout
         'gnosis-core-transport-error)
     "gnosisd unavailable. Check service and gnosis-core-base-url.")
    (_ "Gnosis dashboard unavailable. Check gnosisd and client/server versions.")))

(defun gnosis--render-dashboard-failure (condition)
  "Render a safe corrective dashboard for typed CONDITION."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (insert
     (propertize (gnosis--center-text "Gnosis Dashboard")
                 'face 'gnosis-face-dashboard-header)
     "\n" (gnosis--separator) "\n\n"
     (gnosis--center-text
      (propertize (gnosis-dashboard--failure-message condition) 'face 'error)))
    (goto-char (point-min))))

(autoload 'gnosis-dashboard "gnosis-dashboard" nil t)

;;;###autoload
(defun gnosis ()
  "Open the server-projected Gnosis dashboard."
  (interactive)
  (gnosis-dashboard))

(provide 'gnosis)
;;; gnosis.el ends here
