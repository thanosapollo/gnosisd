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
(require 'gnosis-tl)

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

(defconst gnosis-themata--cache-version 3)
(defconst gnosis-themata--cache-maximum-bytes (* 128 1024 1024))
(defconst gnosis-themata--cache-envelope-reserve (* 1024 1024))
(defconst gnosis-themata--progressive-threshold 1000)
(defconst gnosis-themata--render-chunk-size 500)
(defconst gnosis-themata--materialize-chunk-size 500)
(defvar gnosis-themata--cache-read-chunk-bytes (* 256 1024))
(defvar gnosis-themata--preloaded-cache nil)
(defvar gnosis-themata--preload-state nil)
(defvar gnosis-themata--preload-run 0)
(defvar gnosis-themata--preload-timer nil)

(defvar-local gnosis-themata--generation nil
  "Generation of the visible thema catalog.")

(defvar-local gnosis-themata--records nil
  "Catalog records keyed by opaque thema identity.")

(defvar-local gnosis-themata--catalog-records nil)
(defvar-local gnosis-themata--catalog-bytes nil)
(defvar-local gnosis-themata--search-filter nil)
(defvar-local gnosis-themata--review-filter :all)
(defvar-local gnosis-themata--tag-filter nil)
(defvar-local gnosis-themata--render-epoch 0)
(defvar-local gnosis-themata--render-timer nil)
(defvar-local gnosis-themata--layout-width nil)
(defvar-local gnosis-themata--layout-timer nil)
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
(define-key gnosis-dashboard-themata-mode-map
            [remap tabulated-list-widen-current-column]
            #'gnosis-themata-widen-current-column)
(define-key gnosis-dashboard-themata-mode-map
            [remap tabulated-list-narrow-current-column]
            #'gnosis-themata-narrow-current-column)

(defun gnosis-themata--column-format (width)
  "Return thema columns proportioned for window WIDTH."
  (let ((available (max 0 (- width 7))))
    `[("Question" ,(max 10 (/ (* available 28) 100)) t)
      ("Hypothesis" ,(max 8 (/ (* available 16) 100)) t)
      ("Answer" ,(max 8 (/ (* available 16) 100)) t)
      ("Tags" ,(max 8 (/ (* available 18) 100)) t)
      ("Kind" ,(max 5 (/ (* available 10) 100)) t)
      ("Archived" ,(max 5 (/ (* available 6) 100)) t)
      ("Suspended" ,(max 5 (/ (* available 6) 100)) t)]))

(defun gnosis-themata--apply-layout (buffer incarnation width)
  "Apply WIDTH to BUFFER when INCARNATION still owns it."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal incarnation gnosis-themata--incarnation))
        (setq gnosis-themata--layout-timer nil
              gnosis-themata--layout-width width
              tabulated-list-format (gnosis-themata--column-format width))
        (tabulated-list-init-header)
        (when (vectorp gnosis-themata--catalog-records)
          (gnosis-themata--render (tabulated-list-get-id)))))))

(defun gnosis-themata--window-size-change (window)
  "Debounce thema column adjustment for resized WINDOW."
  (let ((width (window-width window)))
    (unless (equal width gnosis-themata--layout-width)
      (when (timerp gnosis-themata--layout-timer)
        (cancel-timer gnosis-themata--layout-timer))
      (setq gnosis-themata--layout-timer
            (run-with-idle-timer
             0.1 nil #'gnosis-themata--apply-layout
             (current-buffer) gnosis-themata--incarnation width)))))

(define-derived-mode gnosis-dashboard-themata-mode
  tabulated-list-mode "Gnosis Themata"
  "Major mode for the server-owned thema management projection."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines t
              tabulated-list-padding 2
              tabulated-list-sort-key nil
              gnosis-themata--layout-width (window-width)
              tabulated-list-format
              (gnosis-themata--column-format gnosis-themata--layout-width)
              revert-buffer-function
              (lambda (&rest _) (gnosis-themata-refresh)))
  (setq gnosis-themata--incarnation (cl-incf gnosis-themata--sequence))
  (add-hook 'kill-buffer-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'change-major-mode-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'window-size-change-functions
            #'gnosis-themata--window-size-change nil t)
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

(defun gnosis-themata--cache-header-p (header &optional authority)
  "Return non-nil when HEADER is valid for AUTHORITY.
When AUTHORITY is nil, resolve the current authenticated authority."
  (let* ((generation (plist-get header :generation))
         (cursor (gnosis-core--decode-cursor generation))
         (count (plist-get header :count))
         (keys (cl-loop for (key _) on header by #'cddr collect key)))
    (and (equal keys '(:version :authority :server_id :generation :count
                       :payload_sha256))
         (equal gnosis-themata--cache-version (plist-get header :version))
         (equal (or authority (gnosis-themata--current-authority))
                (plist-get header :authority))
         cursor
         (equal (plist-get cursor :server_id) (plist-get header :server_id))
         (integerp count)
         (<= 0 count gnosis-themata--catalog-max-items)
         (stringp (plist-get header :payload_sha256))
         (string-match-p "\\`[[:xdigit:]]\\{64\\}\\'"
                         (plist-get header :payload_sha256)))))

(defun gnosis-themata--json-line (value)
  "Return VALUE encoded as one UTF-8 JSON cache line."
  (encode-coding-string
   (json-serialize value
                   :null-object gnosis-core--json-null
                   :false-object gnosis-core--json-false)
   'utf-8-unix))

(defun gnosis-themata--parse-json-line (line)
  "Return plist parsed from UTF-8 JSON cache LINE."
  (json-parse-string
   (decode-coding-string line 'utf-8-unix)
   :object-type 'plist :array-type 'array
   :null-object gnosis-core--json-null
   :false-object gnosis-core--json-false))

(defun gnosis-themata--cache-digest-initial ()
  "Return the binary initial state for cache payload integrity."
  (encode-coding-string (make-string 32 0) 'binary))

(defun gnosis-themata--cache-digest-step (digest line)
  "Return integrity chain after exact payload LINE following DIGEST."
  (secure-hash
   'sha256
   (concat digest (secure-hash 'sha256 line nil nil t))
   nil nil t))

(defun gnosis-themata--cache-digest-hex (digest)
  "Return lowercase hexadecimal encoding of binary DIGEST."
  (mapconcat (lambda (byte) (format "%02x" byte)) digest ""))

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

(defun gnosis-themata--cache-write (catalog &optional authority)
  "Persist complete CATALOG for AUTHORITY atomically and return it."
  (let* ((generation (plist-get catalog :generation))
         (cursor (gnosis-core--decode-cursor generation))
         (items (plist-get catalog :items)))
    (unless (and cursor (gnosis-themata--cache-items-p items))
      (signal 'gnosis-core-protocol-error '("Invalid Gnosis thema cache")))
    (when (> (gnosis-themata--records-byte-size items)
             gnosis-themata--cache-maximum-bytes)
      (signal 'gnosis-core-validation-error '("Gnosis thema cache is too large")))
    (let* ((authority (or authority (gnosis-themata--current-authority)))
           (directory (gnosis-themata--secure-cache-directory))
           (path (expand-file-name gnosis-themata-cache-file))
           (bytes
            (with-temp-buffer
              (set-buffer-multibyte nil)
              (let ((digest (gnosis-themata--cache-digest-initial)))
                (cl-loop for item across items
                         for line = (gnosis-themata--json-line item)
                         do (setq digest
                                  (gnosis-themata--cache-digest-step digest line))
                         do (insert line "\n"))
                (goto-char (point-min))
                (insert
                 (gnosis-themata--json-line
                  `(:version ,gnosis-themata--cache-version
                    :authority ,authority
                    :server_id ,(plist-get cursor :server_id)
                    :generation ,generation :count ,(length items)
                    :payload_sha256
                    ,(gnosis-themata--cache-digest-hex digest)))
                 "\n"))
              (buffer-string)))
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
            (unless (and (> (buffer-size) 0)
                         (eq (char-before (point-max)) ?\n))
              (signal 'gnosis-core-validation-error
                      '("Truncated Gnosis thema cache")))
            (goto-char (point-min))
            (let* ((header (gnosis-themata--parse-json-line
                            (buffer-substring-no-properties
                             (line-beginning-position) (line-end-position))))
                   (expected (plist-get header :count))
                   (seen (make-hash-table :test #'equal))
                   (digest (gnosis-themata--cache-digest-initial))
                   records)
              (forward-line 1)
              (while (< (point) (point-max))
                (let* ((line (buffer-substring-no-properties
                              (line-beginning-position) (line-end-position)))
                       (record (gnosis-themata--parse-json-line line))
                       (thema-id (plist-get record :thema_id)))
                  (unless (and (gnosis-core--catalog-thema-p record)
                               (not (gethash thema-id seen))
                               (< (length records) gnosis-themata--catalog-max-items))
                    (signal 'gnosis-core-validation-error
                            '("Invalid Gnosis thema cache")))
                  (puthash thema-id t seen)
                  (setq digest (gnosis-themata--cache-digest-step digest line))
                  (push record records))
                (forward-line 1))
              (let ((items (vconcat (nreverse records))))
                (when (and (gnosis-themata--cache-header-p header authority)
                           (= expected (length items))
                           (equal (plist-get header :payload_sha256)
                                  (gnosis-themata--cache-digest-hex digest))
                           (gnosis-themata--cache-items-p items))
                  (list :generation (plist-get header :generation)
                        :items items)))))))
    (error nil)))

(defun gnosis-themata--finish-cache-preload (run catalog)
  "Finish cache preload RUN with validated CATALOG or nil."
  (when (equal run gnosis-themata--preload-run)
    (let* ((authority (plist-get gnosis-themata--preload-state :authority))
           (live-authority
            (condition-case nil
                (gnosis-themata--current-authority)
              (error nil)))
           (current-p (and live-authority
                           (equal authority live-authority))))
      (setq gnosis-themata--preload-timer nil
            gnosis-themata--preload-state nil
            gnosis-themata--preloaded-cache
            (and current-p catalog
                 (list :authority authority :catalog catalog)))
      (when-let* ((buffer (and current-p
                              (get-buffer gnosis-dashboard-buffer-name))))
        (with-current-buffer buffer
          (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                     (equal authority gnosis-themata--authority))
            (if catalog
                (progn
                  (gnosis-themata--install catalog)
                  (gnosis-themata-refresh))
              (gnosis-themata--start-bootstrap))))))))

(defun gnosis-themata--consume-cache-line (state line)
  "Return preload STATE after consuming one raw JSON LINE."
  (if (null (plist-get state :header))
      (let ((header (gnosis-themata--parse-json-line line)))
        (unless (gnosis-themata--cache-header-p
                 header (plist-get state :authority))
          (signal 'gnosis-core-validation-error '("Invalid Gnosis thema cache")))
        (plist-put state :header header))
    (let* ((record (gnosis-themata--parse-json-line line))
           (thema-id (plist-get record :thema_id))
           (seen (plist-get state :seen))
           (count (1+ (plist-get state :count))))
      (unless (and (<= count gnosis-themata--catalog-max-items)
                   (gnosis-core--catalog-thema-p record)
                   (not (gethash thema-id seen)))
        (signal 'gnosis-core-validation-error '("Invalid Gnosis thema cache")))
      (puthash thema-id t seen)
      (plist-put state :count count)
      (plist-put state :digest
                 (gnosis-themata--cache-digest-step
                  (plist-get state :digest) line))
      (plist-put state :records (cons record (plist-get state :records))))))

(defun gnosis-themata--cache-preload-step (run)
  "Read one bounded cache chunk for preload RUN."
  (when (and (equal run gnosis-themata--preload-run)
             gnosis-themata--preload-state)
    (condition-case nil
        (let* ((state gnosis-themata--preload-state)
               (path (plist-get state :path))
               (offset (plist-get state :offset))
               (size (plist-get state :size))
               (end (min size (+ offset gnosis-themata--cache-read-chunk-bytes)))
               (chunk
                (with-temp-buffer
                  (set-buffer-multibyte nil)
                  (insert-file-contents-literally path nil offset end)
                  (buffer-string)))
               (parts (split-string
                       (concat (plist-get state :remainder) chunk) "\n"))
               (remainder (car (last parts))))
          (dolist (line (butlast parts))
            (when (string-empty-p line)
              (signal 'gnosis-core-validation-error
                      '("Invalid Gnosis thema cache")))
            (setq state (gnosis-themata--consume-cache-line state line)))
          (when (> (string-bytes remainder)
                   gnosis-themata--cache-envelope-reserve)
            (signal 'gnosis-core-validation-error
                    '("Invalid Gnosis thema cache")))
          (setq state (plist-put state :offset end)
                state (plist-put state :remainder remainder)
                gnosis-themata--preload-state state)
          (if (< end size)
              (setq gnosis-themata--preload-timer
                    (run-at-time 0 nil #'gnosis-themata--cache-preload-step run))
            (let* ((header (plist-get state :header))
                   (items (vconcat (nreverse (plist-get state :records))))
                   (attributes (file-attributes path 'integer)))
              (unless (and (string-empty-p remainder)
                           attributes
                           (= size (file-attribute-size attributes))
                           (equal (plist-get state :modified)
                                  (file-attribute-modification-time attributes))
                           (= (plist-get header :count) (length items))
                           (equal (plist-get header :payload_sha256)
                                  (gnosis-themata--cache-digest-hex
                                   (plist-get state :digest)))
                           (gnosis-themata--cache-items-p items))
                (signal 'gnosis-core-validation-error
                        '("Invalid Gnosis thema cache")))
              (gnosis-themata--finish-cache-preload
               run (list :generation (plist-get header :generation)
                         :items items)))))
      (error (gnosis-themata--finish-cache-preload run nil)))))

(defun gnosis-themata--start-cache-preload ()
  "Start bounded background hydration of the disposable thema cache."
  (when (timerp gnosis-themata--preload-timer)
    (cancel-timer gnosis-themata--preload-timer))
  (let ((run (cl-incf gnosis-themata--preload-run)))
    (setq gnosis-themata--preload-timer nil
          gnosis-themata--preload-state nil
          gnosis-themata--preloaded-cache nil)
    (condition-case nil
        (let* ((authority (gnosis-themata--current-authority))
             (path (expand-file-name gnosis-themata-cache-file))
             (attributes (and (file-regular-p path)
                              (not (file-symlink-p path))
                              (file-attributes path 'integer)))
             (size (and attributes (file-attribute-size attributes))))
        (unless (and attributes size
                     (<= size gnosis-themata--cache-maximum-bytes)
                     (= #o600 (logand #o777 (file-modes path)))
                     (equal (file-attribute-user-id attributes) (user-uid)))
          (signal 'gnosis-core-validation-error '("Unsafe Gnosis thema cache")))
          (setq gnosis-themata--preload-state
                (list :authority authority :path path :size size :offset 0
                      :modified (file-attribute-modification-time attributes)
                      :remainder (encode-coding-string "" 'binary) :header nil
                      :records nil :count 0
                      :digest (gnosis-themata--cache-digest-initial)
                      :seen (make-hash-table :test #'equal))
                gnosis-themata--preload-timer
                (run-at-time 0 nil #'gnosis-themata--cache-preload-step run)))
      (error nil))))

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

(defun gnosis-themata--reconcile-state (catalog)
  "Return run-owned reconciliation state for CATALOG."
  (if (plist-get catalog :base_items)
      catalog
    (let* ((order (plist-get catalog :item_order))
           (records (or (plist-get catalog :record_table)
                        (gnosis-themata--record-table
                         (plist-get catalog :items))))
           (items (or (plist-get catalog :items)
                      (vconcat
                       (mapcar (lambda (id) (gethash id records)) order)))))
      (list :generation (plist-get catalog :generation)
            :record_table records :base_items items
            :record_updates (make-hash-table :test #'equal)
            :new_ids nil :new_ids_tail nil
            :count (hash-table-count records)
            :bytes (or (plist-get catalog :bytes)
                       (gnosis-themata--records-byte-size items))))))

(defun gnosis-themata--reconcile-materialize-step (state)
  "Materialize one bounded catalog chunk from reconciliation STATE."
  (let* ((target (plist-get state :target))
         (target-index (plist-get state :target_index))
         (base (plist-get state :base_items))
         (base-index (plist-get state :base_index))
         (new-ids (plist-get state :new_ids))
         (records (plist-get state :record_table))
         (updates (plist-get state :record_updates))
         (missing (make-symbol "missing"))
         (budget gnosis-themata--materialize-chunk-size))
    (while (and (> budget 0) (< target-index (length target)))
      (let ((id (if (< base-index (length base))
                    (prog1 (plist-get (aref base base-index) :thema_id)
                      (cl-incf base-index))
                  (prog1 (car new-ids) (setq new-ids (cdr new-ids))))))
        (aset target target-index
              (let ((updated (gethash id updates missing)))
                (if (eq updated missing) (gethash id records) updated))))
      (cl-incf target-index)
      (cl-decf budget))
    (if (= target-index (length target))
        (list :status 'done
              :catalog (list :generation (plist-get state :generation)
                             :items target)
              :selected (plist-get state :selected))
      (setq state (plist-put state :target_index target-index)
            state (plist-put state :base_index base-index)
            state (plist-put state :new_ids new-ids))
      (list :status 'continue :state state))))

(defun gnosis-themata--reconcile-page (catalog)
  "Apply one authenticated replica page after CATALOG.
Return a plist containing updated `:catalog' and boolean `:done'."
  (let* ((state (gnosis-themata--reconcile-state catalog))
         (generation (plist-get state :generation))
         (from (gnosis-core--decode-cursor generation))
         (page (gnosis-core-sync-changes generation))
         (next-generation (plist-get page :next))
         (next (gnosis-core--decode-cursor next-generation))
         (items (plist-get page :items))
         (has-more (eq t (plist-get page :has_more)))
         (records (plist-get state :record_table))
         (updates (plist-get state :record_updates))
         (bytes (plist-get state :bytes))
         (count (plist-get state :count))
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
          page-new-ids)
      (cl-loop
       for change across items
       for thema-id = (plist-get (plist-get change :projection) :thema_id)
       for overlaid = (and thema-id (gethash thema-id overlay missing))
       for previous = (and thema-id
                           (if (eq overlaid missing)
                               (let ((updated (gethash thema-id updates missing)))
                                 (if (eq updated missing)
                                     (gethash thema-id records)
                                   updated))
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
            (when (null previous)
              (push thema-id page-new-ids))
            (puthash thema-id updated overlay)))
      (maphash (lambda (thema-id record)
                 (puthash thema-id record updates))
               overlay)
      (let ((new-ids (plist-get state :new_ids))
            (new-tail (plist-get state :new_ids_tail))
            (page-ids (nreverse page-new-ids)))
        (when page-ids
          (if new-tail
              (setcdr new-tail page-ids)
            (setq new-ids page-ids))
          (setq new-tail (last page-ids)))
        (if (eq t (plist-get page :has_more))
            (list :done nil
                  :catalog
                  (list :generation next-generation
                        :record_table records
                        :record_updates updates
                        :base_items (plist-get state :base_items)
                        :new_ids new-ids :new_ids_tail new-tail
                        :count count :bytes bytes))
          (let ((result
                 (gnosis-themata--reconcile-materialize-step
                  (list :kind 'reconcile-materialize
                        :generation next-generation
                        :record_table records
                        :record_updates updates
                        :base_items (plist-get state :base_items)
                        :base_index 0 :new_ids new-ids
                        :target (make-vector count nil) :target_index 0))))
            (if (eq 'done (plist-get result :status))
                (list :done t :catalog (plist-get result :catalog))
              (list :done nil :catalog (plist-get result :state)))))))))

(defun gnosis-themata--cache-try-write (catalog &optional authority)
  "Best-effort persist CATALOG for pinned AUTHORITY."
  (condition-case error
      (gnosis-themata--cache-write catalog authority)
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
    (forward-line 1))
  (if (eobp)
      (progn (goto-char (point-min)) nil)
    t))

(defun gnosis-themata--install (catalog &optional selected-id)
  "Install CATALOG in the current buffer and restore SELECTED-ID."
  (let ((items (plist-get catalog :items)))
    (setq gnosis-themata--generation (plist-get catalog :generation)
          gnosis-themata--catalog-records items
          gnosis-themata--catalog-bytes
          (gnosis-themata--records-byte-size items)
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

(defun gnosis-themata--visible-p (record)
  "Return non-nil when RECORD belongs in the current projection."
  (and (gnosis-themata--matches-search-p record)
       (gnosis-themata--matches-review-filter-p record)
       (gnosis-themata--matches-tag-filter-p record)))

(defun gnosis-themata--cancel-render ()
  "Cancel any pending progressive render in the current buffer."
  (when (timerp gnosis-themata--render-timer)
    (cancel-timer gnosis-themata--render-timer))
  (setq gnosis-themata--render-timer nil
        gnosis-themata--render-epoch (cl-incf gnosis-themata--sequence)))

(defun gnosis-themata--cancel-timers ()
  "Cancel pending thema rendering and reconciliation timers."
  (gnosis-themata--cancel-render)
  (when (timerp gnosis-themata--layout-timer)
    (cancel-timer gnosis-themata--layout-timer))
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (setq gnosis-themata--layout-timer nil
        gnosis-themata--refresh-timer nil
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
      (cond
       ((or (not (derived-mode-p 'gnosis-dashboard-themata-mode))
            (/= epoch gnosis-themata--render-epoch)))
       ((/= expected-tick (buffer-chars-modified-tick))
        (setq gnosis-themata--render-timer nil)
        (gnosis-themata--render selected-id))
       (t
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
          (gnosis-tl-append-entries entries)
          (if remaining
              (setq gnosis-themata--render-timer
                    (run-at-time 0 nil #'gnosis-themata--render-chunk
                                 buffer epoch (buffer-chars-modified-tick)
                                 remaining selected-id))
            (setq gnosis-themata--render-timer nil)
            (if selected-id
                (gnosis-themata--goto-id selected-id)
              (goto-char (point-min))))))))))

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
          (gnosis-tl-print t)
          (if selected-id
              (gnosis-themata--goto-id selected-id)
            (goto-char (point-min))))
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

(defun gnosis-themata-widen-current-column (&optional n)
  "Widen the current column by N and restart the complete render."
  (interactive "p")
  (let ((selected-id (tabulated-list-get-id)))
    (tabulated-list-widen-current-column n)
    (gnosis-themata--render selected-id)))

(defun gnosis-themata-narrow-current-column (&optional n)
  "Narrow the current column by N and restart the complete render."
  (interactive "p")
  (let ((selected-id (tabulated-list-get-id)))
    (tabulated-list-narrow-current-column n)
    (gnosis-themata--render selected-id)))

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
       (car (aref tabulated-list-format
                  (gnosis-tl--column-at-point)))))))

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

(defun gnosis-themata--bootstrap-materialize-step (state)
  "Materialize one bounded catalog chunk from bootstrap STATE."
  (let ((target (plist-get state :target))
        (index (plist-get state :target_index))
        (records (plist-get state :records))
        (budget gnosis-themata--materialize-chunk-size))
    (while (and records (> budget 0))
      (aset target index (pop records))
      (cl-incf index)
      (cl-decf budget))
    (if records
        (progn
          (setq state (plist-put state :target_index index)
                state (plist-put state :records records))
          (list :status 'continue :state state))
      (list :status 'done
            :catalog (list :generation (plist-get state :generation)
                           :items target)
            :selected (plist-get state :selected)))))

(defun gnosis-themata--bootstrap-page-result (state page)
  "Validate and retain one bounded bootstrap PAGE in run-owned STATE."
  (let* ((generation (plist-get state :generation))
         (records (plist-get state :records))
         (record-tail (plist-get state :record_tail))
         (seen-ids (plist-get state :seen_ids))
         (seen-cursors (plist-get state :seen_cursors))
         (count (or (plist-get state :count) (length records)))
         (bytes (or (plist-get state :bytes)
                    gnosis-themata--cache-envelope-reserve))
         (items (plist-get page :items))
         (page-records (append items nil))
         (page-ids (make-hash-table :test #'equal))
         (next (plist-get page :next))
         (page-generation (plist-get page :generation)))
    (if generation
        (unless (equal generation page-generation)
          (signal 'gnosis-core-conflict
                  '("Gnosis catalog changed during bootstrap")))
      (setq generation page-generation))
    (when (> (+ count (length items))
             gnosis-themata--catalog-max-items)
      (signal 'gnosis-core-protocol-error '("Gnosis thema catalog is too large")))
    (cl-loop for record in page-records
             for thema-id = (plist-get record :thema_id)
             for record-bytes = (1+ (gnosis-themata--json-bytes record))
             do (when (or (gethash thema-id seen-ids)
                          (gethash thema-id page-ids)
                          (> (+ bytes record-bytes)
                             gnosis-themata--cache-maximum-bytes))
                  (signal 'gnosis-core-protocol-error
                          '("Invalid or oversized Gnosis catalog page")))
             do (puthash thema-id t page-ids)
             do (cl-incf bytes record-bytes)
             finally return nil)
    (unless (eq next gnosis-core--json-null)
      (when (or (= 0 (length items)) (gethash next seen-cursors))
        (signal 'gnosis-core-protocol-error
                '("Gnosis catalog bootstrap did not advance"))))
    (maphash (lambda (id _) (puthash id t seen-ids)) page-ids)
    (unless (eq next gnosis-core--json-null) (puthash next t seen-cursors))
    (when page-records
      (if record-tail
          (setcdr record-tail page-records)
        (setq records page-records))
      (setq record-tail (last page-records)))
    (cl-incf count (length items))
    (if (eq next gnosis-core--json-null)
        (gnosis-themata--bootstrap-materialize-step
         (list :kind 'bootstrap-materialize :generation generation
               :records records :target (make-vector count nil)
               :target_index 0 :selected (plist-get state :selected)))
      (list :status 'continue
            :state
            (list :kind 'bootstrap :after next :generation generation
                  :records records :record_tail record-tail :count count
                  :seen_ids seen-ids :seen_cursors seen-cursors :bytes bytes
                  :selected (plist-get state :selected))))))

(defun gnosis-themata--bootstrap-step (&optional state)
  "Fetch one bootstrap page from STATE without publishing it."
  (let* ((state (or state gnosis-themata--refresh-state))
         (page (gnosis-core-thema-catalog
                nil (plist-get state :after)
                gnosis-core--thema-catalog-page-limit)))
    (gnosis-themata--bootstrap-page-result state page)))

(defun gnosis-themata--refresh-fetch (state)
  "Fetch and materialize one private result from refresh STATE."
  (pcase (plist-get state :kind)
    ('bootstrap
     (gnosis-themata--bootstrap-step state))
    ('bootstrap-materialize
     (gnosis-themata--bootstrap-materialize-step state))
    ('reconcile-materialize
     (gnosis-themata--reconcile-materialize-step state))
    ('reconcile
     (let* ((page-result
             (gnosis-themata--reconcile-page (plist-get state :catalog)))
            (catalog (plist-get page-result :catalog)))
       (if (plist-get page-result :done)
           (list :status 'done :catalog catalog
                 :selected (plist-get state :selected))
         (if (eq 'reconcile-materialize (plist-get catalog :kind))
             (list :status 'continue
                   :state (plist-put catalog :selected
                                     (plist-get state :selected)))
           (list :status 'continue
                 :state (list :kind 'reconcile :catalog catalog
                              :selected (plist-get state :selected)))))))))

(defun gnosis-themata--refresh-context-p (buffer run incarnation authority)
  "Return non-nil when BUFFER owns RUN, INCARNATION, and AUTHORITY."
  (and (buffer-live-p buffer)
       (with-current-buffer buffer
         (and (derived-mode-p 'gnosis-dashboard-themata-mode)
              (equal run gnosis-themata--refresh-run)
              (equal incarnation gnosis-themata--incarnation)
              (equal authority gnosis-themata--authority)
              (condition-case nil
                  (equal authority (gnosis-themata--current-authority))
                (error nil))))))

(defun gnosis-themata--publish-refresh-result
    (buffer run incarnation authority result)
  "Publish RESULT while BUFFER owns RUN, INCARNATION, and AUTHORITY."
  (when (gnosis-themata--refresh-context-p
         buffer run incarnation authority)
    (with-current-buffer buffer
      (pcase (plist-get result :status)
        ('continue
         (setq gnosis-themata--refresh-state (plist-get result :state))
         (gnosis-themata--schedule-refresh-step buffer run))
        ('done
         (let ((catalog (plist-get result :catalog))
               (selected (plist-get result :selected)))
           (gnosis-themata--cache-try-write catalog authority)
           (when (gnosis-themata--refresh-context-p
                  buffer run incarnation authority)
             (setq gnosis-themata--refresh-timer nil
                   gnosis-themata--refresh-state nil)
             (gnosis-themata--install catalog selected))))))))

(defun gnosis-themata--refresh-step (buffer run)
  "Execute one bounded two-phase refresh step for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal run gnosis-themata--refresh-run))
        (let ((incarnation gnosis-themata--incarnation)
              (opening-authority gnosis-themata--authority)
              (state gnosis-themata--refresh-state))
          (setq gnosis-themata--refresh-timer nil)
          (condition-case error
              (let* ((token (gnosis-core--token))
                     (base-url (gnosis-core--validated-base-url))
                     (request-authority
                      (vector base-url (secure-hash 'sha256 token))))
                (unless (equal opening-authority request-authority)
                  (signal 'gnosis-core-authorization-error
                          '("Gnosis credentials changed; reopen Themata")))
                (let ((result
                       (let ((gnosis-core--token-override token)
                             (gnosis-core--base-url-override base-url))
                         (gnosis-themata--refresh-fetch state))))
                  (gnosis-themata--publish-refresh-result
                   buffer run incarnation opening-authority result)))
            (gnosis-core-conflict
             (when (gnosis-themata--refresh-context-p
                    buffer run incarnation opening-authority)
               (message "Gnosis thema cursor expired; rebuilding cache")
               (gnosis-themata--start-bootstrap (tabulated-list-get-id))))
            ((gnosis-core-error file-error)
             (when (gnosis-themata--refresh-context-p
                    buffer run incarnation opening-authority)
               (setq gnosis-themata--refresh-state nil)
               (message "Gnosis thema refresh failed: %s"
                        (error-message-string error))))))))))

(defun gnosis-themata--start-bootstrap (&optional selected-id)
  "Start cancellable complete catalog bootstrap, restoring SELECTED-ID."
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (let ((run (cl-incf gnosis-themata--sequence)))
    (setq gnosis-themata--refresh-run run
          gnosis-themata--refresh-state
          (list :kind 'bootstrap :after nil :generation nil :records nil
                :record_tail nil :count 0
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
                                     :items gnosis-themata--catalog-records
                                     :record_table gnosis-themata--records
                                     :bytes gnosis-themata--catalog-bytes)
                      :selected (tabulated-list-get-id)))
          (gnosis-themata--schedule-refresh-step (current-buffer) run)))
    (gnosis-themata--start-bootstrap)))

;;;###autoload
(defun gnosis-dashboard-themata ()
  "Open the complete cached server-owned thema management projection."
  (interactive)
  (let* ((authority (gnosis-themata--current-authority))
         (cached (and (equal authority
                             (plist-get gnosis-themata--preloaded-cache
                                        :authority))
                      (plist-get gnosis-themata--preloaded-cache :catalog)))
         (warming (equal authority
                         (plist-get gnosis-themata--preload-state :authority)))
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
        (insert (propertize (if warming
                                "Warming thema cache…\n"
                              "Loading complete thema catalog…\n")
                            'face 'gnosis-face-muted)))
      (unless warming
        (gnosis-themata--start-bootstrap)))
    (keymap-popup gnosis-dashboard-themata-mode-map)))

(defun gnosis-themata--record-at-point ()
  "Return the catalog record at point or signal a user error."
  (gnosis-themata--ensure-authority)
  (let ((thema-id (tabulated-list-get-id)))
    (or (and thema-id (gethash thema-id gnosis-themata--records))
        (user-error "No thema at point"))))

(defun gnosis-themata--sort-before-p (left right)
  "Return non-nil when LEFT should sort before RIGHT under the active key."
  (let* ((column (car tabulated-list-sort-key))
         (descending (cdr tabulated-list-sort-key))
         (left-value (gnosis-themata--sort-value left column))
         (right-value (gnosis-themata--sort-value right column)))
    (if descending
        (string-lessp right-value left-value)
      (string-lessp left-value right-value))))

(defun gnosis-themata--insert-entry-sorted (entry entries)
  "Return ENTRIES with ENTRY inserted under the active sort key."
  (let* ((record (gethash (car entry) gnosis-themata--records))
         (result nil)
         (inserted nil))
    (dolist (item entries)
      (unless inserted
        (let ((other (gethash (car item) gnosis-themata--records)))
          (when (and record other (gnosis-themata--sort-before-p record other))
            (push entry result)
            (setq inserted t))))
      (push item result))
    (unless inserted
      (push entry result))
    (nreverse result)))

(defun gnosis-themata--next-entry-id (thema-id entries)
  "Return the id following THEMA-ID in ENTRIES, or nil when last."
  (let ((cell (cl-member thema-id entries :key #'car :test #'equal)))
    (car-safe (car-safe (cdr cell)))))

(defun gnosis-themata--replace-record (record)
  "Replace visible catalog RECORD after an authoritative mutation."
  (let* ((thema-id (plist-get record :thema_id))
         (previous (gethash thema-id gnosis-themata--records))
         (old-visible (and previous (gnosis-themata--visible-p previous)))
         (new-visible (gnosis-themata--visible-p record))
         (sort-column (car-safe tabulated-list-sort-key))
         (sort-stable
          (or (null sort-column)
              (and previous
                   (equal (gnosis-themata--sort-value previous sort-column)
                          (gnosis-themata--sort-value record sort-column)))))
         (displayed (assoc thema-id tabulated-list-entries))
         (entry (gnosis-themata--entry record))
         (selected-id (tabulated-list-get-id))
         (selected-column (current-column))
         (start-id (and (get-buffer-window (current-buffer))
                        (get-text-property (window-start) 'tabulated-list-id))))
    (puthash thema-id record gnosis-themata--records)
    (setq gnosis-themata--catalog-records
          (vconcat
           (mapcar (lambda (item)
                     (if (equal thema-id (plist-get item :thema_id)) record item))
                   (append gnosis-themata--catalog-records nil))))
    (cond
     ((and old-visible new-visible sort-stable displayed)
      (setq tabulated-list-entries
            (mapcar (lambda (item)
                      (if (equal thema-id (car item)) entry item))
                    tabulated-list-entries))
      (gnosis-tl-replace-entry thema-id (cadr entry)))
     ((and old-visible new-visible displayed)
      (let* ((without (cl-remove thema-id tabulated-list-entries
                                 :key #'car :test #'equal))
             (entries (gnosis-themata--insert-entry-sorted entry without))
             (before-id (gnosis-themata--next-entry-id thema-id entries)))
        (setq tabulated-list-entries entries)
        (gnosis-tl-delete-entry thema-id)
        (gnosis-tl-insert-entry thema-id (cadr entry) before-id)
        (when (and start-id (gnosis-tl--goto-id start-id)
                   (get-buffer-window (current-buffer)))
          (set-window-start (selected-window) (line-beginning-position)))
        (when (and selected-id (gnosis-tl--goto-id selected-id))
          (move-to-column selected-column))))
     ((and old-visible (not new-visible) displayed)
      (setq tabulated-list-entries
            (cl-delete thema-id tabulated-list-entries
                       :key #'car :test #'equal))
      (gnosis-tl-delete-entry thema-id))
     ((and (not old-visible) new-visible)
      (let* ((entries (gnosis-themata--insert-entry-sorted
                       entry tabulated-list-entries))
             (before-id (gnosis-themata--next-entry-id thema-id entries)))
        (setq tabulated-list-entries entries)
        (gnosis-tl-insert-entry thema-id (cadr entry) before-id)
        (when (and start-id (gnosis-tl--goto-id start-id)
                   (get-buffer-window (current-buffer)))
          (set-window-start (selected-window) (line-beginning-position)))
        (when (and selected-id (gnosis-tl--goto-id selected-id))
          (move-to-column selected-column))))
     ((and (not old-visible) (not new-visible)) nil)
     (t (gnosis-themata--render thema-id)))))

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
        (incarnation gnosis-themata--incarnation)
        (kind (gnosis-edit-read-kind)))
    (gnosis-add-thema
     (lambda (_projection)
       (when (buffer-live-p buffer)
         (with-current-buffer buffer
           (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                      (equal authority gnosis-themata--authority)
                      (or (null authority)
                          (equal authority (gnosis-themata--current-authority)))
                      (equal incarnation gnosis-themata--incarnation))
             (gnosis-themata-refresh)))))
     kind)))

(defalias 'gnosis-themata-new #'gnosis-themata-add)

(defun gnosis-themata--call-with-authority (function)
  "Call FUNCTION with one token pinned to the dashboard authority."
  (unless gnosis-themata--authority
    (signal 'gnosis-core-authorization-error
            '("Gnosis dashboard authority is unavailable")))
  (let* ((base-url (gnosis-core--validated-base-url))
         (token (gnosis-core--token))
         (authority
          (let ((gnosis-core--base-url-override base-url))
            (gnosis-core--authenticated-authority token))))
    (unless (equal authority gnosis-themata--authority)
      (signal 'gnosis-core-authorization-error
              '("Gnosis authenticated authority changed")))
    (let ((gnosis-core--base-url-override base-url)
          (gnosis-core--token-override token))
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
    (gnosis-themata--start-cache-preload)
    (keymap-popup gnosis-dashboard-mode-map)))

;;;###autoload
(defun gnosis ()
  "Open the server-projected Gnosis dashboard."
  (interactive)
  (gnosis-dashboard))

(provide 'gnosis)
;;; gnosis.el ends here
