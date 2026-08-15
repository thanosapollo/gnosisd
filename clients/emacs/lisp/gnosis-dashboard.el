;;; gnosis-dashboard.el --- EWOC Gnosis dashboard  -*- lexical-binding: t; -*-

;; Copyright (C) 2022-2026 Free Software Foundation, Inc.

;; This file is part of Gnosis.

;;; Commentary:

;; Complete server-owned thema projection for the Gnosis dashboard.  HTTP
;; pages and timer callbacks stay bounded while native EWOC nodes provide one
;; continuous collection and surgical row updates.

;;; Code:

(require 'cl-lib)
(require 'ewoc)
(require 'gnosis)
(require 'gnosis-core)
(require 'keymap-popup)
(require 'seq)
(require 'subr-x)
(require 'tabulated-list)

(declare-function gnosis--dashboard-text "gnosis")
(declare-function gnosis--render-dashboard-failure "gnosis")
(declare-function gnosis--separator "gnosis")
(declare-function gnosis--center-text "gnosis")
(declare-function gnosis-dashboard--failure-message "gnosis")
(declare-function gnosis-dashboard--popup-description "gnosis")
(declare-function gnosis-dashboard--review-description "gnosis")
(declare-function gnosis-dashboard--active-session-p "gnosis")
(declare-function gnosis-review "gnosis-review")
(declare-function gnosis-review-resume "gnosis-review")
(declare-function gnosis-review-topic "gnosis-review")
(declare-function gnosis-nodes-find "gnosis-nodes")
(declare-function gnosis-edit-read-kind "gnosis-edit")
(declare-function gnosis-edit-thema "gnosis-edit")
(declare-function gnosis-add-thema "gnosis-edit")

(defvar gnosis-dashboard--projection)
(defvar gnosis-dashboard-buffer-name)
(defvar gnosis-dashboard-themata-buffer-name)
(defvar gnosis-dashboard-mode-map)
(defvar gnosis-dashboard-themata-mode-map)

(defvar gnosis-review-history-buffer-name "*Gnosis Review History*"
  "Name of the bounded review history buffer.")

(defvar-local gnosis-review-history--authority nil)
(defvar-local gnosis-review-history--cursor nil)
(defvar-local gnosis-review-history--next nil)
(defvar-local gnosis-review-history--previous nil)
(defvar-local gnosis-review-history--records nil)
(defvar-local gnosis-review-history--status nil)

(defconst gnosis-themata--render-chunk-size 500
  "Maximum EWOC rows formatted by one timer callback.")

(defconst gnosis-themata--bulk-chunk-size 500
  "Maximum selected themata sent in one mutation request.")

(defvar-local gnosis-themata--ewoc nil
  "EWOC rendering the complete thema projection in this dashboard.")
(defvar-local gnosis-themata--nodes nil
  "Complete EWOC nodes keyed by opaque thema identity.")
(defvar-local gnosis-themata--generation nil)
(defvar-local gnosis-themata--search-filter nil)
(defvar-local gnosis-themata--review-filter :all)
(defvar-local gnosis-themata--tag-filter nil)
(defvar-local gnosis-themata--max-reviews nil)
(defvar-local gnosis-themata--sort-key nil)
(defvar-local gnosis-themata--column-widths nil)
(defvar-local gnosis-themata--layout-width nil)
(defvar-local gnosis-themata--layout-timer nil)
(defvar-local gnosis-themata--refresh-timer nil)
(defvar-local gnosis-themata--refresh-run nil)
(defvar-local gnosis-themata--refresh-state nil)
(defvar-local gnosis-themata--render-timer nil)
(defvar-local gnosis-themata--render-run nil)
(defvar-local gnosis-themata--render-state nil)
(defvar-local gnosis-themata--loading-p nil)
(defvar-local gnosis-themata--authority nil)
(defvar-local gnosis-themata--incarnation nil)
(defvar-local gnosis-themata--marked nil)
(defvar-local gnosis-themata--bulk-timer nil)
(defvar-local gnosis-themata--active-bulk nil)
(defvar-local gnosis-themata--pending-bulk nil)
(defvar gnosis-themata--sequence 0)

(defun gnosis-themata--current-authority ()
  "Return non-secret identity of the current authenticated authority."
  (gnosis-core--authenticated-authority))

(defun gnosis-themata--ensure-authority ()
  "Reject use when live dashboard authority changed."
  (when (and gnosis-themata--authority
             (not (equal gnosis-themata--authority
                         (gnosis-themata--current-authority))))
    (signal 'gnosis-core-authorization-error
            '("Gnosis authenticated authority changed"))))

(defun gnosis-themata--column-format (width)
  "Return thema column widths proportioned for window WIDTH."
  (let ((available (max 56 (- width 8))))
    (vector (max 10 (/ (* available 27) 100))
            (max 8 (/ (* available 16) 100))
            (max 8 (/ (* available 16) 100))
            (max 8 (/ (* available 17) 100))
            (max 5 (/ (* available 9) 100))
            (max 5 (/ (* available 7) 100))
            (max 5 (/ (* available 8) 100)))))

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
  "Return readable boolean text for wire VALUE."
  (if (eq value t) "Yes" "No"))

(defun gnosis-themata--columns (record)
  "Return display columns for catalog RECORD."
  (vector
   (gnosis-themata--display-text (plist-get record :keimenon))
   (gnosis-themata--display-text
    (gnosis-themata--display-list (plist-get record :hypothesis)))
   (gnosis-themata--display-text
    (gnosis-themata--display-list (plist-get record :answer)))
   (gnosis-themata--display-list (plist-get record :tags))
   (plist-get record :kind)
   (gnosis-themata--yes-no (plist-get record :archived))
   (gnosis-themata--yes-no (plist-get record :suspended))))

(defun gnosis-themata--column-text (text width index face)
  "Return TEXT fitted to WIDTH for column INDEX using FACE."
  (let* ((text (or text ""))
         (fitted (truncate-string-to-width text width nil ?\s "…"))
         (padded (concat fitted (make-string (max 0 (- width (string-width fitted))) ?\s))))
    (propertize padded 'gnosis-column-index index 'face face)))

(defun gnosis-themata--row-text (record)
  "Return one propertized dashboard row for RECORD."
  (let* ((columns (gnosis-themata--columns record))
         (widths gnosis-themata--column-widths)
         (archived (eq t (plist-get record :archived)))
         (suspended (eq t (plist-get record :suspended)))
         (faces (vector 'default 'default 'default 'font-lock-constant-face
                        'font-lock-type-face
                        (if archived 'warning 'shadow)
                        (if suspended 'warning 'shadow)))
         (parts
          (cl-loop for index below (length columns)
                   collect (gnosis-themata--column-text
                            (aref columns index) (aref widths index) index
                            (aref faces index))))
         (id (plist-get record :thema_id)))
    (propertize (concat (if (gethash id gnosis-themata--marked)
                            (propertize "* " 'face 'font-lock-constant-face)
                          "  ")
                        (string-join parts " "))
                'gnosis-thema-id id 'mouse-face 'highlight)))

(defun gnosis-themata--pretty-print-default (record)
  "Insert dashboard row for EWOC-owned RECORD when visible."
  (when (gnosis-themata--visible-p record)
    (insert (gnosis-themata--row-text record))))

(defun gnosis-themata--pretty-print (record)
  "Insert EWOC-owned RECORD when visible."
  (gnosis-themata--pretty-print-default record))

(defun gnosis-themata--header-line ()
  "Return current thema column header."
  (let ((names ["Question" "Hypothesis" "Answer" "Tags" "Kind"
                "Archived" "Suspended"])
        (widths gnosis-themata--column-widths))
    (concat
     (propertize "Themata" 'face 'gnosis-face-dashboard-header)
     "\n"
     (propertize
      (concat "  "
              (string-join
               (cl-loop for index below (length names)
                        collect (gnosis-themata--column-text
                                 (aref names index) (aref widths index) index
                                 'shadow))
               " "))
      'face 'shadow)
     "\n")))

(defun gnosis-themata--dashboard-header (_projection)
  "Return the dedicated thema collection header."
  (concat
   (propertize (gnosis--center-text "Gnosis Themata")
               'face 'gnosis-face-dashboard-header)
   "\n" (gnosis--separator) "\n\n"
   (gnosis-themata--header-line)))

(defun gnosis-themata--status-text (kind &optional detail)
  "Return public status footer for KIND and optional DETAIL."
  (pcase kind
    ('loading (propertize "Loading complete thema catalog…" 'face 'shadow))
    ('refreshing (propertize "Refreshing complete thema catalog…" 'face 'shadow))
    ('empty (propertize "No themata yet. Press a to add one." 'face 'shadow))
    ('empty-filter (propertize "No themata match current filters." 'face 'shadow))
    ('conflict
     (propertize "Thema catalog changed during refresh. Press g to retry."
                 'face 'warning))
    ('error (propertize (or detail "Thema catalog unavailable.") 'face 'error))
    (_ "")))

(defun gnosis-themata--set-footer (kind &optional detail)
  "Set EWOC footer to status KIND and optional DETAIL."
  (when (ewoc-p gnosis-themata--ewoc)
    (ewoc-set-hf gnosis-themata--ewoc
                 (gnosis-themata--dashboard-header gnosis-dashboard--projection)
                 (gnosis-themata--status-text kind detail))))

(defun gnosis-themata--setup-ewoc (projection)
  "Create empty dashboard EWOC for server PROJECTION."
  (let ((inhibit-read-only t))
    (erase-buffer)
    (setq gnosis-dashboard--projection projection
          gnosis-themata--layout-width (window-width)
          gnosis-themata--column-widths
          (gnosis-themata--column-format gnosis-themata--layout-width)
          gnosis-themata--nodes (make-hash-table :test #'equal)
          gnosis-themata--generation nil
          gnosis-themata--loading-p t)
    (setq gnosis-themata--ewoc
          (ewoc-create #'gnosis-themata--pretty-print
                       (gnosis-themata--dashboard-header projection)
                       (gnosis-themata--status-text 'loading)))
    (goto-char (point-min))))

(defun gnosis-themata--id-at-position (position)
  "Return opaque thema identity rendered at POSITION, or nil."
  (save-excursion
    (goto-char position)
    (or (get-text-property position 'gnosis-thema-id)
        (get-text-property (line-beginning-position) 'gnosis-thema-id))))

(defun gnosis-themata--current-id ()
  "Return opaque thema identity at point, or nil."
  (gnosis-themata--id-at-position (point)))

(defun gnosis-themata--current-node ()
  "Return visible EWOC node at point, or nil."
  (gethash (gnosis-themata--current-id) gnosis-themata--nodes))

(defun gnosis-themata--move-row (step)
  "Move with EWOC STEP to a visible row, clamping at collection bounds."
  (when-let* ((node (gnosis-themata--current-node)))
    (let ((column (current-column))
          (next (funcall step gnosis-themata--ewoc node)))
      (while (and next
                  (not (gnosis-themata--visible-p (ewoc-data next))))
        (setq next (funcall step gnosis-themata--ewoc next)))
      (when next
        (ewoc-goto-node gnosis-themata--ewoc next)
        (move-to-column column)))))

(defun gnosis-themata-next-row ()
  "Move to the next visible thema, clamping at the final row."
  (interactive)
  (gnosis-themata--move-row #'ewoc-next))

(defun gnosis-themata-previous-row ()
  "Move to the previous visible thema, clamping at the first row."
  (interactive)
  (gnosis-themata--move-row #'ewoc-prev))

(defun gnosis-themata--goto-id (thema-id)
  "Move point to visible THEMA-ID and return non-nil when found."
  (when-let* ((node (and thema-id (gethash thema-id gnosis-themata--nodes)))
              (_ (gnosis-themata--visible-p (ewoc-data node))))
    (ewoc-goto-node gnosis-themata--ewoc node)
    t))

(defun gnosis-themata--matches-search-p (record)
  "Return non-nil when RECORD matches current text filter."
  (or (null gnosis-themata--search-filter)
      (let ((case-fold-search t)
            (query (regexp-quote gnosis-themata--search-filter)))
        (seq-some
         (lambda (value) (and (stringp value) (string-match-p query value)))
         (append (list (plist-get record :keimenon))
                 (append (plist-get record :hypothesis) nil)
                 (append (plist-get record :answer) nil)
                 (append (plist-get record :tags) nil))))))

(defun gnosis-themata--matches-review-filter-p (record)
  "Return non-nil when RECORD matches current review filter."
  (let ((archived (eq t (plist-get record :archived)))
        (suspended (eq t (plist-get record :suspended))))
    (pcase gnosis-themata--review-filter
      (:active (and (not archived) (not suspended)))
      (:suspended (and (not archived) suspended))
      (_ t))))

(defun gnosis-themata--matches-tag-filter-p (record)
  "Return non-nil when RECORD matches current exact tag filter."
  (or (null gnosis-themata--tag-filter)
      (member gnosis-themata--tag-filter
              (append (plist-get record :tags) nil))))

(defun gnosis-themata--visible-p (record)
  "Return non-nil when RECORD belongs in current projection."
  (and (gnosis-themata--matches-search-p record)
       (gnosis-themata--matches-review-filter-p record)
       (gnosis-themata--matches-tag-filter-p record)
       (or (null gnosis-themata--max-reviews)
           (<= (plist-get record :review_count)
               gnosis-themata--max-reviews))))

(defun gnosis-themata--sort-value (record column)
  "Return RECORD sort value for COLUMN index."
  (aref (gnosis-themata--columns record) column))

(defun gnosis-themata--complete-records ()
  "Return current materialized and pending EWOC records exactly once."
  (append (ewoc-collect gnosis-themata--ewoc #'identity)
          (copy-sequence (plist-get gnosis-themata--render-state :records))))

(defun gnosis-themata--ordered-records (&optional records)
  "Return complete RECORDS in requested display order.
When RECORDS is nil, use materialized and pending EWOC records."
  (let ((records (or records (gnosis-themata--complete-records))))
    (if-let* ((column (car-safe gnosis-themata--sort-key)))
        (let ((descending (cdr gnosis-themata--sort-key)))
          (sort records
                (lambda (left-record right-record)
                  (let ((left (gnosis-themata--sort-value left-record column))
                        (right (gnosis-themata--sort-value right-record column)))
                    (if descending
                        (string-lessp right left)
                      (string-lessp left right))))))
      (sort records
            (lambda (left-record right-record)
              (< (plist-get left-record :catalog-ordinal)
                 (plist-get right-record :catalog-ordinal)))))))

(defun gnosis-themata--capture-context ()
  "Return immutable selection and viewport ownership token."
  (let* ((buffer (current-buffer))
         (window (get-buffer-window buffer))
         (start-position (and window (window-start window)))
         (start-id (and start-position
                        (gnosis-themata--id-at-position start-position))))
    (list :buffer buffer
          :incarnation gnosis-themata--incarnation
          :authority gnosis-themata--authority
          :selected (gnosis-themata--current-id)
          :column (current-column)
          :window window
          :start-id start-id
          :selection-owned t
          :selection-fallback t
          :viewport-owned (and window start-id t)
          :expected-point (point)
          :expected-column (current-column)
          :expected-selected (gnosis-themata--current-id)
          :expected-window-start start-position
          :expected-start-id start-id)))

(defun gnosis-themata--context-owner-p (context)
  "Return non-nil when CONTEXT still owns the current dashboard."
  (and context
       (eq (current-buffer) (plist-get context :buffer))
       (derived-mode-p 'gnosis-dashboard-themata-mode)
       (equal gnosis-themata--incarnation (plist-get context :incarnation))
       (equal gnosis-themata--authority (plist-get context :authority))
       (condition-case nil
           (equal gnosis-themata--authority
                  (gnosis-themata--current-authority))
         (error nil))))

(defun gnosis-themata--reconcile-context (context)
  "Return next CONTEXT after dropping independently lost claims."
  (when (gnosis-themata--context-owner-p context)
    (let* ((next (copy-sequence context))
           (buffer (plist-get context :buffer))
           (window (plist-get context :window))
           (selected (gnosis-themata--current-id))
           (selection-unchanged
            (and (equal (point) (plist-get context :expected-point))
                 (equal (current-column) (plist-get context :expected-column))
                 (equal selected (plist-get context :expected-selected))))
           (window-valid
            (and (window-live-p window)
                 (eq (window-buffer window) buffer)))
           (start-position (and window-valid (window-start window)))
           (start-id (and start-position
                          (gnosis-themata--id-at-position start-position)))
           (viewport-unchanged
            (and window-valid
                 (equal start-position
                        (plist-get context :expected-window-start))
                 (equal start-id (plist-get context :expected-start-id)))))
      (when (and (plist-get context :selection-owned)
                 (not selection-unchanged))
        (setq next (plist-put next :selected selected)
              next (plist-put next :column (current-column))
              next (plist-put next :selection-owned (and selected t))
              next (plist-put next :selection-fallback nil)))
      (when (and (plist-get context :viewport-owned)
                 (not viewport-unchanged))
        (setq next (plist-put next :start-id start-id)
              next (plist-put next :viewport-owned (and start-id t))))
      (setq next (plist-put next :expected-point (point))
            next (plist-put next :expected-column (current-column))
            next (plist-put next :expected-selected selected)
            next (plist-put next :expected-window-start start-position)
            next (plist-put next :expected-start-id start-id))
      next)))

(defun gnosis-themata--observe-context (context)
  "Return next CONTEXT with observations after owned buffer mutation."
  (when (gnosis-themata--context-owner-p context)
    (let* ((next (copy-sequence context))
           (buffer (plist-get context :buffer))
           (window (plist-get context :window))
           (viewport-owned
            (and (plist-get context :viewport-owned)
                 (window-live-p window)
                 (eq (window-buffer window) buffer)))
           (start-position (and viewport-owned (window-start window))))
      (setq next (plist-put next :expected-point (point))
            next (plist-put next :expected-column (current-column))
            next (plist-put next :expected-selected
                            (gnosis-themata--current-id))
            next (plist-put next :viewport-owned viewport-owned)
            next (plist-put next :expected-window-start start-position)
            next (plist-put next :expected-start-id
                            (and start-position
                                 (gnosis-themata--id-at-position
                                  start-position))))
      next)))

(defun gnosis-themata--context-select (context selected-id)
  "Return copy of CONTEXT restoring SELECTED-ID."
  (let ((next (copy-sequence context)))
    (plist-put next :selected selected-id)))

(defun gnosis-themata--first-visible-node ()
  "Return first visible EWOC node, or nil."
  (let ((node (and (ewoc-p gnosis-themata--ewoc)
                   (ewoc-nth gnosis-themata--ewoc 0))))
    (while (and node (not (gnosis-themata--visible-p (ewoc-data node))))
      (setq node (ewoc-next gnosis-themata--ewoc node)))
    node))

(defun gnosis-themata--restore-context (context)
  "Restore still-owned selection and viewport dimensions from CONTEXT."
  (when (gnosis-themata--context-owner-p context)
    (when (plist-get context :selection-owned)
      (if-let* ((selected (plist-get context :selected))
                (_ (gnosis-themata--goto-id selected)))
          (move-to-column (plist-get context :column))
        (if (plist-get context :selection-fallback)
            (when-let* ((first (gnosis-themata--first-visible-node)))
              (ewoc-goto-node gnosis-themata--ewoc first))
          (goto-char (point-min)))))
    (when-let* (((plist-get context :viewport-owned))
                (window (plist-get context :window))
                (_ (window-live-p window))
                (_ (eq (window-buffer window) (current-buffer)))
                (start-id (plist-get context :start-id))
                (node (gethash start-id gnosis-themata--nodes))
                (_ (gnosis-themata--visible-p (ewoc-data node)))
                (position (ewoc-location node))
                (_ (equal start-id
                          (gnosis-themata--id-at-position position))))
      (set-window-start window position t))))

(defun gnosis-themata--clear-ewoc ()
  "Delete all current EWOC nodes and reset node index."
  (let ((inhibit-read-only t))
    (when (ewoc-p gnosis-themata--ewoc)
      (while-let ((node (ewoc-nth gnosis-themata--ewoc 0)))
        (ewoc-delete gnosis-themata--ewoc node))))
  (setq gnosis-themata--nodes (make-hash-table :test #'equal)))

(defun gnosis-themata--enter-last (record)
  "Append complete RECORD to current EWOC and node index."
  (puthash (plist-get record :thema_id)
           (ewoc-enter-last gnosis-themata--ewoc record)
           gnosis-themata--nodes))

(defun gnosis-themata--next-catalog-ordinal ()
  "Return ordinal following every complete catalog record."
  (cl-loop for record in (gnosis-themata--complete-records)
           maximize (plist-get record :catalog-ordinal) into maximum
           finally return (1+ (or maximum -1))))

(defun gnosis-themata--cancel-render ()
  "Cancel pending projection render."
  (when (timerp gnosis-themata--render-timer)
    (cancel-timer gnosis-themata--render-timer))
  (setq gnosis-themata--render-timer nil
        gnosis-themata--render-run (cl-incf gnosis-themata--sequence)
        gnosis-themata--render-state nil))

(defun gnosis-themata--render-step (buffer run)
  "Render one bounded projection chunk for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal run gnosis-themata--render-run)
                 gnosis-themata--render-state
                 (gnosis-themata--context-owner-p
                  (plist-get gnosis-themata--render-state :context)))
        (let* ((state gnosis-themata--render-state)
               (context
                (gnosis-themata--reconcile-context
                 (plist-get state :context)))
               (records (plist-get state :records))
               (count (min gnosis-themata--render-chunk-size (length records)))
               (chunk (seq-take records count))
               (remaining (nthcdr count records))
               (visible-count
                (+ (plist-get state :visible-count)
                   (cl-count-if #'gnosis-themata--visible-p chunk))))
          (setq gnosis-themata--render-timer nil)
          (let ((inhibit-read-only t)
                (inhibit-modification-hooks t)
                (buffer-undo-list t))
            (dolist (record chunk) (gnosis-themata--enter-last record)))
          (setq context (gnosis-themata--observe-context context)
                state (plist-put state :context context))
          (if remaining
              (progn
                (setq state (plist-put state :records remaining)
                      state (plist-put state :visible-count visible-count)
                      gnosis-themata--render-state state
                      gnosis-themata--render-timer
                      (run-at-time 0 nil #'gnosis-themata--render-step buffer run)))
            (setq gnosis-themata--render-state nil)
            (gnosis-themata--restore-context context)
            (gnosis-themata--set-footer
             (if (zerop visible-count)
                 (if (zerop (hash-table-count gnosis-themata--nodes))
                     'empty 'empty-filter)
               nil))))))))

(defun gnosis-themata--start-render (records context)
  "Progressively publish ordered RECORDS while CONTEXT owns the buffer."
  (when-let* ((context (gnosis-themata--reconcile-context context)))
    (gnosis-themata--cancel-render)
    (gnosis-themata--clear-ewoc)
    (setq context (gnosis-themata--observe-context context))
    (let ((run (cl-incf gnosis-themata--sequence)))
      (setq gnosis-themata--render-run run
            gnosis-themata--render-state
            (list :records records :visible-count 0 :context context))
      (gnosis-themata--render-step (current-buffer) run))))

(defun gnosis-themata--render (&optional selected-id)
  "Progressively render current filter and sort, restoring SELECTED-ID."
  (let* ((active-context
          (plist-get gnosis-themata--render-state :context))
         (context (or active-context (gnosis-themata--capture-context)))
         (records (gnosis-themata--ordered-records)))
    (when (and selected-id (null active-context))
      (setq context (gnosis-themata--context-select context selected-id)))
    (gnosis-themata--start-render records context)))

(defun gnosis-themata--append-refresh-records (state records)
  "Return refresh STATE after appending RECORDS in O(1) list time."
  (when records
    (if-let* ((tail (plist-get state :records-tail)))
        (setcdr tail records)
      (setq state (plist-put state :records records)))
    (setq state (plist-put state :records-tail (last records))))
  state)

(defun gnosis-themata--bootstrap-page-result (state page)
  "Validate PAGE atomically and return updated bootstrap STATE."
  (let* ((generation (plist-get state :generation))
         (page-generation (plist-get page :generation))
         (items (plist-get page :items))
         (next (plist-get page :next))
         (after (plist-get state :after))
         (seen-ids (plist-get state :seen-ids))
         (seen-cursors (plist-get state :seen-cursors))
         (next-ordinal (plist-get state :next-ordinal))
         (page-seen (make-hash-table :test #'equal))
         page-records)
    (unless (or (null generation) (equal generation page-generation))
      (signal 'gnosis-core-conflict '("Gnosis catalog changed during traversal")))
    (unless (vectorp items)
      (signal 'gnosis-core-protocol-error '("Invalid Gnosis thema catalog page")))
    (cl-loop for record across items
             for id = (plist-get record :thema_id)
             do (unless (and (gnosis-core--catalog-thema-p record)
                             (not (gethash id seen-ids))
                             (not (gethash id page-seen)))
                  (signal 'gnosis-core-protocol-error
                          '("Duplicate or invalid Gnosis thema catalog item")))
             do (puthash id t page-seen)
             do (push (plist-put (copy-tree record)
                                 :catalog-ordinal next-ordinal)
                      page-records)
             do (cl-incf next-ordinal))
    (unless (eq next gnosis-core--json-null)
      (when (or (= 0 (length items))
                (equal next after)
                (gethash next seen-cursors))
        (signal 'gnosis-core-protocol-error
                '("Gnosis thema catalog continuation did not advance"))))
    (setq page-records (nreverse page-records))
    (maphash (lambda (id _) (puthash id t seen-ids)) page-seen)
    (unless (eq next gnosis-core--json-null) (puthash next t seen-cursors))
    (unless (plist-get state :fresh)
      (setq state (gnosis-themata--append-refresh-records state page-records)))
    (setq state (plist-put state :generation page-generation)
          state (plist-put state :after next)
          state (plist-put state :next-ordinal next-ordinal)
          state (plist-put state :page-records page-records))
    (list :status (if (eq next gnosis-core--json-null) 'done 'continue)
          :state state)))

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

(defun gnosis-themata--schedule-refresh-step (buffer run)
  "Schedule one bounded refresh step for BUFFER and RUN."
  (setq gnosis-themata--refresh-timer
        (run-at-time 0 nil #'gnosis-themata--refresh-step buffer run)))

(defun gnosis-themata--publish-initial-page (state records)
  "Publish one validated initial page of EWOC-owned RECORDS from STATE."
  (setq gnosis-themata--generation (plist-get state :generation))
  (let ((inhibit-read-only t)
        (inhibit-modification-hooks t)
        (buffer-undo-list t))
    (dolist (record records)
      (gnosis-themata--enter-last record))))

(defun gnosis-themata--install-refresh-state (state context)
  "Install terminal refresh STATE and progressively publish CONTEXT."
  (when (gnosis-themata--context-owner-p context)
    (setq gnosis-themata--generation (plist-get state :generation))
    (gnosis-themata--start-render
     (gnosis-themata--ordered-records (plist-get state :records)) context)))

(defun gnosis-themata--refresh-step (buffer run)
  "Fetch and publish one bounded catalog page for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal run gnosis-themata--refresh-run)
                 gnosis-themata--refresh-state)
        (let* ((incarnation gnosis-themata--incarnation)
               (authority gnosis-themata--authority)
               (state gnosis-themata--refresh-state)
               (context
                (gnosis-themata--reconcile-context
                 (plist-get state :context))))
          (setq gnosis-themata--refresh-timer nil)
          (when context
            (setq state (plist-put state :context context))
            (condition-case nil
                (let* ((token (gnosis-core--token))
                       (base-url (gnosis-core--validated-base-url))
                       (request-authority
                        (vector base-url (secure-hash 'sha256 token))))
                  (unless (equal authority request-authority)
                    (signal 'gnosis-core-authorization-error
                            '("Gnosis credentials changed; reopen dashboard")))
                  (let* ((gnosis-core--token-override token)
                         (gnosis-core--base-url-override base-url)
                         (page (gnosis-core-thema-catalog
                                nil (plist-get state :after)
                                gnosis-core--thema-catalog-page-limit))
                         (result (gnosis-themata--bootstrap-page-result state page))
                         (updated (plist-get result :state)))
                    (when (gnosis-themata--refresh-context-p
                           buffer run incarnation authority)
                      (when (plist-get state :fresh)
                        (gnosis-themata--publish-initial-page
                         updated (plist-get updated :page-records))
                        (setq updated
                              (plist-put updated :context
                                         (gnosis-themata--observe-context
                                          (plist-get updated :context)))))
                      (setq gnosis-themata--refresh-state updated)
                      (if (eq 'continue (plist-get result :status))
                          (gnosis-themata--schedule-refresh-step buffer run)
                        (setq gnosis-themata--refresh-state nil
                              gnosis-themata--refresh-timer nil
                              gnosis-themata--loading-p nil)
                        (if (plist-get state :fresh)
                            (gnosis-themata--set-footer
                             (if (zerop (hash-table-count gnosis-themata--nodes))
                                 'empty nil))
                          (gnosis-themata--install-refresh-state
                           updated (plist-get updated :context)))))))
              (gnosis-core-conflict
               (when (gnosis-themata--refresh-context-p
                      buffer run incarnation authority)
                 (setq gnosis-themata--refresh-state nil
                       gnosis-themata--loading-p nil)
                 (gnosis-themata--set-footer 'conflict)))
              (gnosis-core-error
               (when (gnosis-themata--refresh-context-p
                      buffer run incarnation authority)
                 (setq gnosis-themata--refresh-state nil
                       gnosis-themata--loading-p nil)
                 (gnosis-themata--set-footer
                  'error "Thema catalog unavailable. Press g to retry."))))))))))

(defun gnosis-themata--start-bootstrap (&optional selected-id fresh)
  "Start terminal catalog traversal, restoring SELECTED-ID.
When FRESH is non-nil, publish each accepted page progressively."
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (let ((run (cl-incf gnosis-themata--sequence))
        (context (gnosis-themata--capture-context)))
    (when selected-id
      (setq context (gnosis-themata--context-select context selected-id)))
    (setq gnosis-themata--refresh-run run
          gnosis-themata--loading-p t
          gnosis-themata--refresh-state
          (list :kind 'bootstrap :fresh fresh :after nil :generation nil
                :records nil :records-tail nil
                :next-ordinal 0
                :seen-ids (make-hash-table :test #'equal)
                :seen-cursors (make-hash-table :test #'equal)
                :context context))
    (gnosis-themata--set-footer (if fresh 'loading 'refreshing))
    (gnosis-themata--schedule-refresh-step (current-buffer) run)))

(defun gnosis-themata-refresh ()
  "Restart complete terminal catalog traversal without discarding live rows."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-themata-mode)
    (user-error "Not in the Gnosis dashboard"))
  (gnosis-themata--ensure-authority)
  (gnosis-themata--start-bootstrap (gnosis-themata--current-id) nil))

(defun gnosis-themata-filter-search (query)
  "Filter authoritative themata by literal QUERY."
  (interactive "sThema search (empty clears): ")
  (setq gnosis-themata--search-filter (unless (string-empty-p query) query))
  (gnosis-themata--render (gnosis-themata--current-id)))

(defun gnosis-themata-filter-review-state (state)
  "Filter authoritative themata by review STATE."
  (interactive
   (list (intern (concat ":" (completing-read
                              "State: " '("all" "active" "suspended")
                              nil t nil nil "all")))))
  (unless (memq state '(:all :active :suspended))
    (user-error "Unknown Gnosis thema state filter"))
  (setq gnosis-themata--review-filter state)
  (gnosis-themata--render (gnosis-themata--current-id)))

(defun gnosis-themata-filter-max-reviews (maximum)
  "Show authoritative themata reviewed at most MAXIMUM times.
Without a prefix argument, show only never-reviewed themata."
  (interactive
   (list (if current-prefix-arg
             (read-number "Maximum reviews: " 0)
           0)))
  (unless (natnump maximum)
    (user-error "Maximum reviews must be non-negative"))
  (let ((selected (gnosis-themata--current-id)))
    (setq gnosis-themata--max-reviews maximum)
    (gnosis-themata--render selected)))

(defun gnosis-themata--column-at-point ()
  "Return column index at point, defaulting to Question."
  (or (get-text-property (point) 'gnosis-column-index) 0))

(defun gnosis-themata-sort (&optional column)
  "Sort by COLUMN and progressively rebuild complete projection.
With numeric prefix -1, restore authoritative catalog order."
  (interactive "P")
  (when gnosis-themata--loading-p
    (user-error "Wait for the complete Gnosis projection"))
  (let ((selected (gnosis-themata--current-id)))
    (if (equal column -1)
        (setq gnosis-themata--sort-key nil)
      (let ((column (or column (gnosis-themata--column-at-point))))
        (unless (and (integerp column) (<= 0 column 6))
          (user-error "Invalid column number"))
        (if (equal column (car-safe gnosis-themata--sort-key))
            (setcdr gnosis-themata--sort-key
                    (not (cdr gnosis-themata--sort-key)))
          (setq gnosis-themata--sort-key (cons column nil)))))
    (gnosis-themata--render selected)))

(defun gnosis-themata-widen-current-column (&optional n)
  "Widen current column by N and restart projection render."
  (interactive "p")
  (let* ((index (gnosis-themata--column-at-point))
         (n (or n 1)))
    (aset gnosis-themata--column-widths index
          (max 3 (+ (aref gnosis-themata--column-widths index) n)))
    (gnosis-themata--render (gnosis-themata--current-id))))

(defun gnosis-themata-narrow-current-column (&optional n)
  "Narrow current column by N and restart projection render."
  (interactive "p")
  (gnosis-themata-widen-current-column (- (or n 1))))

(defun gnosis-themata--apply-layout (buffer incarnation width)
  "Apply WIDTH to BUFFER when INCARNATION still owns it."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal incarnation gnosis-themata--incarnation))
        (setq gnosis-themata--layout-timer nil
              gnosis-themata--layout-width width
              gnosis-themata--column-widths (gnosis-themata--column-format width))
        (gnosis-themata--render (gnosis-themata--current-id))))))

(defun gnosis-themata--window-size-change (window)
  "Debounce thema column adjustment for resized WINDOW."
  (when (eq (window-buffer window) (current-buffer))
    (let ((width (window-width window)))
      (unless (equal width gnosis-themata--layout-width)
        (when (timerp gnosis-themata--layout-timer)
          (cancel-timer gnosis-themata--layout-timer))
        (setq gnosis-themata--layout-timer
              (run-with-idle-timer
               0.1 nil #'gnosis-themata--apply-layout
               (current-buffer) gnosis-themata--incarnation width))))))

(defun gnosis-themata--cancel-timers ()
  "Cancel dashboard rendering and catalog traversal timers."
  (gnosis-themata--cancel-render)
  (dolist (timer (list gnosis-themata--layout-timer gnosis-themata--refresh-timer
                       gnosis-themata--bulk-timer))
    (when (timerp timer) (cancel-timer timer)))
  (setq gnosis-themata--layout-timer nil
        gnosis-themata--refresh-timer nil
        gnosis-themata--refresh-state nil
        gnosis-themata--bulk-timer nil
        gnosis-themata--active-bulk nil
        gnosis-themata--pending-bulk nil
        gnosis-themata--loading-p nil
        gnosis-themata--refresh-run (cl-incf gnosis-themata--sequence)))

(defun gnosis-themata--record-at-point ()
  "Return authoritative record at point or signal user error."
  (gnosis-themata--ensure-authority)
  (or (when-let* ((node (gnosis-themata--current-node)))
        (ewoc-data node))
      (user-error "No thema at point")))

(defun gnosis-themata--replace-record (record)
  "Replace catalog RECORD after an authoritative mutation."
  (let* ((id (plist-get record :thema_id))
         (records (gnosis-themata--complete-records))
         (node (gethash id gnosis-themata--nodes))
         (previous
          (seq-find (lambda (candidate)
                      (equal id (plist-get candidate :thema_id)))
                    records))
         (record
          (plist-put (copy-tree record) :catalog-ordinal
                     (if previous
                         (plist-get previous :catalog-ordinal)
                       (gnosis-themata--next-catalog-ordinal))))
         (sort-stable
          (or (null gnosis-themata--sort-key)
              (and previous
                   (equal (gnosis-themata--sort-value
                           previous (car gnosis-themata--sort-key))
                          (gnosis-themata--sort-value
                           record (car gnosis-themata--sort-key))))))
         (context
          (if gnosis-themata--render-state
              (plist-get gnosis-themata--render-state :context)
            (gnosis-themata--capture-context)))
         (updated-records
          (if previous
              (mapcar (lambda (candidate)
                        (if (equal id (plist-get candidate :thema_id))
                            record candidate))
                      records)
            (append records (list record)))))
    (if gnosis-themata--render-state
        (gnosis-themata--start-render
         (gnosis-themata--ordered-records updated-records) context)
      (if node
          (ewoc-set-data node record)
        (gnosis-themata--enter-last record))
      (if (or (and node sort-stable)
              (and (null node) (null gnosis-themata--sort-key)))
          (progn
            (when node
              (let ((inhibit-read-only t))
                (ewoc-invalidate gnosis-themata--ewoc node)))
            (gnosis-themata--restore-context context))
        (gnosis-themata--start-render
         (gnosis-themata--ordered-records) context)))))

(defun gnosis-themata--content-record (projection previous)
  "Return compact record from PROJECTION and PREVIOUS review state."
  (let ((record (or (copy-tree previous)
                    `(:thema_id ,(plist-get projection :thema_id)
                      :state_revision 0 :review_count 0
                      :suspended ,gnosis-core--json-false))))
    (dolist (field '(:kind :keimenon :hypothesis :answer :tags :archived))
      (setq record (plist-put record field (plist-get projection field))))
    (plist-put record :thema_revision (plist-get projection :revision))))

(defun gnosis-themata--edited (buffer authority incarnation projection)
  "Update BUFFER from PROJECTION when AUTHORITY and INCARNATION still own it."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                 (equal authority gnosis-themata--authority)
                 (equal authority (gnosis-themata--current-authority))
                 (equal incarnation gnosis-themata--incarnation))
        (let* ((id (plist-get projection :thema_id))
               (previous
                (seq-find (lambda (record)
                            (equal id (plist-get record :thema_id)))
                          (gnosis-themata--complete-records))))
          (gnosis-themata--replace-record
           (gnosis-themata--content-record projection previous)))))))

(defun gnosis-themata-edit ()
  "Open shared editor for thema at point."
  (interactive)
  (gnosis-themata--call-with-authority
   (lambda ()
     (let ((buffer (current-buffer))
           (authority gnosis-themata--authority)
           (incarnation gnosis-themata--incarnation)
           (id (plist-get (gnosis-themata--record-at-point) :thema_id)))
       (gnosis-edit-thema
        id (lambda (projection)
             (gnosis-themata--edited buffer authority incarnation projection)))))))

(defun gnosis-themata-add ()
  "Open shared editor for a new thema."
  (interactive)
  (let ((kind (gnosis-edit-read-kind)))
    (gnosis-themata--call-with-authority
     (lambda ()
       (let ((buffer (current-buffer))
             (authority gnosis-themata--authority)
             (incarnation gnosis-themata--incarnation))
         (gnosis-add-thema
          (lambda (projection)
            (gnosis-themata--edited buffer authority incarnation projection))
          kind))))))

(defalias 'gnosis-themata-new #'gnosis-themata-add)

(defun gnosis-themata--call-with-authority (function)
  "Call FUNCTION with one token pinned to dashboard authority."
  (let* ((base-url (gnosis-core--validated-base-url))
         (token (gnosis-core--token))
         (authority (let ((gnosis-core--base-url-override base-url))
                      (gnosis-core--authenticated-authority token))))
    (unless (equal authority gnosis-themata--authority)
      (signal 'gnosis-core-authorization-error
              '("Gnosis authenticated authority changed")))
    (let ((gnosis-core--base-url-override base-url)
          (gnosis-core--token-override token))
      (funcall function))))

(defun gnosis-themata--set-mark (marked)
  "Set mark on thema at point to MARKED."
  (let* ((record (gnosis-themata--record-at-point))
         (id (plist-get record :thema_id))
         (node (gethash id gnosis-themata--nodes)))
    (if marked
        (puthash id t gnosis-themata--marked)
      (remhash id gnosis-themata--marked))
    (ewoc-invalidate gnosis-themata--ewoc node)))

(defun gnosis-themata-mark ()
  "Mark thema at point for a selected-item mutation."
  (interactive)
  (gnosis-themata--set-mark t))

(defun gnosis-themata-unmark ()
  "Unmark thema at point."
  (interactive)
  (gnosis-themata--set-mark nil))

(defun gnosis-themata-mark-all ()
  "Mark every thema in the complete current projection."
  (interactive)
  (when (or gnosis-themata--loading-p gnosis-themata--render-state)
    (user-error "Wait for the complete Gnosis projection"))
  (let ((context (gnosis-themata--capture-context)))
    (dolist (record (gnosis-themata--complete-records))
      (when (gnosis-themata--visible-p record)
        (puthash (plist-get record :thema_id) t gnosis-themata--marked)))
    (ewoc-refresh gnosis-themata--ewoc)
    (gnosis-themata--restore-context
     (gnosis-themata--observe-context context))))

(defun gnosis-themata-unmark-all ()
  "Unmark every thema."
  (interactive)
  (let ((context (gnosis-themata--capture-context)))
    (clrhash gnosis-themata--marked)
    (ewoc-refresh gnosis-themata--ewoc)
    (gnosis-themata--restore-context
     (gnosis-themata--observe-context context))))

(defun gnosis-themata--selected-records ()
  "Return marked records, or the current record when none are marked."
  (let ((records
         (cl-loop for record in (gnosis-themata--complete-records)
                  when (gethash (plist-get record :thema_id)
                                gnosis-themata--marked)
                  collect record)))
    (sort (or records (list (gnosis-themata--record-at-point)))
          (lambda (left right)
            (string-lessp (plist-get left :thema_id)
                          (plist-get right :thema_id))))))

(defun gnosis-themata--bulk-items (records kind)
  "Return stable mutation items for RECORDS and mutation KIND."
  (mapcar
   (lambda (record)
     (list :operation_id (gnosis-core-operation-id)
           :thema_id (plist-get record :thema_id)
           :expected_revision
           (plist-get record (if (eq kind :suspension)
                                 :state_revision
                               :thema_revision))))
   records))

(defun gnosis-themata--bulk-update-record (record kind result)
  "Return RECORD updated from accepted bulk RESULT for KIND."
  (let ((updated (copy-tree record))
        (projection (plist-get result :projection)))
    (if (eq kind :suspension)
        (let ((state (plist-get projection :state)))
          (setq updated (plist-put updated :state_revision
                                   (plist-get state :state_revision))
                updated (plist-put updated :suspended
                                   (plist-get state :suspended))))
      (setq updated (plist-put updated :thema_revision
                               (plist-get projection :revision))
            updated (plist-put updated :archived
                               (plist-get projection :archived))))
    updated))

(defun gnosis-themata--publish-bulk (state)
  "Publish accepted authority from terminal bulk STATE once."
  (let ((kind (plist-get state :kind))
        (accepted (plist-get state :accepted))
        (context (plist-get state :context)))
    (when accepted
      (dolist (result accepted)
        (let* ((id (plist-get result :thema_id))
               (node (gethash id gnosis-themata--nodes)))
          (when node
            (ewoc-set-data
             node (gnosis-themata--bulk-update-record
                   (ewoc-data node) kind result)))
          (remhash id gnosis-themata--marked)))
      (gnosis-themata--start-render
       (gnosis-themata--ordered-records)
       (gnosis-themata--observe-context context)))))

(defun gnosis-themata--finish-bulk (status pending)
  "Finish active bulk run with STATUS and retained PENDING items."
  (let* ((state gnosis-themata--active-bulk)
         (accepted (length (plist-get state :accepted)))
         (rejected (plist-get state :rejected)))
    (setq gnosis-themata--bulk-timer nil
          gnosis-themata--pending-bulk
          (and pending
               (list :kind (plist-get state :kind)
                     :desired (plist-get state :desired)
                     :items pending)))
    (gnosis-themata--publish-bulk state)
    (setq gnosis-themata--active-bulk nil)
    (message "Gnosis bulk %s: %d accepted, %d rejected, %d pending"
             status accepted rejected (length pending))))

(defun gnosis-themata--bulk-step (buffer run)
  "Execute one bounded selected-item request for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and gnosis-themata--active-bulk
                 (equal run (plist-get gnosis-themata--active-bulk :run)))
        (setq gnosis-themata--bulk-timer nil)
        (let* ((state gnosis-themata--active-bulk)
               (items (plist-get state :items))
               (chunk (seq-take items gnosis-themata--bulk-chunk-size))
               (remaining (nthcdr (length chunk) items))
               (kind (plist-get state :kind))
               (desired (plist-get state :desired)))
          (condition-case nil
              (let* ((results
                      (gnosis-themata--call-with-authority
                       (lambda ()
                         (if (eq kind :suspension)
                             (gnosis-core-themata-set-suspended chunk desired)
                           (gnosis-core-themata-set-archived chunk desired)))))
                     (accepted
                      (cl-remove-if-not
                       (lambda (result) (eq :applied (plist-get result :status)))
                       results))
                     (rejected (- (length results) (length accepted))))
                (setq state (plist-put state :items remaining)
                      state (plist-put state :accepted
                                       (append (plist-get state :accepted) accepted))
                      state (plist-put state :rejected
                                       (+ (plist-get state :rejected) rejected))
                      gnosis-themata--active-bulk state)
                (if remaining
                    (progn
                      (message "Gnosis bulk progress: %d/%d"
                               (- (plist-get state :total) (length remaining))
                               (plist-get state :total))
                      (setq gnosis-themata--bulk-timer
                            (run-at-time 0 nil #'gnosis-themata--bulk-step
                                         buffer run)))
                  (gnosis-themata--finish-bulk 'complete nil)))
            (gnosis-core-error
             (gnosis-themata--finish-bulk 'uncertain (append chunk remaining)))))))))

(defun gnosis-themata--start-bulk (kind desired)
  "Start selected-item bulk KIND to DESIRED state."
  (when gnosis-themata--active-bulk
    (user-error "A Gnosis bulk mutation is already active"))
  (when (or gnosis-themata--loading-p gnosis-themata--render-state)
    (user-error "Wait for the complete Gnosis projection"))
  (let* ((pending gnosis-themata--pending-bulk)
         (items
          (if pending
              (progn
                (unless (and (eq kind (plist-get pending :kind))
                             (eq desired (plist-get pending :desired)))
                  (user-error "Retry the pending Gnosis bulk mutation first"))
                (plist-get pending :items))
            (gnosis-themata--bulk-items
             (gnosis-themata--selected-records) kind)))
         (count (length items))
         (action (if (eq kind :suspension)
                     (if desired "Suspend" "Unsuspend")
                   (if desired "Delete" "Restore"))))
    (when (yes-or-no-p (format "%s %d selected %s? "
                               action count (if (= count 1) "thema" "themata")))
      (let ((run (cl-incf gnosis-themata--sequence)))
        (setq gnosis-themata--pending-bulk nil
              gnosis-themata--active-bulk
              (list :run run :kind kind :desired desired :items items
                    :total (length items) :accepted nil :rejected 0
                    :context (gnosis-themata--capture-context))
              gnosis-themata--bulk-timer
              (run-at-time 0 nil #'gnosis-themata--bulk-step
                           (current-buffer) run))
        (set-transient-map
         (let ((map (make-sparse-keymap)))
           (define-key map (kbd "C-g") #'gnosis-themata-cancel-bulk)
           map)
         (lambda () gnosis-themata--active-bulk))))))

(defun gnosis-themata-bulk-suspension (&optional unsuspend)
  "Suspend selected themata, or UNSUSPEND them with a prefix argument."
  (interactive "P")
  (gnosis-themata--start-bulk :suspension (not unsuspend)))

(defun gnosis-themata-bulk-archive (&optional unarchive)
  "Delete selected themata, or restore them when UNARCHIVE is non-nil."
  (interactive "P")
  (gnosis-themata--start-bulk :archive (not unarchive)))

(defun gnosis-themata-cancel-bulk ()
  "Cancel unsent chunks while preserving accepted server authority."
  (interactive)
  (unless gnosis-themata--active-bulk
    (user-error "No Gnosis bulk mutation is active"))
  (when (timerp gnosis-themata--bulk-timer)
    (cancel-timer gnosis-themata--bulk-timer))
  (gnosis-themata--finish-bulk
   'cancelled (plist-get gnosis-themata--active-bulk :items)))

(defun gnosis-themata-toggle-suspension ()
  "Suspend or unsuspend thema at point."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (id (plist-get record :thema_id))
         (desired (not (eq t (plist-get record :suspended))))
         (action (if desired "Suspend" "Unsuspend")))
    (when (y-or-n-p (format "%s thema at point? " action))
      (gnosis-themata--call-with-authority
       (lambda ()
         (let* ((projection (gnosis-core-thema-set-suspended
                             id (plist-get record :state_revision) desired))
                (state (plist-get projection :state))
                (updated (copy-tree record)))
           (setq updated (plist-put updated :state_revision
                                    (plist-get state :state_revision))
                 updated (plist-put updated :suspended
                                    (plist-get state :suspended)))
           (gnosis-themata--replace-record updated)))))))

(defun gnosis-themata-toggle-archive ()
  "Delete or restore thema at point after confirmation."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (id (plist-get record :thema_id))
         (desired (not (eq t (plist-get record :archived))))
         (action (if desired "Delete" "Restore")))
    (when (y-or-n-p (format "%s thema at point? " action))
      (gnosis-themata--call-with-authority
       (lambda ()
         (let ((updated (gnosis-themata--content-record
                         (gnosis-core-thema-set-archived
                          id (plist-get record :thema_revision) desired)
                         record)))
           (gnosis-themata--replace-record updated)))))))

(defun gnosis-themata-find-tag (tag)
  "Show authoritative thema set for exact TAG at point."
  (interactive
   (let ((tags (append (plist-get (gnosis-themata--record-at-point) :tags) nil)))
     (unless tags (user-error "Thema at point has no tags"))
     (list (completing-read "Tag: " tags nil t))))
  (setq gnosis-themata--tag-filter tag)
  (gnosis-themata--render (gnosis-themata--current-id)))


(defun gnosis-themata--reset-authority-state ()
  "Discard all dashboard state owned by the previous authority."
  (gnosis-themata--cancel-timers)
  (let ((inhibit-read-only t))
    (erase-buffer))
  (setq gnosis-dashboard--projection nil
        gnosis-themata--ewoc nil
        gnosis-themata--nodes (make-hash-table :test #'equal)
        gnosis-themata--generation nil
        gnosis-themata--search-filter nil
        gnosis-themata--review-filter :all
        gnosis-themata--tag-filter nil
        gnosis-themata--max-reviews nil
        gnosis-themata--sort-key nil
        gnosis-themata--marked (make-hash-table :test #'equal)
        gnosis-themata--pending-bulk nil))

(defun gnosis-review-history--date (review-day)
  "Return readable date for numeric REVIEW-DAY."
  (format "%04d-%02d-%02d"
          (/ review-day 10000)
          (% (/ review-day 100) 100)
          (% review-day 100)))

(defun gnosis-review-history--entry (event)
  "Return a public tabulated-list entry for review EVENT."
  (let ((accepted (eq t (plist-get event :accepted_success)))
        (detected (eq t (plist-get event :detected_success)))
        (overrides (plist-get event :override_count)))
    (list
     (plist-get event :event_id)
     (vector
      (propertize (gnosis-review-history--date (plist-get event :review_day))
                  'face 'font-lock-type-face)
      (propertize (if accepted "Success" "Failure")
                  'face (if accepted 'success 'error))
      (propertize (if detected "Success" "Failure")
                  'face (if detected 'success 'error))
      (propertize (number-to-string overrides)
                  'face (if (> overrides 0) 'warning 'shadow))))))

(defun gnosis-review-history--status-text ()
  "Return actionable text for current history status."
  (pcase gnosis-review-history--status
    ('loading (propertize "Loading review history…" 'face 'shadow))
    ('empty (propertize "No review history yet. Press g to refresh."
                       'face 'shadow))
    ('end (propertize "End of review history." 'face 'shadow))
    ('error (propertize "Review history unavailable. Press g to retry."
                       'face 'error))
    (_ "")))

(defun gnosis-review-history--render ()
  "Render current bounded history page and status."
  (setq tabulated-list-entries
        (mapcar #'gnosis-review-history--entry
                gnosis-review-history--records))
  (let ((inhibit-read-only t))
    (tabulated-list-print t)
    (save-excursion
      (goto-char (point-max))
      (insert "\n" (gnosis-review-history--status-text) "\n"))))

(defun gnosis-review-history--load (cursor &optional replace-authority)
  "Load bounded history page after CURSOR.
When REPLACE-AUTHORITY is non-nil, replace buffer ownership first.
Return `installed', `terminal', `error', or `stale'."
  (let* ((base-url (gnosis-core--validated-base-url))
         (token (let ((gnosis-core--base-url-override base-url))
                  (gnosis-core--token)))
         (authority
          (let ((gnosis-core--base-url-override base-url))
            (gnosis-core--authenticated-authority token))))
    (if replace-authority
        (setq gnosis-review-history--authority authority
              gnosis-review-history--cursor nil
              gnosis-review-history--next nil
              gnosis-review-history--previous nil
              gnosis-review-history--records nil)
      (unless (equal gnosis-review-history--authority authority)
        (signal 'gnosis-core-authorization-error
                '("Gnosis authenticated authority changed"))))
    (setq gnosis-review-history--status 'loading)
    (gnosis-review-history--render)
    (condition-case nil
        (let* ((gnosis-core--base-url-override base-url)
               (gnosis-core--token-override token)
               (page (gnosis-core-review-history cursor 100))
               (items (plist-get page :items))
               (next (plist-get page :next)))
          (cond
           ((not (equal gnosis-review-history--authority authority))
            'stale)
           ((and cursor (= (length items) 0))
            (setq gnosis-review-history--next gnosis-core--json-null
                  gnosis-review-history--status 'end)
            (gnosis-review-history--render)
            'terminal)
           (t
            (setq gnosis-review-history--cursor cursor
                  gnosis-review-history--next next
                  gnosis-review-history--records (append items nil)
                  gnosis-review-history--status
                  (if (= (length items) 0) 'empty nil))
            (gnosis-review-history--render)
            'installed)))
      (gnosis-core-error
       (if (not (equal gnosis-review-history--authority authority))
           'stale
         (setq gnosis-review-history--status 'error)
         (gnosis-review-history--render)
         'error)))))

(defun gnosis-review-history-refresh ()
  "Refresh the current bounded review history page."
  (interactive)
  (gnosis-review-history--load gnosis-review-history--cursor))

(defun gnosis-review-history-next ()
  "Show the next bounded review history page."
  (interactive)
  (when (or (null gnosis-review-history--next)
            (eq gnosis-review-history--next gnosis-core--json-null))
    (user-error "No later review history page"))
  (let ((cursor gnosis-review-history--cursor))
    (when (eq 'installed
              (gnosis-review-history--load gnosis-review-history--next))
      (push cursor gnosis-review-history--previous))))

(defun gnosis-review-history-previous ()
  "Show the previous bounded review history page."
  (interactive)
  (unless gnosis-review-history--previous
    (user-error "No earlier review history page"))
  (when (eq 'installed
            (gnosis-review-history--load
             (car gnosis-review-history--previous)))
    (pop gnosis-review-history--previous)))

(defun gnosis-review-history--next-inapt-p ()
  "Return non-nil when no next history page is available."
  (or (null gnosis-review-history--next)
      (eq gnosis-review-history--next gnosis-core--json-null)))

(defun gnosis-review-history--previous-inapt-p ()
  "Return non-nil when no previous history page is available."
  (null gnosis-review-history--previous))

(keymap-popup-define gnosis-review-history-mode-map
  "Keymap for `gnosis-review-history-mode'."
  :parent tabulated-list-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description "Review history"
  :group "Page"
  "n" ("Next page" gnosis-review-history-next
       :inapt-if #'gnosis-review-history--next-inapt-p)
  "p" ("Previous page" gnosis-review-history-previous
       :inapt-if #'gnosis-review-history--previous-inapt-p)
  "g" ("Refresh page" gnosis-review-history-refresh :stay-open t)
  "q" ("Quit" quit-window))

(define-derived-mode gnosis-review-history-mode tabulated-list-mode
  "Gnosis-History"
  "Major mode for bounded server-authoritative review history."
  (setq tabulated-list-format
        [("Date" 12 t)
         ("Result" 10 t)
         ("Detected" 10 t)
         ("Overrides" 10 t)])
  (setq-local tabulated-list-padding 0
              revert-buffer-function
              (lambda (&rest _) (gnosis-review-history-refresh)))
  (tabulated-list-init-header))

;;;###autoload
(defun gnosis-review-history ()
  "Open bounded server-authoritative review history."
  (interactive)
  (let ((buffer (get-buffer-create gnosis-review-history-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (unless (derived-mode-p 'gnosis-review-history-mode)
      (gnosis-review-history-mode))
    (gnosis-review-history--load nil t)
    (goto-char (point-min))
    (keymap-popup gnosis-review-history-mode-map)))

(defun gnosis-themata--load (&optional initial)
  "Load the dedicated thema collection.
Start fresh catalog traversal when INITIAL is non-nil."
  (let ((projection (gnosis-core-dashboard)))
    (if (ewoc-p gnosis-themata--ewoc)
        (progn
          (setq gnosis-dashboard--projection projection)
          (gnosis-themata--set-footer (and gnosis-themata--loading-p 'refreshing)))
      (gnosis-themata--setup-ewoc projection))
    (when initial
      (gnosis-themata--start-bootstrap nil t))))

(defun gnosis-dashboard-refresh ()
  "Refresh the server-projected dashboard summary."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
    (user-error "Not in the Gnosis dashboard"))
  (condition-case condition
      (let ((projection (gnosis-core-dashboard)))
        (setq gnosis-dashboard--projection projection)
        (gnosis--render-dashboard projection))
    (gnosis-core-error
     (setq gnosis-dashboard--projection nil)
     (gnosis--render-dashboard-failure condition)
     (user-error "%s" (gnosis-dashboard--failure-message condition)))))

;;;###autoload
(defun gnosis-dashboard ()
  "Open the server-projected Gnosis overview."
  (interactive)
  (let ((buffer (get-buffer-create gnosis-dashboard-buffer-name)))
    (pop-to-buffer-same-window buffer)
    (unless (derived-mode-p 'gnosis-dashboard-mode)
      (gnosis-dashboard-mode))
    (condition-case condition
        (let ((projection (gnosis-core-dashboard)))
          (setq gnosis-dashboard--projection projection)
          (gnosis--render-dashboard projection t))
      (gnosis-core-error
       (setq gnosis-dashboard--projection nil)
       (let ((inhibit-read-only t))
         (erase-buffer)
         (insert (propertize (gnosis-dashboard--failure-message condition)
                             'face 'error)))))
    (goto-char (point-min))
    (keymap-popup gnosis-dashboard-mode-map)))

;;;###autoload
(defun gnosis-dashboard-themata (&optional query tag max-reviews)
  "Open the complete server-owned thema collection.
QUERY, TAG, and MAX-REVIEWS select an initial server-owned projection."
  (interactive)
  (let* ((buffer (get-buffer-create gnosis-dashboard-themata-buffer-name))
         (initial (not (with-current-buffer buffer
                         (and (derived-mode-p 'gnosis-dashboard-themata-mode)
                              (ewoc-p gnosis-themata--ewoc))))))
    (pop-to-buffer-same-window buffer)
    (when initial (gnosis-dashboard-themata-mode))
    (condition-case condition
        (let* ((authority (gnosis-themata--current-authority))
               (authority-changed
                (and (not initial)
                     (not (equal gnosis-themata--authority authority)))))
          (when authority-changed
            (gnosis-themata--reset-authority-state))
          (setq gnosis-themata--authority authority)
          (let ((filters-changed
                 (or (not (equal gnosis-themata--search-filter query))
                     (not (eq gnosis-themata--review-filter :all))
                     (not (equal gnosis-themata--tag-filter tag))
                     (not (equal gnosis-themata--max-reviews max-reviews)))))
            (gnosis-themata--load (or initial authority-changed))
            (setq gnosis-themata--search-filter query
                  gnosis-themata--review-filter :all
                  gnosis-themata--tag-filter tag
                  gnosis-themata--max-reviews max-reviews)
            (when (and (ewoc-p gnosis-themata--ewoc) filters-changed)
              (gnosis-themata--render nil))))
      (gnosis-core-error
       (let ((message (gnosis-dashboard--failure-message condition)))
         (if (ewoc-p gnosis-themata--ewoc)
             (gnosis-themata--set-footer 'error message)
           (setq gnosis-dashboard--projection nil)
           (let ((inhibit-read-only t))
             (erase-buffer)
             (insert (propertize message 'face 'error)))))))
    (goto-char (point-min))
    (keymap-popup gnosis-dashboard-themata-mode-map)))

(defun gnosis-dashboard-search-themata (query)
  "Open the thema collection filtered by literal QUERY."
  (interactive "sThema search: ")
  (gnosis-dashboard-themata (unless (string-empty-p query) query)))

(defun gnosis-dashboard-themata-by-tag (tag)
  "Open the thema collection filtered by exact TAG."
  (interactive "sThema tag: ")
  (gnosis-dashboard-themata nil (unless (string-empty-p tag) tag)))

(defun gnosis-dashboard-themata-new (&optional maximum)
  "Open themata reviewed at most MAXIMUM times.
Without a prefix argument, show only never-reviewed themata."
  (interactive
   (list (if current-prefix-arg
             (read-number "Maximum reviews: " 0)
           0)))
  (setq maximum (or maximum 0))
  (unless (natnump maximum)
    (user-error "Maximum reviews must be non-negative"))
  (gnosis-dashboard-themata nil nil maximum))

(defun gnosis-dashboard-add-thema ()
  "Add a thema from the overview and refresh its server projection."
  (interactive)
  (let ((buffer (current-buffer))
        (kind (gnosis-edit-read-kind)))
    (gnosis-add-thema
     (lambda (_projection)
       (when (buffer-live-p buffer)
         (with-current-buffer buffer
           (when (derived-mode-p 'gnosis-dashboard-mode)
             (gnosis-dashboard-refresh)))))
     kind)))

(defun gnosis-dashboard-nodes ()
  "Open all server-backed nodes."
  (interactive)
  (gnosis-nodes-page-open
   "All" (lambda (after) (gnosis-core-nodes nil after 100))))

(defun gnosis-dashboard-health ()
  "Check gnosisd health and show authenticated content totals."
  (interactive)
  (condition-case condition
      (let* ((_status (gnosis-core-status))
             (projection (gnosis-core-dashboard))
             (content (plist-get projection :content)))
        (message "gnosisd ready — %d themata, %d documents, %d nodes"
                 (plist-get content :active_themata)
                 (plist-get content :active_documents)
                 (plist-get content :active_nodes)))
    (gnosis-core-error
     (user-error "%s" (gnosis-dashboard--failure-message condition)))))

(keymap-popup-define gnosis-nodes-map
  "Nodes"
  :description "Nodes"
  :group "Nodes"
  "a" ("View all nodes" gnosis-dashboard-nodes)
  "t" ("View nodes by tag" gnosis-nodes-find-by-tag)
  "SPC" ("Search node titles" gnosis-nodes-search)
  "c" ("Search document content" gnosis-nodes-search-content))

(keymap-popup-define gnosis-themata-map
  "Themata"
  :description "Themata"
  :group "Themata"
  "a" ("View all themata" gnosis-dashboard-themata)
  "SPC" ("Search themata" gnosis-dashboard-search-themata)
  "t" ("View by tag" gnosis-dashboard-themata-by-tag)
  "n" ("New or low reviews" gnosis-dashboard-themata-new))

(keymap-popup-define gnosis-dashboard-mode-map
  "Gnosis Dashboard"
  :parent special-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description #'gnosis-dashboard--popup-description
  :group "Navigate"
  "n" ("Nodes" :keymap gnosis-nodes-map)
  "t" ("Themata" :keymap gnosis-themata-map)
  "h" ("Health" gnosis-dashboard-health :stay-open t)
  "H" ("Review history" gnosis-review-history)
  :group "Review"
  "r" (#'gnosis-dashboard--review-description gnosis-review)
  "T" ("Review topic" gnosis-review-topic)
  "R" ("Active review controls" gnosis-review-resume
       :if #'gnosis-dashboard--active-session-p)
  :group "Actions"
  "a" ("Add thema" gnosis-dashboard-add-thema)
  "SPC" ("Search themata" gnosis-dashboard-search-themata)
  "g" ("Refresh overview" gnosis-dashboard-refresh :stay-open t)
  :group "Window"
  "q" ("Quit" quit-window))

(keymap-popup-define gnosis-dashboard-themata-mode-map
  "Keymap for `gnosis-dashboard-themata-mode'."
  :parent special-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description "Gnosis Themata"
  :group "Navigate"
  "q" ("Back" gnosis-dashboard)
  "n" ("Next row" gnosis-themata-next-row :stay-open t)
  "p" ("Previous row" gnosis-themata-previous-row :stay-open t)
  "RET" ("Edit at point" gnosis-themata-edit)
  :group "Quick views"
  "SPC" ("Search" gnosis-themata-filter-search)
  "N" ("New or low reviews" gnosis-themata-filter-max-reviews)
  :group "Edit"
  "e" ("Edit thema" gnosis-themata-edit)
  "a" ("Add thema" gnosis-themata-add)
  "s" ("Suspend or unsuspend" gnosis-themata-toggle-suspension)
  "d" ("Delete selected (prefix: restore)" gnosis-themata-bulk-archive)
  :group "Selection"
  "*" ("Mark current" gnosis-themata-mark :stay-open t)
  "u" ("Unmark current" gnosis-themata-unmark :stay-open t)
  "M" ("Mark all visible" gnosis-themata-mark-all :stay-open t)
  "U" ("Unmark all" gnosis-themata-unmark-all :stay-open t)
  :group "Mutate"
  "S" ("Suspend selected (prefix: unsuspend)" gnosis-themata-bulk-suspension)
  "x" ("Cancel bulk mutation" gnosis-themata-cancel-bulk
       :if (lambda () gnosis-themata--active-bulk))
  :group "Filter"
  "t" ("Show tag at point" gnosis-themata-find-tag)
  "/" ("Search" gnosis-themata-filter-search)
  "f" ("Review state" gnosis-themata-filter-review-state)
  "m" ("Maximum reviews" gnosis-themata-filter-max-reviews)
  :group "Layout"
  "o" ("Sort at point" gnosis-themata-sort)
  ">" ("Widen column" gnosis-themata-widen-current-column)
  "<" ("Narrow column" gnosis-themata-narrow-current-column)
  "g" ("Refresh" gnosis-themata-refresh :stay-open t))

(define-derived-mode gnosis-dashboard-mode special-mode "Gnosis Dashboard"
  "Major mode for the server-projected Gnosis overview."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-dashboard-refresh))))

(define-derived-mode gnosis-dashboard-themata-mode
    special-mode "Gnosis Themata"
  "Major mode for the complete server-owned thema collection."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-themata-refresh)))
  (setq gnosis-themata--incarnation (cl-incf gnosis-themata--sequence)
        gnosis-themata--marked (make-hash-table :test #'equal))
  (add-hook 'kill-buffer-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'change-major-mode-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'window-size-change-functions
            #'gnosis-themata--window-size-change nil t))

(provide 'gnosis-dashboard)
;;; gnosis-dashboard.el ends here
