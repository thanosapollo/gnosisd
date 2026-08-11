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

(declare-function gnosis--dashboard-text "gnosis")
(declare-function gnosis--separator "gnosis")
(declare-function gnosis--center-text "gnosis")
(declare-function gnosis-dashboard--failure-message "gnosis")
(declare-function gnosis-dashboard--popup-description "gnosis")
(declare-function gnosis-dashboard--review-description "gnosis")
(declare-function gnosis-dashboard--active-session-p "gnosis")
(declare-function gnosis-dashboard-review "gnosis")
(declare-function gnosis-review "gnosis-review")
(declare-function gnosis-review-resume "gnosis-review")
(declare-function gnosis-review-topic "gnosis-review")
(declare-function gnosis-nodes-find "gnosis-nodes")
(declare-function gnosis-edit-read-kind "gnosis-edit")
(declare-function gnosis-edit-thema "gnosis-edit")
(declare-function gnosis-add-thema "gnosis-edit")

(defvar gnosis-dashboard--projection)
(defvar gnosis-dashboard-buffer-name)
(defvar gnosis-dashboard-mode-map)

(defconst gnosis-themata--render-chunk-size 500
  "Maximum EWOC rows formatted by one timer callback.")

(defvar-local gnosis-themata--ewoc nil
  "EWOC rendering the complete thema projection in this dashboard.")
(defvar-local gnosis-themata--records nil
  "Thema records keyed by opaque server identity.")
(defvar-local gnosis-themata--order nil
  "Opaque thema identities in authoritative catalog order.")
(defvar-local gnosis-themata--order-tail nil)
(defvar-local gnosis-themata--nodes nil
  "Visible EWOC nodes keyed by opaque thema identity.")
(defvar-local gnosis-themata--generation nil)
(defvar-local gnosis-themata--search-filter nil)
(defvar-local gnosis-themata--review-filter :all)
(defvar-local gnosis-themata--tag-filter nil)
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
    (vector (max 10 (/ (* available 28) 100))
            (max 8 (/ (* available 16) 100))
            (max 8 (/ (* available 16) 100))
            (max 8 (/ (* available 18) 100))
            (max 5 (/ (* available 10) 100))
            (max 5 (/ (* available 6) 100))
            (max 5 (/ (* available 6) 100)))))

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
    (propertize (concat "  " (string-join parts " "))
                'gnosis-thema-id id 'mouse-face 'highlight)))

(defun gnosis-themata--pretty-print-default (thema-id)
  "Insert the row identified by opaque THEMA-ID."
  (when-let* ((record (gethash thema-id gnosis-themata--records)))
    (insert (gnosis-themata--row-text record))))

(defun gnosis-themata--pretty-print (thema-id)
  "Insert EWOC row identified by opaque THEMA-ID."
  (gnosis-themata--pretty-print-default thema-id))

(defun gnosis-themata--header-line ()
  "Return current thema column header."
  (let ((names ["Question" "Hypothesis" "Answer" "Tags" "Kind" "Archived" "Suspended"])
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

(defun gnosis-themata--dashboard-header (projection)
  "Return summary and collection header for dashboard PROJECTION."
  (concat
   (propertize (gnosis--center-text "Gnosis Dashboard")
               'face 'gnosis-face-dashboard-header)
   "\n" (gnosis--separator) "\n\n"
   (gnosis--center-text (gnosis--dashboard-text projection))
   "\n\n" (gnosis-themata--header-line)))

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
          gnosis-themata--records (make-hash-table :test #'equal)
          gnosis-themata--nodes (make-hash-table :test #'equal)
          gnosis-themata--order nil
          gnosis-themata--order-tail nil
          gnosis-themata--generation nil
          gnosis-themata--loading-p t)
    (setq gnosis-themata--ewoc
          (ewoc-create #'gnosis-themata--pretty-print
                       (gnosis-themata--dashboard-header projection)
                       (gnosis-themata--status-text 'loading)))
    (goto-char (point-min))))

(defun gnosis-themata--current-node ()
  "Return visible EWOC node at point, or nil above the collection."
  (when (ewoc-p gnosis-themata--ewoc)
    (let ((first (ewoc-nth gnosis-themata--ewoc 0)))
      (when (and first (>= (point) (ewoc-location first)))
        (ewoc-locate gnosis-themata--ewoc (point))))))

(defun gnosis-themata--current-id ()
  "Return opaque thema identity at point, or nil."
  (when-let* ((node (gnosis-themata--current-node)))
    (ewoc-data node)))

(defun gnosis-themata--goto-id (thema-id)
  "Move point to visible THEMA-ID and return non-nil when found."
  (when-let* ((node (and thema-id (gethash thema-id gnosis-themata--nodes))))
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
  (pcase gnosis-themata--review-filter
    (:active (and (not (eq t (plist-get record :archived)))
                  (not (eq t (plist-get record :suspended)))))
    (:archived (eq t (plist-get record :archived)))
    (:suspended (eq t (plist-get record :suspended)))
    (_ t)))

(defun gnosis-themata--matches-tag-filter-p (record)
  "Return non-nil when RECORD matches current exact tag filter."
  (or (null gnosis-themata--tag-filter)
      (member gnosis-themata--tag-filter
              (append (plist-get record :tags) nil))))

(defun gnosis-themata--visible-p (record)
  "Return non-nil when RECORD belongs in current projection."
  (and (gnosis-themata--matches-search-p record)
       (gnosis-themata--matches-review-filter-p record)
       (gnosis-themata--matches-tag-filter-p record)))

(defun gnosis-themata--sort-value (record column)
  "Return RECORD sort value for COLUMN index."
  (aref (gnosis-themata--columns record) column))

(defun gnosis-themata--ordered-visible-ids ()
  "Return visible opaque ids in requested display order."
  (let ((ids
         (cl-loop for id in gnosis-themata--order
                  for record = (gethash id gnosis-themata--records)
                  when (and record (gnosis-themata--visible-p record))
                  collect id)))
    (if-let* ((column (car-safe gnosis-themata--sort-key)))
        (let ((descending (cdr gnosis-themata--sort-key)))
          (sort ids
                (lambda (left-id right-id)
                  (let ((left (gnosis-themata--sort-value
                               (gethash left-id gnosis-themata--records) column))
                        (right (gnosis-themata--sort-value
                                (gethash right-id gnosis-themata--records) column)))
                    (if descending
                        (string-lessp right left)
                      (string-lessp left right))))))
      ids)))

(defun gnosis-themata--capture-context ()
  "Return selected row, column, and visible `window-start' identity."
  (let* ((window (get-buffer-window (current-buffer)))
         (start-node (and window (ewoc-p gnosis-themata--ewoc)
                          (ewoc-locate gnosis-themata--ewoc
                                       (window-start window)))))
    (list :selected (gnosis-themata--current-id)
          :column (current-column)
          :start-id (and start-node (ewoc-data start-node)))))

(defun gnosis-themata--restore-context (context)
  "Restore selection, column, and window from CONTEXT when possible."
  (if-let* ((selected (plist-get context :selected))
            (_ (gnosis-themata--goto-id selected)))
      (move-to-column (plist-get context :column))
    (when-let* ((first (and (ewoc-p gnosis-themata--ewoc)
                            (ewoc-nth gnosis-themata--ewoc 0))))
      (ewoc-goto-node gnosis-themata--ewoc first)))
  (when-let* ((window (get-buffer-window (current-buffer)))
              (start-id (plist-get context :start-id))
              (node (gethash start-id gnosis-themata--nodes)))
    (set-window-start window (ewoc-location node) t)))

(defun gnosis-themata--clear-ewoc ()
  "Delete all current EWOC nodes and reset node index."
  (let ((inhibit-read-only t))
    (when (ewoc-p gnosis-themata--ewoc)
      (while-let ((node (ewoc-nth gnosis-themata--ewoc 0)))
        (ewoc-delete gnosis-themata--ewoc node))))
  (setq gnosis-themata--nodes (make-hash-table :test #'equal)))

(defun gnosis-themata--enter-last (thema-id)
  "Append opaque THEMA-ID to current EWOC and node index."
  (puthash thema-id (ewoc-enter-last gnosis-themata--ewoc thema-id)
           gnosis-themata--nodes))

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
      (when (and (derived-mode-p 'gnosis-dashboard-mode)
                 (equal run gnosis-themata--render-run)
                 gnosis-themata--render-state)
        (let* ((state gnosis-themata--render-state)
               (ids (plist-get state :ids))
               (count (min gnosis-themata--render-chunk-size (length ids)))
               (chunk (seq-take ids count))
               (remaining (nthcdr count ids)))
          (setq gnosis-themata--render-timer nil)
          (let ((inhibit-read-only t)
                (inhibit-modification-hooks t)
                (buffer-undo-list t))
            (dolist (id chunk) (gnosis-themata--enter-last id)))
          (if remaining
              (progn
                (setq gnosis-themata--render-state
                      (plist-put state :ids remaining)
                      gnosis-themata--render-timer
                      (run-at-time 0 nil #'gnosis-themata--render-step buffer run)))
            (setq gnosis-themata--render-state nil)
            (gnosis-themata--restore-context (plist-get state :context))
            (gnosis-themata--set-footer
             (if (zerop (hash-table-count gnosis-themata--nodes))
                 (if (zerop (hash-table-count gnosis-themata--records))
                     'empty
                   'empty-filter)
               nil))))))))

(defun gnosis-themata--render (&optional selected-id)
  "Progressively render current filter and sort, restoring SELECTED-ID."
  (let ((context (gnosis-themata--capture-context)))
    (when selected-id (setq context (plist-put context :selected selected-id)))
    (gnosis-themata--cancel-render)
    (gnosis-themata--clear-ewoc)
    (let ((run (cl-incf gnosis-themata--sequence)))
      (setq gnosis-themata--render-run run
            gnosis-themata--render-state
            (list :ids (gnosis-themata--ordered-visible-ids) :context context))
      (gnosis-themata--render-step (current-buffer) run))))

(defun gnosis-themata--append-order (state ids)
  "Return STATE after appending IDS in O(1) list time."
  (when ids
    (if-let* ((tail (plist-get state :order-tail)))
        (setcdr tail ids)
      (setq state (plist-put state :order ids)))
    (setq state (plist-put state :order-tail (last ids))))
  state)

(defun gnosis-themata--bootstrap-page-result (state page)
  "Validate PAGE atomically and return updated bootstrap STATE."
  (let* ((generation (plist-get state :generation))
         (page-generation (plist-get page :generation))
         (items (plist-get page :items))
         (next (plist-get page :next))
         (after (plist-get state :after))
         (records (plist-get state :records))
         (seen-cursors (plist-get state :seen-cursors))
         (page-seen (make-hash-table :test #'equal))
         ids)
    (unless (or (null generation) (equal generation page-generation))
      (signal 'gnosis-core-conflict '("Gnosis catalog changed during traversal")))
    (unless (vectorp items)
      (signal 'gnosis-core-protocol-error '("Invalid Gnosis thema catalog page")))
    (cl-loop for record across items
             for id = (plist-get record :thema_id)
             do (unless (and (gnosis-core--catalog-thema-p record)
                             (not (gethash id records))
                             (not (gethash id page-seen)))
                  (signal 'gnosis-core-protocol-error
                          '("Duplicate or invalid Gnosis thema catalog item")))
             do (puthash id t page-seen)
             do (push id ids))
    (unless (eq next gnosis-core--json-null)
      (when (or (= 0 (length items))
                (equal next after)
                (gethash next seen-cursors))
        (signal 'gnosis-core-protocol-error
                '("Gnosis thema catalog continuation did not advance"))))
    (setq ids (nreverse ids))
    (cl-loop for record across items
             do (puthash (plist-get record :thema_id) record records))
    (unless (eq next gnosis-core--json-null) (puthash next t seen-cursors))
    (setq state (gnosis-themata--append-order state ids)
          state (plist-put state :generation page-generation)
          state (plist-put state :after next)
          state (plist-put state :page-ids ids))
    (list :status (if (eq next gnosis-core--json-null) 'done 'continue)
          :state state)))

(defun gnosis-themata--refresh-context-p (buffer run incarnation authority)
  "Return non-nil when BUFFER owns RUN, INCARNATION, and AUTHORITY."
  (and (buffer-live-p buffer)
       (with-current-buffer buffer
         (and (derived-mode-p 'gnosis-dashboard-mode)
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

(defun gnosis-themata--publish-initial-page (state ids)
  "Publish one validated initial page IDS from STATE."
  (setq gnosis-themata--records (plist-get state :records)
        gnosis-themata--order (plist-get state :order)
        gnosis-themata--order-tail (plist-get state :order-tail)
        gnosis-themata--generation (plist-get state :generation))
  (let ((inhibit-read-only t)
        (inhibit-modification-hooks t)
        (buffer-undo-list t))
    (dolist (id ids)
      (when (gnosis-themata--visible-p (gethash id gnosis-themata--records))
        (gnosis-themata--enter-last id)))))

(defun gnosis-themata--install-refresh-state (state context)
  "Install terminal refresh STATE and progressively publish CONTEXT."
  (gnosis-themata--cancel-render)
  (gnosis-themata--clear-ewoc)
  (setq gnosis-themata--records (plist-get state :records)
        gnosis-themata--order (plist-get state :order)
        gnosis-themata--order-tail (plist-get state :order-tail)
        gnosis-themata--generation (plist-get state :generation))
  (let ((run (cl-incf gnosis-themata--sequence)))
    (setq gnosis-themata--render-run run
          gnosis-themata--render-state
          (list :ids (gnosis-themata--ordered-visible-ids) :context context))
    (gnosis-themata--render-step (current-buffer) run)))

(defun gnosis-themata--refresh-step (buffer run)
  "Fetch and publish one bounded catalog page for BUFFER and RUN."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-mode)
                 (equal run gnosis-themata--refresh-run)
                 gnosis-themata--refresh-state)
        (let ((incarnation gnosis-themata--incarnation)
              (authority gnosis-themata--authority)
              (state gnosis-themata--refresh-state))
          (setq gnosis-themata--refresh-timer nil)
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
                    (setq gnosis-themata--refresh-state updated)
                    (when (plist-get state :fresh)
                      (gnosis-themata--publish-initial-page
                       updated (plist-get updated :page-ids)))
                    (if (eq 'continue (plist-get result :status))
                        (gnosis-themata--schedule-refresh-step buffer run)
                      (setq gnosis-themata--refresh-state nil
                            gnosis-themata--refresh-timer nil
                            gnosis-themata--loading-p nil)
                      (if (plist-get state :fresh)
                          (gnosis-themata--set-footer
                           (if (zerop (hash-table-count gnosis-themata--records))
                               'empty nil))
                        (gnosis-themata--install-refresh-state
                         updated (plist-get state :context)))))))
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
                'error "Thema catalog unavailable. Press g to retry.")))))))))

(defun gnosis-themata--start-bootstrap (&optional selected-id fresh)
  "Start terminal catalog traversal, restoring SELECTED-ID.
When FRESH is non-nil, publish each accepted page progressively."
  (when (timerp gnosis-themata--refresh-timer)
    (cancel-timer gnosis-themata--refresh-timer))
  (let ((run (cl-incf gnosis-themata--sequence))
        (context (gnosis-themata--capture-context)))
    (when selected-id (setq context (plist-put context :selected selected-id)))
    (setq gnosis-themata--refresh-run run
          gnosis-themata--loading-p t
          gnosis-themata--refresh-state
          (list :kind 'bootstrap :fresh fresh :after nil :generation nil
                :records (make-hash-table :test #'equal)
                :order nil :order-tail nil
                :seen-cursors (make-hash-table :test #'equal)
                :context context))
    (gnosis-themata--set-footer (if fresh 'loading 'refreshing))
    (gnosis-themata--schedule-refresh-step (current-buffer) run)))

(defun gnosis-themata-refresh ()
  "Restart complete terminal catalog traversal without discarding live rows."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
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
                              "State: " '("all" "active" "suspended" "archived")
                              nil t nil nil "all")))))
  (unless (memq state '(:all :active :suspended :archived))
    (user-error "Unknown Gnosis thema state filter"))
  (setq gnosis-themata--review-filter state)
  (gnosis-themata--render (gnosis-themata--current-id)))

(defun gnosis-themata--column-at-point ()
  "Return column index at point, defaulting to Question."
  (or (get-text-property (point) 'gnosis-column-index) 0))

(defun gnosis-themata-sort (&optional column)
  "Sort by COLUMN and progressively rebuild complete projection.
With numeric prefix -1, restore authoritative catalog order."
  (interactive "P")
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
      (when (and (derived-mode-p 'gnosis-dashboard-mode)
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
  (dolist (timer (list gnosis-themata--layout-timer gnosis-themata--refresh-timer))
    (when (timerp timer) (cancel-timer timer)))
  (setq gnosis-themata--layout-timer nil
        gnosis-themata--refresh-timer nil
        gnosis-themata--refresh-state nil
        gnosis-themata--loading-p nil
        gnosis-themata--refresh-run (cl-incf gnosis-themata--sequence)))

(defun gnosis-themata--record-at-point ()
  "Return authoritative record at point or signal user error."
  (gnosis-themata--ensure-authority)
  (or (gethash (gnosis-themata--current-id) gnosis-themata--records)
      (user-error "No thema at point")))

(defun gnosis-themata--replace-record (record)
  "Replace catalog RECORD after an authoritative mutation."
  (let* ((id (plist-get record :thema_id))
         (previous (gethash id gnosis-themata--records))
         (old-visible (and previous (gnosis-themata--visible-p previous)))
         (new-visible (gnosis-themata--visible-p record))
         (node (gethash id gnosis-themata--nodes))
         (sort-stable
          (or (null gnosis-themata--sort-key)
              (and previous
                   (equal (gnosis-themata--sort-value
                           previous (car gnosis-themata--sort-key))
                          (gnosis-themata--sort-value
                           record (car gnosis-themata--sort-key))))))
         (context (gnosis-themata--capture-context)))
    (puthash id record gnosis-themata--records)
    (unless previous
      (let ((ids (list id)))
        (if gnosis-themata--order-tail
            (setcdr gnosis-themata--order-tail ids)
          (setq gnosis-themata--order ids))
        (setq gnosis-themata--order-tail ids)))
    (cond
     ((and old-visible new-visible sort-stable node)
      (ewoc-invalidate gnosis-themata--ewoc node))
     ((and (not old-visible) new-visible (null gnosis-themata--sort-key))
      (gnosis-themata--enter-last id))
     ((and old-visible (not new-visible) node)
      (ewoc-delete gnosis-themata--ewoc node)
      (remhash id gnosis-themata--nodes))
     (t (gnosis-themata--render (plist-get context :selected))))
    (gnosis-themata--restore-context context)))

(defun gnosis-themata--content-record (projection previous)
  "Return compact record from PROJECTION and PREVIOUS review state."
  (let ((record (or (copy-tree previous)
                    `(:thema_id ,(plist-get projection :thema_id)
                      :state_revision 0 :suspended ,gnosis-core--json-false))))
    (dolist (field '(:kind :keimenon :hypothesis :answer :tags :archived))
      (setq record (plist-put record field (plist-get projection field))))
    (plist-put record :thema_revision (plist-get projection :revision))))

(defun gnosis-themata--edited (buffer authority incarnation projection)
  "Update BUFFER from PROJECTION when AUTHORITY and INCARNATION still own it."
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and (derived-mode-p 'gnosis-dashboard-mode)
                 (equal authority gnosis-themata--authority)
                 (equal authority (gnosis-themata--current-authority))
                 (equal incarnation gnosis-themata--incarnation))
        (let* ((id (plist-get projection :thema_id))
               (previous (gethash id gnosis-themata--records)))
          (gnosis-themata--replace-record
           (gnosis-themata--content-record projection previous)))))))

(defun gnosis-themata-edit ()
  "Open shared editor for thema at point."
  (interactive)
  (let ((buffer (current-buffer))
        (authority gnosis-themata--authority)
        (incarnation gnosis-themata--incarnation)
        (id (plist-get (gnosis-themata--record-at-point) :thema_id)))
    (gnosis-edit-thema
     id (lambda (projection)
          (gnosis-themata--edited buffer authority incarnation projection)))))

(defun gnosis-themata-add ()
  "Open shared editor for a new thema."
  (interactive)
  (gnosis-themata--ensure-authority)
  (let ((buffer (current-buffer))
        (authority gnosis-themata--authority)
        (incarnation gnosis-themata--incarnation)
        (kind (gnosis-edit-read-kind)))
    (gnosis-add-thema
     (lambda (projection)
       (gnosis-themata--edited buffer authority incarnation projection))
     kind)))

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
  "Archive or unarchive thema at point after confirmation."
  (interactive)
  (let* ((record (gnosis-themata--record-at-point))
         (id (plist-get record :thema_id))
         (desired (not (eq t (plist-get record :archived))))
         (action (if desired "Archive" "Unarchive")))
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

(defun gnosis-themata-suspend-tag ()
  "Suspend each unsuspended thema in visible exact-tag set."
  (interactive)
  (gnosis-themata--ensure-authority)
  (unless gnosis-themata--tag-filter
    (user-error "Select a Gnosis tag filter first"))
  (let ((records
         (sort
          (cl-loop for id in gnosis-themata--order
                   for record = (gethash id gnosis-themata--records)
                   when (and (gnosis-themata--visible-p record)
                             (not (eq t (plist-get record :suspended))))
                   collect (copy-tree record))
          (lambda (left right)
            (string-lessp (plist-get left :thema_id)
                          (plist-get right :thema_id))))))
    (if (null records)
        (message "No unsuspended themata in this tag set")
      (when (yes-or-no-p (format "Suspend %d tagged themata? " (length records)))
        (gnosis-themata--call-with-authority
         (lambda ()
           (dolist (record records)
             (let* ((projection
                     (gnosis-core-thema-set-suspended
                      (plist-get record :thema_id)
                      (plist-get record :state_revision) t))
                    (state (plist-get projection :state))
                    (updated (copy-tree record)))
               (setq updated (plist-put updated :state_revision
                                        (plist-get state :state_revision))
                     updated (plist-put updated :suspended
                                        (plist-get state :suspended)))
               (gnosis-themata--replace-record updated)))))))))

(defun gnosis-dashboard--load-ewoc (&optional initial)
  "Load summary into EWOC dashboard.
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
  "Refresh summary and complete thema projection from server."
  (interactive)
  (unless (derived-mode-p 'gnosis-dashboard-mode)
    (user-error "Not in a Gnosis dashboard"))
  (condition-case condition
      (progn
        (gnosis-dashboard--load-ewoc nil)
        (gnosis-themata-refresh))
    (gnosis-core-error
     (user-error "%s" (gnosis-dashboard--failure-message condition)))))

;;;###autoload
(defun gnosis-dashboard ()
  "Open summary and complete server-owned thema dashboard."
  (interactive)
  (let* ((buffer (get-buffer-create gnosis-dashboard-buffer-name))
         (initial (not (with-current-buffer buffer
                         (and (derived-mode-p 'gnosis-dashboard-mode)
                              (ewoc-p gnosis-themata--ewoc))))))
    (pop-to-buffer-same-window buffer)
    (when initial (gnosis-dashboard-mode))
    (condition-case condition
        (progn
          (setq gnosis-themata--authority (gnosis-themata--current-authority))
          (gnosis-dashboard--load-ewoc initial))
      (gnosis-core-error
       (let ((message (gnosis-dashboard--failure-message condition)))
         (if (ewoc-p gnosis-themata--ewoc)
             (gnosis-themata--set-footer 'error message)
           (setq gnosis-dashboard--projection nil)
           (let ((inhibit-read-only t))
             (erase-buffer)
             (insert (propertize message 'face 'error)))))))
    (goto-char (point-min))
    (keymap-popup gnosis-dashboard-mode-map)))

;;;###autoload
(defun gnosis-dashboard-themata ()
  "Open dashboard and move to first visible thema when available."
  (interactive)
  (gnosis-dashboard)
  (when-let* ((node (and (ewoc-p gnosis-themata--ewoc)
                         (ewoc-nth gnosis-themata--ewoc 0))))
    (ewoc-goto-node gnosis-themata--ewoc node)))

(defun gnosis-dashboard-themata-mode ()
  "Compatibility entry selecting `gnosis-dashboard-mode'."
  (interactive)
  (gnosis-dashboard-mode))

(keymap-popup-define gnosis-dashboard-mode-map
  "Keymap for `gnosis-dashboard-mode'."
  :parent special-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description #'gnosis-dashboard--popup-description
  :group "Review"
  "r" (#'gnosis-dashboard--review-description gnosis-dashboard-review)
  "T" ("Review topic" gnosis-review-topic)
  "R" ("Active review controls" gnosis-review-resume
       :if #'gnosis-dashboard--active-session-p)
  :group "Browse"
  "n" ("Nodes / documents" gnosis-nodes-find)
  "g" ("Refresh dashboard" gnosis-dashboard-refresh :stay-open t)
  "q" ("Quit" quit-window)
  :group "Thema"
  "RET" ("Edit at point" gnosis-themata-edit)
  "e" ("Edit at point" gnosis-themata-edit)
  "a" ("Add thema" gnosis-themata-add)
  :group "Mutate"
  "s" ("Suspend or unsuspend" gnosis-themata-toggle-suspension)
  "A" ("Archive or unarchive" gnosis-themata-toggle-archive)
  "S" ("Suspend visible tag set" gnosis-themata-suspend-tag)
  :group "Filter"
  "t" ("Show themata" gnosis-dashboard-themata)
  "v" ("Show tag" gnosis-themata-find-tag)
  "/" ("Search themata" gnosis-themata-filter-search)
  "f" ("Review state" gnosis-themata-filter-review-state)
  :group "Layout"
  "o" ("Sort at point" gnosis-themata-sort)
  ">" ("Widen column" gnosis-themata-widen-current-column)
  "<" ("Narrow column" gnosis-themata-narrow-current-column))

(define-derived-mode gnosis-dashboard-mode special-mode "Gnosis"
  "Major mode for the Gnosis summary and complete thema projection."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines nil
              revert-buffer-function
              (lambda (&rest _) (gnosis-dashboard-refresh)))
  (setq gnosis-themata--incarnation (cl-incf gnosis-themata--sequence))
  (add-hook 'kill-buffer-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'change-major-mode-hook #'gnosis-themata--cancel-timers nil t)
  (add-hook 'window-size-change-functions
            #'gnosis-themata--window-size-change nil t))

(provide 'gnosis-dashboard)
;;; gnosis-dashboard.el ends here
