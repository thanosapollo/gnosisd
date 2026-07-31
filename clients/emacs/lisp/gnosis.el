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
(require 'tabulated-list)

(declare-function gnosis-add-thema "gnosis-edit")
(declare-function gnosis-edit-thema "gnosis-edit")
(declare-function gnosis-review "gnosis-review")
(declare-function gnosis-review-resume "gnosis-review")
(declare-function gnosis-review-topic "gnosis-review")
(autoload 'gnosis-review "gnosis-review" nil t)
(autoload 'gnosis-review-resume "gnosis-review" nil t)
(autoload 'gnosis-review-topic "gnosis-review" nil t)
(autoload 'gnosis-add-thema "gnosis-edit" nil t)
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
  '((t :inherit shadow :strike-through t))
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

(defconst gnosis-themata--catalog-max-items 50000
  "Maximum themata materialized by the management dashboard.")

(defvar-local gnosis-themata--generation nil
  "Generation of the visible thema catalog.")

(defvar-local gnosis-themata--records nil
  "Catalog records keyed by opaque thema identity.")

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

(keymap-popup-define gnosis-dashboard-mode-map
  "Keymap for `gnosis-dashboard-mode'."
  :popup-key "?"
  :exit-key "C-g"
  :description #'gnosis-dashboard--popup-description
  :group "Review"
  "r" (#'gnosis-dashboard--review-description gnosis-dashboard-review)
  "T" ("Review topic" gnosis-review-topic)
  "R" ("Active review controls" gnosis-review-resume
       :if #'gnosis-dashboard--active-session-p)
  :group "View"
  "n" ("Nodes / documents" gnosis-nodes-find)
  "t" ("Themata" gnosis-dashboard-themata)
  "g" ("Refresh" gnosis-dashboard-refresh :stay-open t)
  "q" ("Quit" quit-window))

(define-derived-mode gnosis-dashboard-mode special-mode "Gnosis"
  "Major mode for the Gnosis foundation dashboard."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-dashboard-refresh))))

(keymap-popup-define gnosis-dashboard-themata-mode-map
  "Keymap for `gnosis-dashboard-themata-mode'."
  :parent tabulated-list-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description "Gnosis Themata"
  :group "Themata"
  "RET" ("Edit at point" gnosis-themata-edit)
  "e" ("Edit at point" gnosis-themata-edit)
  "a" ("Add thema" gnosis-themata-add)
  "s" ("Suspend or unsuspend" gnosis-themata-toggle-suspension)
  :group "Navigate"
  "g" ("Refresh" gnosis-themata-refresh :stay-open t)
  "q" ("Dashboard" gnosis-dashboard))

(define-derived-mode gnosis-dashboard-themata-mode
  tabulated-list-mode "Gnosis Themata"
  "Major mode for the server-owned thema management projection."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines t
              tabulated-list-padding 2
              tabulated-list-sort-key (cons "Question" nil)
              tabulated-list-format
              [("Question" 36 t)
               ("Hypothesis" 24 t)
               ("Answer" 28 t)
               ("Tags" 20 t)
               ("Kind" 10 t)
               ("Archived" 9 t)
               ("Suspended" 9 t)]
              revert-buffer-function
              (lambda (&rest _) (gnosis-themata-refresh)))
  (tabulated-list-init-header))

(defun gnosis-themata--fetch ()
  "Return the complete generation-bound thema catalog."
  (let ((after nil)
        (done nil)
        (generation nil)
        (records nil)
        (seen-cursors (make-hash-table :test #'equal)))
    (while (not done)
      (let* ((page (gnosis-core-thema-catalog
                    nil after gnosis-core--thema-catalog-page-limit))
             (items (plist-get page :items))
             (next (plist-get page :next))
             (page-generation (plist-get page :generation)))
        (if generation
            (unless (equal generation page-generation)
              (signal 'gnosis-core-conflict
                      '("Gnosis thema catalog changed during pagination")))
          (setq generation page-generation))
        (when (> (+ (length records) (length items))
                 gnosis-themata--catalog-max-items)
          (signal 'gnosis-core-protocol-error
                  '("Gnosis thema catalog is too large")))
        (cl-loop for thema across items do (push thema records))
        (if (eq next gnosis-core--json-null)
            (setq done t)
          (when (or (= (length items) 0) (gethash next seen-cursors))
            (signal 'gnosis-core-protocol-error
                    '("Gnosis thema catalog did not advance")))
          (puthash next t seen-cursors)
          (setq after next))))
    (list :generation generation :items (vconcat (nreverse records)))))

(defun gnosis-themata--display-list (values)
  "Return one-line comma-separated text for vector VALUES."
  (mapconcat
   (lambda (value)
     (if (eq value gnosis-core--json-null) "" (format "%s" value)))
   (append values nil) ", "))

(defun gnosis-themata--display-text (value)
  "Return VALUE collapsed to one display line."
  (string-trim
   (replace-regexp-in-string "[[:space:]\n\r]+" " " (or value ""))))

(defun gnosis-themata--yes-no (value)
  "Return a readable boolean for wire VALUE."
  (if (eq value t) "Yes" "No"))

(defun gnosis-themata--entry (record)
  "Return a tabulated entry for catalog RECORD."
  (list
   (plist-get record :thema_id)
   (vector
    (gnosis-themata--display-text (plist-get record :keimenon))
    (gnosis-themata--display-text
     (gnosis-themata--display-list (plist-get record :hypothesis)))
    (gnosis-themata--display-text
     (gnosis-themata--display-list (plist-get record :answer)))
    (gnosis-themata--display-list (plist-get record :tags))
    (plist-get record :kind)
    (gnosis-themata--yes-no (plist-get record :archived))
    (gnosis-themata--yes-no (plist-get record :suspended)))))

(defun gnosis-themata--record-table (items)
  "Return catalog ITEMS keyed by thema identity."
  (let ((records (make-hash-table :test #'equal)))
    (cl-loop for record across items
             do (puthash (plist-get record :thema_id) record records))
    records))

(defun gnosis-themata--goto-id (thema-id)
  "Move point to THEMA-ID when it is visible."
  (goto-char (point-min))
  (while (and (not (eobp))
              (not (equal thema-id (tabulated-list-get-id))))
    (forward-line 1)))

(defun gnosis-themata--install (catalog &optional selected-id)
  "Install CATALOG in the current buffer and restore SELECTED-ID."
  (let ((items (plist-get catalog :items)))
    (setq gnosis-themata--generation (plist-get catalog :generation)
          gnosis-themata--records (gnosis-themata--record-table items)
          tabulated-list-entries
          (cl-loop for record across items
                   collect (gnosis-themata--entry record)))
    (tabulated-list-print t)
    (when selected-id
      (gnosis-themata--goto-id selected-id))))

(defun gnosis-themata-refresh ()
  "Refresh the complete thema management projection."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-themata-mode)
    (user-error "Not in the Gnosis themata dashboard"))
  (let ((selected-id (tabulated-list-get-id))
        (catalog (gnosis-themata--fetch)))
    (gnosis-themata--install catalog selected-id)))

;;;###autoload
(defun gnosis-dashboard-themata ()
  "Open the complete server-projected thema management list."
  (interactive)
  (let ((catalog (gnosis-themata--fetch))
        (buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (gnosis-dashboard-themata-mode)
    (gnosis-themata--install catalog)
    (keymap-popup gnosis-dashboard-themata-mode-map)))

(defun gnosis-themata--record-at-point ()
  "Return the catalog record at point or signal a user error."
  (let ((thema-id (tabulated-list-get-id)))
    (or (and thema-id (gethash thema-id gnosis-themata--records))
        (user-error "No thema at point"))))

(defun gnosis-themata--replace-record (record)
  "Replace visible catalog RECORD after an authoritative mutation."
  (let ((thema-id (plist-get record :thema_id)))
    (puthash thema-id record gnosis-themata--records)
    (setq tabulated-list-entries
          (mapcar (lambda (entry)
                    (if (equal thema-id (car entry))
                        (gnosis-themata--entry record)
                      entry))
                  tabulated-list-entries))
    (tabulated-list-print t)
    (gnosis-themata--goto-id thema-id)))

(defun gnosis-themata--edited (buffer projection)
  "Update BUFFER from authoritative content PROJECTION."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (derived-mode-p 'gnosis-dashboard-themata-mode)
        (let* ((thema-id (plist-get projection :thema_id))
               (record (copy-tree (gethash thema-id gnosis-themata--records))))
          (if record
              (progn
                (dolist (field '(:kind :keimenon :hypothesis :answer
                                 :tags :archived))
                  (setq record
                        (plist-put record field (plist-get projection field))))
                (setq record
                      (plist-put record :thema_revision
                                 (plist-get projection :revision)))
                (gnosis-themata--replace-record record))
            (gnosis-themata-refresh)))))))

(defun gnosis-themata-edit ()
  "Open the shared editor for the thema at point."
  (interactive)
  (let ((buffer (current-buffer))
        (thema-id (plist-get (gnosis-themata--record-at-point) :thema_id)))
    (gnosis-edit-thema
     thema-id (lambda (projection)
                (gnosis-themata--edited buffer projection)))))

(defun gnosis-themata-add ()
  "Open the shared editor for a new thema."
  (interactive)
  (let ((buffer (current-buffer)))
    (gnosis-add-thema
     (lambda (_projection)
       (when (buffer-live-p buffer)
         (with-current-buffer buffer
           (when (derived-mode-p 'gnosis-dashboard-themata-mode)
             (gnosis-themata-refresh))))))))

(defun gnosis-themata-toggle-suspension ()
  "Suspend or unsuspend the thema at point."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (thema-id (plist-get record :thema_id))
         (suspended (eq t (plist-get record :suspended)))
         (desired (not suspended))
         (action (if desired "Suspend" "Unsuspend")))
    (when (y-or-n-p (format "%s thema at point? " action))
      (let* ((projection
              (gnosis-core-thema-set-suspended
               thema-id (plist-get record :state_revision) desired))
             (state (plist-get projection :state))
             (updated (copy-tree record)))
        (setq updated
              (plist-put updated :state_revision
                         (plist-get state :state_revision)))
        (setq updated
              (plist-put updated :suspended (plist-get state :suspended)))
        (gnosis-themata--replace-record updated)
        (message "%s thema %s"
                 (if desired "Suspended" "Unsuspended") thema-id)))))

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

(defun gnosis-dashboard--load (&optional start-at-top)
  "Load and render the authenticated dashboard, optionally START-AT-TOP."
  (let ((state (gnosis-core-dashboard)))
    (gnosis--render-dashboard state start-at-top)
    (setq gnosis-dashboard--projection state)))

(defun gnosis-dashboard-refresh ()
  "Refresh the current Gnosis dashboard from the server projection."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
    (user-error "Not in a Gnosis dashboard"))
  (condition-case condition
      (gnosis-dashboard--load)
    (gnosis-core-error
     (user-error "%s" (gnosis-dashboard--failure-message condition)))))

;;;###autoload
(defun gnosis-dashboard ()
  "Open the server-projected Gnosis dashboard."
  (interactive)
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (unless (derived-mode-p 'gnosis-dashboard-mode)
      (gnosis-dashboard-mode))
    (condition-case condition
        (gnosis-dashboard--load t)
      (gnosis-core-error
       (setq gnosis-dashboard--projection nil)
       (gnosis--render-dashboard-failure condition)))
    (keymap-popup gnosis-dashboard-mode-map)))

;;;###autoload
(defun gnosis ()
  "Open the server-projected Gnosis dashboard."
  (interactive)
  (gnosis-dashboard))

(provide 'gnosis)
;;; gnosis.el ends here
