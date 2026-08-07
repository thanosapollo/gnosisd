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

(defcustom gnosis-themata-cache-file
  (locate-user-emacs-file "gnosis/themata-cache.json")
  "Disposable persistent cache for the complete thema catalog projection."
  :type 'file
  :group 'gnosis)

(defvar gnosis-dashboard-buffer-name "*Gnosis Dashboard*"
  "Gnosis dashboard buffer name.")

(defvar-local gnosis-center-content t
  "Non-nil means center content in the current Gnosis buffer.")

(defvar-local gnosis-dashboard--projection nil
  "Current validated server dashboard projection.")

(defconst gnosis-themata--catalog-max-items 50000
  "Maximum themata materialized by the management dashboard.")

(defconst gnosis-themata--cache-version 2)
(defconst gnosis-themata--cache-maximum-bytes (* 128 1024 1024))
(defconst gnosis-themata--cache-envelope-reserve (* 1024 1024))
(defconst gnosis-themata--progressive-threshold 1000)
(defconst gnosis-themata--render-chunk-size 500)

(defvar-local gnosis-themata--generation nil
  "Generation of the visible thema catalog.")

(defvar-local gnosis-themata--records nil
  "Catalog records keyed by opaque thema identity.")

(defvar-local gnosis-themata--catalog-records nil)
(defvar-local gnosis-themata--search-filter nil)
(defvar-local gnosis-themata--review-filter :all)
(defvar-local gnosis-themata--tag-filter nil)
(defvar-local gnosis-themata--render-epoch 0)
(defvar-local gnosis-themata--render-timer nil)
(defvar-local gnosis-themata--refresh-timer nil)
(defvar-local gnosis-themata--authority nil)
(defvar-local gnosis-themata--refresh-run nil)
(defvar-local gnosis-themata--refresh-state nil)
(defvar-local gnosis-themata--incarnation nil)
(defvar gnosis-themata--sequence 0)

(defun gnosis-themata--current-authority ()
  "Return non-secret identity of the current authenticated authority."
  (gnosis-core--authenticated-authority))

(defun gnosis-themata--ensure-authority ()
  "Reject use when the live buffer authority changed."
  (when (and gnosis-themata--authority
             (not (equal gnosis-themata--authority
                         (gnosis-themata--current-authority))))
    (signal 'gnosis-core-authorization-error
            '("Gnosis authenticated authority changed"))))

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
  "a" ("Add thema" gnosis-themata-new)
  :group "Mutate"
  "s" ("Suspend or unsuspend" gnosis-themata-toggle-suspension)
  "A" ("Archive or unarchive" gnosis-themata-toggle-archive)
  "S" ("Suspend visible tag set" gnosis-themata-suspend-tag)
  :group "Filter"
  "t" ("Show tag" gnosis-themata-find-tag)
  "/" ("Search cached themata" gnosis-themata-filter-search)
  "f" ("Review state" gnosis-themata-filter-review-state)
  :group "Navigate"
  "g" ("Reconcile cache" gnosis-themata-refresh :stay-open t)
  "q" ("Dashboard" gnosis-dashboard))

(define-key gnosis-dashboard-themata-mode-map
            [remap tabulated-list-sort] #'gnosis-themata-sort)
(define-key gnosis-dashboard-themata-mode-map
            [remap tabulated-list-col-sort] #'gnosis-themata-col-sort)

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
  (setq gnosis-themata--incarnation (cl-incf gnosis-themata--sequence))
  (add-hook 'kill-buffer-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'change-major-mode-hook #'gnosis-themata--cancel-timers nil t)
  (tabulated-list-init-header))

(defun gnosis-themata--cache-items-p (items)
  "Return non-nil when ITEMS form one unique complete catalog projection."
  (and (vectorp items)
       (<= (length items) gnosis-themata--catalog-max-items)
       (let ((seen (make-hash-table :test #'equal)))
         (cl-loop
          for item across items
          for thema-id = (plist-get item :thema_id)
          always (and (gnosis-core--catalog-thema-p item)
                      (not (prog1 (gethash thema-id seen)
                             (puthash thema-id t seen))))))))

(defun gnosis-themata--cache-payload-p (payload &optional authority)
  "Return non-nil when PAYLOAD is a valid cache for AUTHORITY.
When AUTHORITY is nil, resolve the current authenticated authority."
  (let* ((generation (plist-get payload :generation))
         (cursor (gnosis-core--decode-cursor generation))
         (keys (cl-loop for (key _) on payload by #'cddr collect key)))
    (and (equal keys '(:version :authority :server_id :generation :items))
         (equal gnosis-themata--cache-version (plist-get payload :version))
         (equal (or authority (gnosis-themata--current-authority))
                (plist-get payload :authority))
         cursor
         (equal (plist-get cursor :server_id) (plist-get payload :server_id))
         (gnosis-themata--cache-items-p (plist-get payload :items)))))

(defun gnosis-themata--cache-directory ()
  "Return the expanded private thema cache directory."
  (directory-file-name
   (file-name-directory (expand-file-name gnosis-themata-cache-file))))

(defun gnosis-themata--json-bytes (value)
  "Return encoded JSON byte count for VALUE."
  (string-bytes
   (encode-coding-string
    (json-serialize value
                    :null-object gnosis-core--json-null
                    :false-object gnosis-core--json-false)
    'utf-8-unix)))

(defun gnosis-themata--records-byte-size (items)
  "Return bounded aggregate JSON byte estimate for vector ITEMS."
  (let ((total gnosis-themata--cache-envelope-reserve))
    (cl-loop for item across items
             do (cl-incf total (1+ (gnosis-themata--json-bytes item)))
             when (> total gnosis-themata--cache-maximum-bytes)
             return total
             finally return total)))

(defun gnosis-themata--secure-cache-directory ()
  "Create and secure the thema cache directory, rejecting symlinks."
  (let ((directory (gnosis-themata--cache-directory)))
    (when (file-symlink-p directory)
      (signal 'gnosis-core-validation-error '("Unsafe Gnosis thema cache")))
    (make-directory directory t)
    (let ((attributes (file-attributes directory 'integer)))
      (unless (and attributes
                   (equal (file-attribute-user-id attributes) (user-uid)))
        (signal 'gnosis-core-validation-error '("Unsafe Gnosis thema cache"))))
    (set-file-modes directory #o700)
    directory))

(defun gnosis-themata--cache-write (catalog)
  "Atomically persist validated complete CATALOG and return CATALOG."
  (let* ((generation (plist-get catalog :generation))
         (cursor (gnosis-core--decode-cursor generation))
         (items (plist-get catalog :items)))
    (unless (and cursor (gnosis-themata--cache-items-p items))
      (signal 'gnosis-core-protocol-error '("Invalid Gnosis thema cache")))
    (when (> (gnosis-themata--records-byte-size items)
             gnosis-themata--cache-maximum-bytes)
      (signal 'gnosis-core-validation-error '("Gnosis thema cache is too large")))
    (let* ((directory (gnosis-themata--secure-cache-directory))
           (path (expand-file-name gnosis-themata-cache-file))
           (payload `(:version ,gnosis-themata--cache-version
                      :authority ,(gnosis-themata--current-authority)
                      :server_id ,(plist-get cursor :server_id)
                      :generation ,generation :items ,items))
           (bytes (encode-coding-string
                   (json-serialize payload
                                   :null-object gnosis-core--json-null
                                   :false-object gnosis-core--json-false)
                   'utf-8-unix))
           temporary)
      (when (or (file-symlink-p path)
                (> (string-bytes bytes) gnosis-themata--cache-maximum-bytes))
        (signal 'gnosis-core-validation-error '("Unsafe Gnosis thema cache")))
      (unwind-protect
          (progn
            (setq temporary (make-temp-file (expand-file-name ".thema-" directory)))
            (let ((coding-system-for-write 'no-conversion)
                  (write-region-inhibit-fsync nil))
              (write-region bytes nil temporary nil 'silent))
            (set-file-modes temporary #o600)
            (rename-file temporary path t)
            (set-file-modes path #o600)
            (setq temporary nil)
            catalog)
        (when (and temporary (file-exists-p temporary))
          (delete-file temporary))))))

(defun gnosis-themata--cache-read (&optional authority)
  "Return the validated cached catalog for AUTHORITY, or nil.
When AUTHORITY is nil, resolve the current authenticated authority."
  (condition-case nil
      (let ((path (expand-file-name gnosis-themata-cache-file)))
        (when (and (file-regular-p path)
                   (not (file-symlink-p path))
                   (= #o600 (logand #o777 (file-modes path))))
          (with-temp-buffer
            (let ((coding-system-for-read 'no-conversion))
              (insert-file-contents
               path nil 0 (1+ gnosis-themata--cache-maximum-bytes)))
            (when (> (buffer-size) gnosis-themata--cache-maximum-bytes)
              (signal 'gnosis-core-validation-error
                      '("Gnosis thema cache is too large")))
            (decode-coding-region (point-min) (point-max) 'utf-8-unix)
            (let ((payload
                   (json-parse-buffer
                    :object-type 'plist :array-type 'array
                    :null-object gnosis-core--json-null
                    :false-object gnosis-core--json-false)))
              (when (gnosis-themata--cache-payload-p payload authority)
                (list :generation (plist-get payload :generation)
                      :items (plist-get payload :items)))))))
    (error nil)))

(defun gnosis-themata--content-record (projection previous)
  "Return compact catalog record from PROJECTION and PREVIOUS review state."
  (let ((record (or (copy-tree previous)
                    `(:thema_id ,(plist-get projection :thema_id)
                      :state_revision 0 :suspended ,gnosis-core--json-false))))
    (dolist (field '(:kind :keimenon :hypothesis :answer :tags :archived))
      (setq record (plist-put record field (plist-get projection field))))
    (plist-put record :thema_revision (plist-get projection :revision))))

(defun gnosis-themata--review-record (projection previous)
  "Return catalog PREVIOUS updated from review-state PROJECTION."
  (let* ((state (plist-get projection :state))
         (record (copy-tree previous)))
    (dolist (field '(:state_revision :suspended :review_count))
      (when (plist-member state field)
        (setq record (plist-put record field (plist-get state field)))))
    record))

(defun gnosis-themata--change-record (previous change)
  "Return PREVIOUS updated by one exact replica CHANGE projection."
  (let* ((kind (plist-get change :entity_kind))
         (projection (plist-get change :projection))
         (thema-id (plist-get projection :thema_id)))
    (pcase kind
      ("thema"
       (and thema-id (gnosis-themata--content-record projection previous)))
      ("review_state"
       (when previous
         (gnosis-themata--review-record projection previous))))))

(defun gnosis-themata--reconcile-page (catalog)
  "Apply one authenticated replica page after CATALOG.
Return a plist containing updated `:catalog' and boolean `:done'."
  (let* ((generation (plist-get catalog :generation))
         (from (gnosis-core--decode-cursor generation))
         (page (gnosis-core-sync-changes generation))
         (next-generation (plist-get page :next))
         (next (gnosis-core--decode-cursor next-generation))
         (items (plist-get page :items))
         (has-more (eq t (plist-get page :has_more)))
         (records (or (plist-get catalog :record_table)
                      (gnosis-themata--record-table (plist-get catalog :items))))
         (bytes (or (plist-get catalog :bytes)
                    (gnosis-themata--records-byte-size
                     (plist-get catalog :items))))
         (expected-position (and from (plist-get from :position)))
         (last-cursor generation)
         (cursor-chain-p
          (and from
               (cl-loop
                for change across items
                for change-cursor = (plist-get change :cursor)
                for decoded = (gnosis-core--decode-cursor change-cursor)
                always
                (and decoded
                     (equal (plist-get from :server_id)
                            (plist-get decoded :server_id))
                     (= (plist-get decoded :position) (1+ expected-position))
                     (progn
                       (setq expected-position (plist-get decoded :position)
                             last-cursor change-cursor)
                       t))))))
    (when (and has-more (= 0 (length items)))
      (signal 'gnosis-core-protocol-error
              '("Gnosis replica continuation did not advance")))
    (unless (and from next cursor-chain-p
                 (equal generation (plist-get page :from))
                 (equal (plist-get from :server_id) (plist-get next :server_id))
                 (= expected-position (plist-get next :position))
                 (equal last-cursor next-generation)
                 (or (not has-more) (> (length items) 0)))
      (signal 'gnosis-core-conflict '("Invalid Gnosis replica continuation")))
    (let ((overlay (make-hash-table :test #'equal))
          (missing (make-symbol "missing"))
          (count (hash-table-count records)))
      (cl-loop
       for change across items
       for thema-id = (plist-get (plist-get change :projection) :thema_id)
       for overlaid = (and thema-id (gethash thema-id overlay missing))
       for previous = (and thema-id
                           (if (eq overlaid missing)
                               (gethash thema-id records)
                             overlaid))
       for updated = (gnosis-themata--change-record previous change)
       when updated
       do (let* ((previous-bytes
                  (if previous (1+ (gnosis-themata--json-bytes previous)) 0))
                 (updated-bytes (1+ (gnosis-themata--json-bytes updated)))
                 (prospective-bytes (+ bytes (- updated-bytes previous-bytes)))
                 (prospective-count (+ count (if previous 0 1))))
            (when (> prospective-count gnosis-themata--catalog-max-items)
              (signal 'gnosis-core-protocol-error
                      '("Gnosis thema catalog exceeds item limit")))
            (when (> prospective-bytes gnosis-themata--cache-maximum-bytes)
              (signal 'gnosis-core-protocol-error
                      '("Gnosis thema catalog exceeds cache byte budget")))
            (setq bytes prospective-bytes
                  count prospective-count)
            (puthash thema-id updated overlay)))
      (maphash (lambda (thema-id record)
                 (puthash thema-id record records))
               overlay))
    (let* ((done (not (eq t (plist-get page :has_more))))
           (updated
            (if done
                (list :generation next-generation
                      :items (vconcat (hash-table-values records)))
              (list :generation next-generation
                    :record_table records :bytes bytes))))
      (list :catalog updated :done done))))

(defun gnosis-themata--cache-try-write (catalog)
  "Best-effort persist CATALOG without making cache I/O authoritative."
  (condition-case error
      (gnosis-themata--cache-write catalog)
    ((file-error gnosis-core-validation-error)
     (message "Gnosis thema cache persistence disabled: %s"
              (error-message-string error))
     catalog)))

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
          gnosis-themata--catalog-records items
          gnosis-themata--records (gnosis-themata--record-table items))
    (gnosis-themata--render selected-id)))

(defun gnosis-themata--matches-search-p (record)
  "Return non-nil when RECORD matches the current text filter."
  (or (null gnosis-themata--search-filter)
      (let ((case-fold-search t)
            (query (regexp-quote gnosis-themata--search-filter)))
        (seq-some
         (lambda (value) (and (stringp value) (string-match-p query value)))
         (append
          (list (plist-get record :keimenon))
          (append (plist-get record :hypothesis) nil)
          (append (plist-get record :answer) nil)
          (append (plist-get record :tags) nil))))))

(defun gnosis-themata--matches-review-filter-p (record)
  "Return non-nil when RECORD matches the current review filter."
  (pcase gnosis-themata--review-filter
    (:active (and (not (eq t (plist-get record :archived)))
                  (not (eq t (plist-get record :suspended)))))
    (:archived (eq t (plist-get record :archived)))
    (:suspended (eq t (plist-get record :suspended)))
    (_ t)))

(defun gnosis-themata--matches-tag-filter-p (record)
  "Return non-nil when RECORD matches the current exact tag filter."
  (or (null gnosis-themata--tag-filter)
      (member gnosis-themata--tag-filter
              (append (plist-get record :tags) nil))))

(defun gnosis-themata--cancel-render ()
  "Cancel any pending progressive render in the current buffer."
  (when (timerp gnosis-themata--render-timer)
    (cancel-timer gnosis-themata--render-timer))
  (setq gnosis-themata--render-timer nil
        gnosis-themata--render-epoch (cl-incf gnosis-themata--sequence)))

(defun gnosis-themata--cancel-timers ()
  "Cancel pending thema rendering and reconciliation timers."
  (gnosis-themata--cancel-render)
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (setq gnosis-themata--refresh-timer nil
        gnosis-themata--refresh-run (cl-incf gnosis-themata--sequence)
        gnosis-themata--refresh-state nil))

(defun gnosis-themata--sort-value (record column)
  "Return RECORD's lightweight sort value for COLUMN."
  (pcase column
    ("Question" (or (plist-get record :keimenon) ""))
    ("Hypothesis" (gnosis-themata--display-list (plist-get record :hypothesis)))
    ("Answer" (gnosis-themata--display-list (plist-get record :answer)))
    ("Tags" (gnosis-themata--display-list (plist-get record :tags)))
    ("Kind" (or (plist-get record :kind) ""))
    ("Archived" (gnosis-themata--yes-no (plist-get record :archived)))
    ("Suspended" (gnosis-themata--yes-no (plist-get record :suspended)))
    (_ (or (plist-get record :thema_id) ""))))

(defun gnosis-themata--sorted-records ()
  "Return authoritative records in the current requested display order."
  (let ((records (append gnosis-themata--catalog-records nil)))
    (if-let* ((column (car-safe tabulated-list-sort-key)))
        (let ((descending (cdr tabulated-list-sort-key)))
          (sort records
                (lambda (left right)
                  (let ((left-value (gnosis-themata--sort-value left column))
                        (right-value (gnosis-themata--sort-value right column)))
                    (if descending
                        (string-lessp right-value left-value)
                      (string-lessp left-value right-value))))))
      records)))

(defun gnosis-themata--render-chunk
    (buffer epoch expected-tick records selected-id)
  "Filter one bounded RECORDS chunk into BUFFER for EPOCH.
Proceed only when EXPECTED-TICK is current.  Restore SELECTED-ID when the
render completes."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (= epoch gnosis-themata--render-epoch)
                 (= expected-tick (buffer-chars-modified-tick)))
        (let* ((count (min gnosis-themata--render-chunk-size (length records)))
               (chunk (seq-take records count))
               (remaining (nthcdr count records))
               (entries
                (cl-loop for record in chunk
                         when (and (gnosis-themata--matches-search-p record)
                                   (gnosis-themata--matches-review-filter-p record)
                                   (gnosis-themata--matches-tag-filter-p record))
                         collect (gnosis-themata--entry record)))
               (inhibit-read-only t))
          (if tabulated-list-entries
              (nconc tabulated-list-entries entries)
            (setq tabulated-list-entries entries))
          (goto-char (point-max))
          (dolist (entry entries)
            (tabulated-list-print-entry (car entry) (cadr entry)))
          (if remaining
              (setq gnosis-themata--render-timer
                    (run-at-time 0 nil #'gnosis-themata--render-chunk
                                 buffer epoch (buffer-chars-modified-tick)
                                 remaining selected-id))
            (setq gnosis-themata--render-timer nil)
            (when selected-id (gnosis-themata--goto-id selected-id))))))))

(defun gnosis-themata--render (&optional selected-id)
  "Render filtered authoritative records and restore SELECTED-ID."
  (gnosis-themata--cancel-render)
  (let ((records (gnosis-themata--sorted-records)))
    (if (<= (length records) gnosis-themata--progressive-threshold)
        (progn
          (setq tabulated-list-entries
                (cl-loop for record in records
                         when (and (gnosis-themata--matches-search-p record)
                                   (gnosis-themata--matches-review-filter-p record)
                                   (gnosis-themata--matches-tag-filter-p record))
                         collect (gnosis-themata--entry record)))
          (tabulated-list-print t)
          (when selected-id (gnosis-themata--goto-id selected-id)))
      (let ((inhibit-read-only t)
            (epoch gnosis-themata--render-epoch))
        (setq tabulated-list-entries nil)
        (erase-buffer)
        (gnosis-themata--render-chunk
         (current-buffer) epoch (buffer-chars-modified-tick)
         records selected-id)))))

(defun gnosis-themata--sort-by-column-name (name)
  "Set or flip the complete projection sort for column NAME."
  (unless (nth 2 (assoc name (append tabulated-list-format nil)))
    (user-error "Cannot sort by %s" name))
  (if (equal name (car tabulated-list-sort-key))
      (setcdr tabulated-list-sort-key (not (cdr tabulated-list-sort-key)))
    (setq tabulated-list-sort-key (cons name nil)))
  (setq tabulated-list--original-order nil)
  (tabulated-list-init-header)
  (gnosis-themata--render (tabulated-list-get-id)))

(defun gnosis-themata-sort (&optional column)
  "Sort by COLUMN and restart the complete progressive projection render.
With numeric prefix -1, restore the authoritative catalog order."
  (interactive "P")
  (when (and column
             (or (>= column (length tabulated-list-format))
                 (< column -1)))
    (user-error "Invalid column number"))
  (if (equal column -1)
      (progn
        (unless tabulated-list-sort-key
          (error "Order is already in original order"))
        (setq tabulated-list-sort-key nil
              tabulated-list--original-order nil)
        (tabulated-list-init-header)
        (gnosis-themata--render (tabulated-list-get-id)))
    (gnosis-themata--sort-by-column-name
     (if column
         (car (aref tabulated-list-format column))
       (get-text-property (point) 'tabulated-list-column-name)))))

(defun gnosis-themata-col-sort (event)
  "Sort the header column selected by mouse EVENT and restart rendering."
  (interactive "e")
  (let* ((position (event-start event))
         (object (posn-object position)))
    (with-current-buffer (window-buffer (posn-window position))
      (gnosis-themata--sort-by-column-name
       (get-text-property (if object (cdr object) (posn-point position))
                          'tabulated-list-column-name
                          (car object))))))

(defun gnosis-themata-filter-search (query)
  "Filter authoritative themata by literal QUERY across visible text fields."
  (interactive "sThema search (empty clears): ")
  (let ((selected (tabulated-list-get-id)))
    (setq gnosis-themata--search-filter (unless (string-empty-p query) query))
    (gnosis-themata--render selected)))

(defun gnosis-themata-filter-review-state (state)
  "Filter authoritative themata by review STATE."
  (interactive
   (list (intern (concat ":" (completing-read
                              "State: " '("all" "active" "suspended" "archived")
                              nil t nil nil "all")))))
  (unless (memq state '(:all :active :suspended :archived))
    (user-error "Unknown Gnosis thema state filter"))
  (let ((selected (tabulated-list-get-id)))
    (setq gnosis-themata--review-filter state)
    (gnosis-themata--render selected)))

(defun gnosis-themata--schedule-refresh-step (buffer run)
  "Schedule one bounded refresh step for BUFFER and RUN."
  (setq gnosis-themata--refresh-timer
        (run-at-time 0 nil #'gnosis-themata--refresh-step buffer run)))

(defun gnosis-themata--finish-refresh (catalog selected-id)
  "Persist and install reconciled CATALOG, restoring SELECTED-ID."
  (gnosis-themata--cache-try-write catalog)
  (setq gnosis-themata--refresh-timer nil
        gnosis-themata--refresh-state nil)
  (gnosis-themata--install catalog selected-id))

(defun gnosis-themata--bootstrap-step ()
  "Fetch one bounded catalog page for the active bootstrap run."
  (let* ((state gnosis-themata--refresh-state)
         (after (plist-get state :after))
         (generation (plist-get state :generation))
         (records (plist-get state :records))
         (seen-ids (plist-get state :seen_ids))
         (seen-cursors (plist-get state :seen_cursors))
         (bytes (or (plist-get state :bytes)
                    gnosis-themata--cache-envelope-reserve))
         (page (gnosis-core-thema-catalog
                nil after gnosis-core--thema-catalog-page-limit))
         (items (plist-get page :items))
         (next (plist-get page :next))
         (page-generation (plist-get page :generation)))
    (if generation
        (unless (equal generation page-generation)
          (signal 'gnosis-core-conflict
                  '("Gnosis catalog changed during bootstrap")))
      (setq generation page-generation))
    (when (> (+ (length records) (length items))
             gnosis-themata--catalog-max-items)
      (signal 'gnosis-core-protocol-error '("Gnosis thema catalog is too large")))
    (cl-loop for record across items
             for thema-id = (plist-get record :thema_id)
             do (when (gethash thema-id seen-ids)
                  (signal 'gnosis-core-protocol-error
                          '("Duplicate Gnosis catalog thema")))
             do (puthash thema-id t seen-ids)
             do (cl-incf bytes (1+ (gnosis-themata--json-bytes record)))
             do (when (> bytes gnosis-themata--cache-maximum-bytes)
                  (signal 'gnosis-core-protocol-error
                          '("Gnosis thema catalog exceeds cache byte budget")))
             do (push record records))
    (if (eq next gnosis-core--json-null)
        (gnosis-themata--finish-refresh
         (list :generation generation :items (vconcat (nreverse records)))
         (plist-get state :selected))
      (when (or (= 0 (length items)) (gethash next seen-cursors))
        (signal 'gnosis-core-protocol-error
                '("Gnosis catalog bootstrap did not advance")))
      (puthash next t seen-cursors)
      (setq gnosis-themata--refresh-state
            (list :kind 'bootstrap :after next :generation generation
                  :records records :seen_ids seen-ids
                  :seen_cursors seen-cursors
                  :bytes bytes
                  :selected (plist-get state :selected))))))

(defun gnosis-themata--reconcile-step ()
  "Apply one authenticated replica page for the active refresh run."
  (let* ((state gnosis-themata--refresh-state)
         (result (gnosis-themata--reconcile-page (plist-get state :catalog)))
         (catalog (plist-get result :catalog)))
    (if (plist-get result :done)
        (gnosis-themata--finish-refresh catalog (plist-get state :selected))
      (setq gnosis-themata--refresh-state
            (list :kind 'reconcile :catalog catalog
                  :selected (plist-get state :selected))))))

(defun gnosis-themata--refresh-step (buffer run)
  "Execute one bounded refresh step for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal run gnosis-themata--refresh-run))
        (setq gnosis-themata--refresh-timer nil)
        (condition-case error
            (progn
              (gnosis-themata--ensure-authority)
              (pcase (plist-get gnosis-themata--refresh-state :kind)
                ('bootstrap (gnosis-themata--bootstrap-step))
                ('reconcile (gnosis-themata--reconcile-step)))
              (when gnosis-themata--refresh-state
                (gnosis-themata--schedule-refresh-step buffer run)))
          (gnosis-core-conflict
           (message "Gnosis thema cursor expired; rebuilding cache")
           (gnosis-themata--start-bootstrap (tabulated-list-get-id)))
          ((gnosis-core-error file-error)
           (setq gnosis-themata--refresh-state nil)
           (message "Gnosis thema refresh failed: %s"
                    (error-message-string error))))))))

(defun gnosis-themata--start-bootstrap (&optional selected-id)
  "Start cancellable complete catalog bootstrap, restoring SELECTED-ID."
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (let ((run (cl-incf gnosis-themata--sequence)))
    (setq gnosis-themata--refresh-run run
          gnosis-themata--refresh-state
          (list :kind 'bootstrap :after nil :generation nil :records nil
                :bytes gnosis-themata--cache-envelope-reserve
                :seen_ids (make-hash-table :test #'equal)
                :seen_cursors (make-hash-table :test #'equal)
                :selected selected-id))
    (gnosis-themata--schedule-refresh-step (current-buffer) run)))

(defun gnosis-themata-refresh ()
  "Start cancellable reconciliation of the complete thema cache."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-themata-mode)
    (user-error "Not in the Gnosis themata dashboard"))
  (gnosis-themata--ensure-authority)
  (if (and gnosis-themata--generation gnosis-themata--catalog-records)
      (progn
        (when (timerp gnosis-themata--refresh-timer)
          (cancel-timer gnosis-themata--refresh-timer))
        (let ((run (cl-incf gnosis-themata--sequence)))
          (setq gnosis-themata--refresh-run run
                gnosis-themata--refresh-state
                (list :kind 'reconcile
                      :catalog (list :generation gnosis-themata--generation
                                     :items gnosis-themata--catalog-records)
                      :selected (tabulated-list-get-id)))
          (gnosis-themata--schedule-refresh-step (current-buffer) run)))
    (gnosis-themata--start-bootstrap)))

;;;###autoload
(defun gnosis-dashboard-themata ()
  "Open the complete cached server-owned thema management projection."
  (interactive)
  (let* ((authority (gnosis-themata--current-authority))
         (cached (gnosis-themata--cache-read authority))
         (buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (gnosis-dashboard-themata-mode)
    (setq gnosis-themata--authority authority)
    (if cached
        (progn
          (gnosis-themata--install cached)
          (gnosis-themata-refresh))
      (let ((inhibit-read-only t))
        (erase-buffer)
        (insert (propertize "Loading complete thema catalog…\n"
                            'face 'gnosis-face-muted)))
      (gnosis-themata--start-bootstrap))
    (keymap-popup gnosis-dashboard-themata-mode-map)))

(defun gnosis-themata--record-at-point ()
  "Return the catalog record at point or signal a user error."
  (gnosis-themata--ensure-authority)
  (let ((thema-id (tabulated-list-get-id)))
    (or (and thema-id (gethash thema-id gnosis-themata--records))
        (user-error "No thema at point"))))

(defun gnosis-themata--replace-record (record)
  "Replace visible catalog RECORD after an authoritative mutation."
  (let ((thema-id (plist-get record :thema_id)))
    (puthash thema-id record gnosis-themata--records)
    (setq gnosis-themata--catalog-records
          (vconcat
           (mapcar (lambda (item)
                     (if (equal thema-id (plist-get item :thema_id)) record item))
                   (append gnosis-themata--catalog-records nil))))
    (gnosis-themata--render thema-id)
    (when (and gnosis-themata--authority
               (gnosis-core--decode-cursor gnosis-themata--generation))
      (gnosis-themata-refresh))))

(defun gnosis-themata--edited (buffer authority incarnation projection)
  "Update BUFFER from PROJECTION when AUTHORITY and INCARNATION still own it."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal authority gnosis-themata--authority)
                 (or (null authority)
                     (equal authority (gnosis-themata--current-authority)))
                 (equal incarnation gnosis-themata--incarnation))
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
        (authority gnosis-themata--authority)
        (incarnation gnosis-themata--incarnation)
        (thema-id (plist-get (gnosis-themata--record-at-point) :thema_id)))
    (gnosis-edit-thema
     thema-id (lambda (projection)
                (gnosis-themata--edited
                 buffer authority incarnation projection)))))

(defun gnosis-themata-add ()
  "Open the shared editor for a new thema."
  (interactive)
  (gnosis-themata--ensure-authority)
  (let ((buffer (current-buffer))
        (authority gnosis-themata--authority)
        (incarnation gnosis-themata--incarnation))
    (gnosis-add-thema
     (lambda (_projection)
       (when (buffer-live-p buffer)
         (with-current-buffer buffer
           (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                      (equal authority gnosis-themata--authority)
                      (or (null authority)
                          (equal authority (gnosis-themata--current-authority)))
                      (equal incarnation gnosis-themata--incarnation))
             (gnosis-themata-refresh))))))))

(defalias 'gnosis-themata-new #'gnosis-themata-add)

(defun gnosis-themata--call-with-authority (function)
  "Call FUNCTION with one token pinned to the dashboard authority."
  (unless gnosis-themata--authority
    (signal 'gnosis-core-authorization-error
            '("Gnosis dashboard authority is unavailable")))
  (let* ((token (gnosis-core--token))
         (authority (gnosis-core--authenticated-authority token)))
    (unless (equal authority gnosis-themata--authority)
      (signal 'gnosis-core-authorization-error
              '("Gnosis authenticated authority changed")))
    (let ((gnosis-core--token-override token))
      (funcall function))))

(defun gnosis-themata-toggle-suspension ()
  "Suspend or unsuspend the thema at point."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (thema-id (plist-get record :thema_id))
         (suspended (eq t (plist-get record :suspended)))
         (desired (not suspended))
         (action (if desired "Suspend" "Unsuspend")))
    (when (y-or-n-p (format "%s thema at point? " action))
      (gnosis-themata--call-with-authority
       (lambda ()
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
                    (if desired "Suspended" "Unsuspended") thema-id)))))))

(defun gnosis-themata-toggle-archive ()
  "Archive or unarchive the thema at point after confirmation."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (thema-id (plist-get record :thema_id))
         (desired (not (eq t (plist-get record :archived))))
         (action (if desired "Archive" "Unarchive")))
    (when (y-or-n-p (format "%s thema at point? " action))
      (gnosis-themata--call-with-authority
       (lambda ()
         (let* ((projection
                 (gnosis-core-thema-set-archived
                  thema-id (plist-get record :thema_revision) desired))
                (updated (copy-tree record)))
           (dolist (field '(:kind :keimenon :hypothesis :answer :tags :archived))
             (setq updated (plist-put updated field (plist-get projection field))))
           (setq updated
                 (plist-put updated :thema_revision (plist-get projection :revision)))
           (gnosis-themata--replace-record updated)
           (message "%s thema %s" action thema-id)))))))

(defun gnosis-themata-find-tag (tag)
  "Show the authoritative thema set for exact TAG at point."
  (interactive
   (let* ((tags (append (plist-get (gnosis-themata--record-at-point) :tags) nil)))
     (unless tags (user-error "Thema at point has no tags"))
     (list (completing-read "Tag: " tags nil t))))
  (let ((selected (tabulated-list-get-id)))
    (setq gnosis-themata--tag-filter tag)
    (gnosis-themata--render selected)))

(defun gnosis-themata-suspend-tag ()
  "Suspend each unsuspended thema in the visible exact-tag set.
Stop on the first failed or uncertain mutation; prior accepted mutations remain
visible as authoritative partial progress."
  (interactive)
  (gnosis-themata--ensure-authority)
  (unless gnosis-themata--tag-filter
    (user-error "Select a Gnosis tag filter first"))
  (let ((records
         (sort
          (cl-loop
           for record across gnosis-themata--catalog-records
           when (and (gnosis-themata--matches-search-p record)
                     (gnosis-themata--matches-review-filter-p record)
                     (gnosis-themata--matches-tag-filter-p record)
                     (not (eq t (plist-get record :suspended))))
           collect (copy-tree record))
          (lambda (left right)
            (string-lessp (plist-get left :thema_id)
                          (plist-get right :thema_id))))))
    (if (null records)
        (message "No unsuspended themata in this tag set")
      (when (yes-or-no-p
             (format "Suspend %d thema%s tagged %s? "
                     (length records) (if (= 1 (length records)) "" "ta")
                     gnosis-themata--tag-filter))
        (gnosis-themata--call-with-authority
         (lambda ()
           (dolist (record records)
             (let* ((projection
                     (gnosis-core-thema-set-suspended
                      (plist-get record :thema_id)
                      (plist-get record :state_revision) t))
                    (state (plist-get projection :state))
                    (updated (copy-tree record)))
               (setq updated
                     (plist-put updated :state_revision
                                (plist-get state :state_revision)))
               (setq updated
                     (plist-put updated :suspended (plist-get state :suspended)))
               (gnosis-themata--replace-record updated)))))))))

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
