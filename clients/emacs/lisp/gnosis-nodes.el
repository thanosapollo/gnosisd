;;; gnosis-nodes.el --- Offline-editable Org synchronization  -*- lexical-binding: t; -*-

;; Copyright (C) 2023-2026  Thanos Apollo

;; Author: Thanos Apollo <public@thanosapollo.org>
;; Keywords: outlines
;; URL: https://thanosapollo.org/projects/gnosis
;;; Commentary:

;; Materialize authoritative Gnosis documents as normal Org files while keeping
;; only synchronization metadata, pending operations, and conflicts locally.

;;; Code:

(require 'cl-lib)
(require 'json)
(require 'org)
(require 'org-id)
(require 'seq)
(require 'subr-x)
(require 'tabulated-list)
(require 'keymap-popup)
(require 'gnosis-core)

(defgroup gnosis-nodes nil
  "Offline-editable Gnosis Org files."
  :group 'gnosis)

(defcustom gnosis-nodes-dir (expand-file-name "~/Notes/")
  "Root directory containing synchronized Org files."
  :type 'directory
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-timestring "%Y%m%d%H%M%S"
  "Time format prepended to filenames created by `gnosis-nodes-insert'.
When nil, use only the selected node title."
  :type '(choice string (const :tag "Title only" nil))
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-templates nil
  "Named Org templates available to `gnosis-nodes-insert-template'.
Each value is a string or a function returning a string.  Braced stars such
as `{*}' and `{**}' expand relative to the current Org heading."
  :type '(alist :key-type string
                :value-type (choice string function))
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-bulk-link-string-length 512
  "Maximum literal character count accepted by bulk link conversion."
  :type 'integer
  :group 'gnosis-nodes)

(defcustom gnosis-journal-directory "journal/"
  "Relative directory below `gnosis-nodes-dir' containing journal entries."
  :type 'string
  :group 'gnosis-nodes)

(defcustom gnosis-journal-date-format "%Y-%m-%d"
  "Time format used for journal titles and filenames."
  :type 'string
  :group 'gnosis-nodes)

(defcustom gnosis-journal-template "{*} Notes\n"
  "Org body inserted into a new journal entry."
  :type 'string
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-state-file nil
  "Synchronization state file.
When nil, use private `gnosis/sync/state.json' below `user-emacs-directory'."
  :type '(choice (const :tag "Private user state" nil) file)
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-idle-seconds 30
  "Idle seconds between global synchronization attempts."
  :type 'natnum
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-pages-per-pass 16
  "Maximum remote pages consumed by one synchronization pass."
  :type 'natnum
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-mutations-per-pass 16
  "Maximum pending mutations sent by one synchronization pass."
  :type 'natnum
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-outbox-scan-per-pass 256
  "Maximum durable outbox entries inspected by one synchronization pass."
  :type 'natnum
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-local-scan-per-pass 256
  "Maximum enrolled local documents inspected during one sync pass."
  :type 'natnum
  :group 'gnosis-nodes)

(defcustom gnosis-nodes-max-snapshot-actions-per-pass 32
  "Maximum snapshot items validated, fetched, or reconciled per pass."
  :type 'natnum
  :group 'gnosis-nodes)

(define-error 'gnosis-nodes-error "Gnosis Org synchronization failed")
(define-error 'gnosis-nodes-busy "Gnosis Org synchronization is busy"
  'gnosis-nodes-error)
(define-error 'gnosis-nodes-path-error "Unsafe Gnosis Org path" 'gnosis-nodes-error)
(define-error 'gnosis-nodes-conflict "Gnosis Org conflict" 'gnosis-nodes-error)

(defvar gnosis-nodes--idle-timer nil)
(defvar gnosis-nodes--stop-requested nil)
(defvar gnosis-nodes--busy nil)
(defvar gnosis-nodes-mode nil)
(defvar gnosis-nodes--pull-complete t)
(defvar gnosis-nodes--preflight nil)
(defvar gnosis-nodes--conflict-base-blobs nil)
(defvar gnosis-nodes--pages-left nil)
(defvar gnosis-nodes--mutation-sent nil)
(defvar gnosis-nodes--recovery-visits 0)
(defvar gnosis-nodes--ediff-sessions (make-hash-table :test #'equal))
(defvar gnosis-nodes--node-catalog nil)
(defvar gnosis-nodes--changed-document-ids nil)

(defvar-local gnosis-nodes--content-sha256 nil)
(defvar-local gnosis-nodes--document-revision nil)

(defvar-local gnosis-nodes-page--fetch nil)
(defvar-local gnosis-nodes-page--cursor nil)
(defvar-local gnosis-nodes-page--next nil)
(defvar-local gnosis-nodes-page--history nil)
(defvar-local gnosis-nodes-page--records nil)

(defconst gnosis-nodes--catalog-page-limit 5000)
(defconst gnosis-nodes--catalog-max-nodes 100000)
(defconst gnosis-nodes--catalog-incremental-limit 16)
(defconst gnosis-nodes--catalog-version 2)

(defun gnosis-nodes--private-state-root ()
  "Return configured user's canonical private synchronization root."
  (file-name-as-directory
   (expand-file-name (locate-user-emacs-file "gnosis/sync/"))))

(defun gnosis-nodes--symlink-ancestor-p (path)
  "Return non-nil when an existing component of PATH is a symlink."
  (let ((current (expand-file-name path)) found parent)
    (while (and (not found)
                (progn
                  (when (file-symlink-p current) (setq found t))
                  (setq parent (file-name-directory
                                (directory-file-name current)))
                  (not (equal current parent))))
      (setq current parent))
    found))

(defun gnosis-nodes--state-file ()
  "Return confined absolute synchronization state filename."
  (let* ((root (gnosis-nodes--private-state-root))
         (file (expand-file-name
                (or gnosis-nodes-state-file
                    (expand-file-name "state.json" root)))))
    (unless (string-prefix-p root file)
      (signal 'gnosis-nodes-path-error (list file)))
    (when (or (gnosis-nodes--symlink-ancestor-p root)
              (gnosis-nodes--symlink-ancestor-p file))
      (signal 'gnosis-nodes-path-error (list file)))
    file))

(defun gnosis-nodes--state-directory ()
  "Return private synchronization state directory."
  (file-name-directory (gnosis-nodes--state-file)))

(defun gnosis-nodes--safe-path (root logical-path)
  "Return safe path below ROOT for relative Org LOGICAL-PATH."
  (let ((parts (and (stringp logical-path)
                    (split-string logical-path "/" nil))))
    (unless (and parts
                 (not (file-name-absolute-p logical-path))
                 (equal "org" (file-name-extension logical-path))
                 (seq-every-p
                  (lambda (part)
                    (and (not (string-empty-p part))
                         (not (member part '("." "..")))))
                  parts))
      (signal 'gnosis-nodes-path-error (list logical-path)))
    (let* ((root (file-name-as-directory (expand-file-name root)))
           (path (expand-file-name logical-path root)))
      (unless (string-prefix-p root path)
        (signal 'gnosis-nodes-path-error (list logical-path)))
      (when (or (gnosis-nodes--symlink-ancestor-p root)
                (file-symlink-p (directory-file-name root))
                (gnosis-nodes--symlink-component-p root path))
        (signal 'gnosis-nodes-path-error (list logical-path)))
      path)))

(defun gnosis-nodes--symlink-component-p (root path)
  "Return non-nil when existing path from ROOT through PATH is a symlink."
  (let ((relative (file-relative-name path root))
        (current (file-name-as-directory (expand-file-name root)))
        found)
    (dolist (part (split-string relative "/" t) found)
      (setq current (expand-file-name part current))
      (when (file-symlink-p current)
        (setq found t)))))

(defun gnosis-nodes--write-bytes (path bytes)
  "Atomically write unibyte BYTES to PATH, rejecting symlinks."
  (let* ((state-root (gnosis-nodes--private-state-root))
         (state-p (string-prefix-p state-root (expand-file-name path)))
         (root (file-name-as-directory
                (expand-file-name
                 (if state-p state-root gnosis-nodes-dir)))))
    (when (file-symlink-p (directory-file-name root))
      (signal 'gnosis-nodes-path-error (list root)))
    (when (gnosis-nodes--symlink-component-p root path)
      (signal 'gnosis-nodes-path-error (list path)))
    (make-directory (file-name-directory path) t)
    (when state-p
      (set-file-modes (file-name-directory path) #o700))
    (let ((temporary (make-temp-file
                      (expand-file-name ".gnosis-write-"
                                        (file-name-directory path)))))
      (unwind-protect
          (progn
            (with-temp-buffer
              (set-buffer-multibyte nil)
              (insert bytes)
              (let ((coding-system-for-write 'no-conversion)
                    (write-region-inhibit-fsync nil))
                (write-region nil nil temporary nil 'silent)))
            (rename-file temporary path t))
        (when (file-exists-p temporary)
          (delete-file temporary))))))

(defun gnosis-nodes--write-document (logical-path bytes)
  "Atomically materialize exact Org BYTES at LOGICAL-PATH."
  (let* ((path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (buffer (get-file-buffer path)))
    (when (and (buffer-live-p buffer) (buffer-modified-p buffer))
      (signal 'gnosis-nodes-conflict (list logical-path)))
    (unless gnosis-nodes--preflight
      (gnosis-nodes--write-bytes path (string-to-unibyte bytes))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer
          (revert-buffer :ignore-auto :noconfirm))))))

(defun gnosis-nodes--move-document (old-logical-path logical-path bytes)
  "Move clean OLD-LOGICAL-PATH to LOGICAL-PATH with authoritative BYTES."
  (let* ((old-path (gnosis-nodes--safe-path
                    gnosis-nodes-dir old-logical-path))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (old-buffer (get-file-buffer old-path))
         (old-bytes (gnosis-nodes--working-bytes old-path))
         (new-bytes (gnosis-nodes--working-bytes path))
         (expected (gnosis-nodes--digest bytes)))
    (when (or (get-file-buffer path)
              (and (buffer-live-p old-buffer)
                   (buffer-modified-p old-buffer))
              (and new-bytes
                   (not (equal expected (gnosis-nodes--digest new-bytes))))
              (not (or old-bytes new-bytes)))
      (signal 'gnosis-nodes-conflict (list logical-path)))
    (unless gnosis-nodes--preflight
      (unless new-bytes
        (gnosis-nodes--write-document logical-path bytes))
      (when old-bytes
        (delete-file old-path))
      (when (buffer-live-p old-buffer)
        (with-current-buffer old-buffer
          (set-visited-file-name path t)
          (revert-buffer :ignore-auto :noconfirm))))))

(defun gnosis-nodes--read-bytes (path)
  "Return exact bytes from PATH."
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (insert-file-contents-literally path)
    (buffer-string)))

(defun gnosis-nodes--working-bytes (path)
  "Return exact bytes from regular PATH, nil when absent.
Reject non-regular destinations without opening them."
  (when (file-exists-p path)
    (unless (and (file-regular-p path) (not (file-symlink-p path)))
      (signal 'gnosis-nodes-path-error (list path)))
    (gnosis-nodes--read-bytes path)))

(defun gnosis-nodes--managed-bytes (root path)
  "Return exact bytes from regular PATH confined below private ROOT."
  (unless (and (stringp path) (stringp root))
    (signal 'gnosis-nodes-path-error (list path)))
  (let* ((root (file-name-as-directory (expand-file-name root)))
         (path (expand-file-name path)))
    (unless (and (string-prefix-p root path)
                 (not (gnosis-nodes--symlink-ancestor-p root))
                 (not (gnosis-nodes--symlink-component-p root path))
                 (file-regular-p path)
                 (not (file-symlink-p path)))
      (signal 'gnosis-nodes-path-error (list path)))
    (gnosis-nodes--read-bytes path)))

(defun gnosis-nodes--digest (bytes)
  "Return SHA-256 hex digest of unibyte BYTES."
  (secure-hash 'sha256 (string-to-unibyte bytes)))

(defun gnosis-nodes--catalog-authority ()
  "Return a non-secret identity for the current authenticated authority."
  (list (gnosis-core--validated-base-url)
        (secure-hash 'sha256 (gnosis-core--token))))

(defun gnosis-nodes--fetch-node-records (&optional document-id)
  "Return compact server node records for optional DOCUMENT-ID."
  (let ((after nil)
        (done nil)
        (generation nil)
        (records nil)
        (seen-ids (make-hash-table :test #'equal))
        (seen-cursors (make-hash-table :test #'equal)))
    (while (not done)
      (let* ((page
              (gnosis-core-node-catalog
               document-id after gnosis-nodes--catalog-page-limit))
             (items (plist-get page :items))
             (next (plist-get page :next))
             (page-generation (plist-get page :generation)))
        (if generation
            (unless (equal generation page-generation)
              (signal 'gnosis-nodes-error
                      '("Gnosis node catalog changed during pagination")))
          (setq generation page-generation))
        (when (> (+ (length records) (length items))
                 gnosis-nodes--catalog-max-nodes)
          (signal 'gnosis-nodes-error '("Gnosis node catalog is too large")))
        (cl-loop
         for node across items
         for node-id = (plist-get node :node_id)
         do (when (gethash node-id seen-ids)
              (signal 'gnosis-nodes-error '("Duplicate Gnosis catalog node")))
         do (puthash node-id t seen-ids)
         do (push node records))
        (if (eq next gnosis-core--json-null)
            (setq done t)
          (when (or (= (length items) 0)
                    (gethash next seen-cursors))
            (signal 'gnosis-nodes-error '("Gnosis node catalog did not advance")))
          (puthash next t seen-cursors)
          (setq after next))))
    (list :generation generation :nodes (vconcat (nreverse records)))))

(defun gnosis-nodes--display-text (value)
  "Return one-line completion text for VALUE."
  (string-trim
   (replace-regexp-in-string "[[:space:]\n\r]+" " " value)))

(defun gnosis-nodes--candidate-label (node)
  "Return NODE title path formatted for completion."
  (mapconcat #'gnosis-nodes--display-text
             (append (plist-get node :title_path) nil) "::"))

(defun gnosis-nodes--candidate-annotation (node)
  "Return filename and state annotation for NODE."
  (string-join
   (delq nil
         (list
          (file-name-nondirectory (plist-get node :logical_path))
          (unless (eq t (plist-get node :active)) "retired")))
   "  "))

(defun gnosis-nodes--candidate-counts (nodes)
  "Return occurrence counts for completion labels in NODES."
  (let ((counts (make-hash-table :test #'equal)))
    (cl-loop
     for node across nodes
     for label = (gnosis-nodes--candidate-label node)
     do (puthash label (1+ (gethash label counts 0)) counts))
    counts))

(defun gnosis-nodes--unique-candidate
    (label occurrence total by-candidate)
  "Return unused LABEL candidate at OCCURRENCE of TOTAL.
BY-CANDIDATE contains candidates already assigned."
  (let ((index occurrence)
        (candidate (if (= total 1) label
                     (format "%s [%d]" label occurrence))))
    (while (gethash candidate by-candidate)
      (setq index (1+ index)
            candidate (format "%s [%d]" label index)))
    candidate))

(defun gnosis-nodes--build-catalog (nodes authority &optional generation)
  "Build a catalog from NODES for AUTHORITY at optional GENERATION."
  (let ((by-id (make-hash-table :test #'equal))
        (by-candidate (make-hash-table :test #'equal))
        (annotations (make-hash-table :test #'equal))
        (label-counts (gnosis-nodes--candidate-counts nodes))
        (label-occurrences (make-hash-table :test #'equal))
        candidates)
    (cl-loop
     for node across nodes
     for node-id = (plist-get node :node_id)
     for label = (gnosis-nodes--candidate-label node)
     for occurrence = (1+ (gethash label label-occurrences 0))
     for candidate = (gnosis-nodes--unique-candidate
                      label occurrence (gethash label label-counts)
                      by-candidate)
     do (when (gethash node-id by-id)
          (signal 'gnosis-nodes-error '("Duplicate Gnosis catalog node")))
     do (puthash label occurrence label-occurrences)
     do (puthash node-id node by-id)
     do (puthash candidate node-id by-candidate)
     do (puthash candidate (gnosis-nodes--candidate-annotation node)
                 annotations)
     do (push candidate candidates))
    (list :authority authority
          :version gnosis-nodes--catalog-version
          :generation generation
          :nodes nodes
          :by-id by-id
          :candidates (vconcat (sort candidates #'string-lessp))
          :by-candidate by-candidate
          :annotations annotations)))

(defun gnosis-nodes--completion-table (catalog)
  "Return annotated completion table for node CATALOG."
  (let ((annotations (plist-get catalog :annotations))
        (candidates
         (cl-loop
          for candidate across (plist-get catalog :candidates)
          collect
          (cons candidate
                (gethash candidate (plist-get catalog :by-candidate))))))
    (lambda (string pred action)
      (if (eq action 'metadata)
          `(metadata
            (annotation-function
             . ,(lambda (candidate)
                  (when-let* ((annotation
                               (gethash candidate annotations)))
                    (concat "  " annotation)))))
        (complete-with-action action candidates string pred)))))

;;;###autoload
(defun gnosis-nodes-refresh-cache ()
  "Replace the disposable in-memory node catalog from the server."
  (interactive)
  (let* ((authority (gnosis-nodes--catalog-authority))
         (result (gnosis-nodes--fetch-node-records))
         (nodes (plist-get result :nodes))
         (catalog
          (gnosis-nodes--build-catalog
           nodes authority (plist-get result :generation))))
    (unless (equal authority (gnosis-nodes--catalog-authority))
      (signal 'gnosis-nodes-error
              '("Gnosis authenticated authority changed during refresh")))
    (setq gnosis-nodes--node-catalog catalog)
    (when (called-interactively-p 'interactive)
      (message "Cached %d Gnosis nodes" (length nodes)))
    catalog))

(defun gnosis-nodes--catalog (&optional refresh)
  "Return the current-authority node catalog, refreshing when needed.
When REFRESH is non-nil, always fetch a new catalog."
  (let ((authority (gnosis-nodes--catalog-authority)))
    (if (and (not refresh)
             gnosis-nodes--node-catalog
             (eql gnosis-nodes--catalog-version
                  (plist-get gnosis-nodes--node-catalog :version))
             (equal authority
                    (plist-get gnosis-nodes--node-catalog :authority)))
        gnosis-nodes--node-catalog
      (gnosis-nodes-refresh-cache))))

(defun gnosis-nodes--current-catalog-p (catalog authority)
  "Return non-nil when CATALOG belongs to current AUTHORITY and code."
  (and catalog
       (eql gnosis-nodes--catalog-version
            (plist-get catalog :version))
       (equal authority (plist-get catalog :authority))))

(defun gnosis-nodes--read-catalog-node-id (prompt)
  "Read a disambiguated node using PROMPT and return its opaque ID."
  (let* ((catalog (gnosis-nodes--catalog))
         (candidate
          (completing-read prompt (gnosis-nodes--completion-table catalog)
                           nil t))
         (node-id (gethash candidate (plist-get catalog :by-candidate))))
    (unless (and node-id
                 (equal (plist-get catalog :authority)
                        (gnosis-nodes--catalog-authority)))
      (signal 'gnosis-nodes-error
              '("Gnosis authority changed during node selection")))
    node-id))

(defun gnosis-nodes--changed-document-list ()
  "Return sorted document IDs recorded during the current synchronization."
  (let (ids)
    (when (hash-table-p gnosis-nodes--changed-document-ids)
      (maphash (lambda (document-id _)
                 (push document-id ids))
               gnosis-nodes--changed-document-ids))
    (sort ids #'string-lessp)))

(defun gnosis-nodes--incremental-catalog
    (catalog document-ids generation authority)
  "Replace DOCUMENT-IDS in CATALOG at replica GENERATION."
  (let ((changed (make-hash-table :test #'equal))
        fresh)
    (dolist (document-id document-ids)
      (puthash document-id t changed)
      (let ((result (gnosis-nodes--fetch-node-records document-id)))
        (unless (equal generation (plist-get result :generation))
          (signal 'gnosis-nodes-error
                  '("Gnosis node catalog changed after synchronization")))
        (setq fresh
              (nconc fresh
                     (append (plist-get result :nodes) nil)))))
    (unless (equal authority (gnosis-nodes--catalog-authority))
      (signal 'gnosis-nodes-error
              '("Gnosis authenticated authority changed during refresh")))
    (gnosis-nodes--build-catalog
     (vconcat
      (cl-loop
       for node across (plist-get catalog :nodes)
       unless (gethash (plist-get node :document_id) changed)
       collect node)
      fresh)
     authority generation)))

(defun gnosis-nodes--refresh-full-catalog-safely ()
  "Refresh the disposable catalog, clearing it when refresh fails."
  (condition-case err
      (gnosis-nodes-refresh-cache)
    (error
     (setq gnosis-nodes--node-catalog nil)
     (message "Gnosis node cache refresh failed: %s"
              (error-message-string err)))))

(defun gnosis-nodes--refresh-catalog-after-sync
    (document-ids starting-generation generation)
  "Refresh DOCUMENT-IDS from STARTING-GENERATION to GENERATION."
  (when gnosis-nodes--node-catalog
    (let* ((authority (gnosis-nodes--catalog-authority))
           (catalog gnosis-nodes--node-catalog))
      (when (gnosis-nodes--current-catalog-p catalog authority)
        (cond
         ((not (equal starting-generation
                      (plist-get catalog :generation)))
          (gnosis-nodes--refresh-full-catalog-safely))
         ((null document-ids)
          (setq gnosis-nodes--node-catalog
                (plist-put (copy-sequence catalog) :generation generation)))
         ((> (length document-ids)
             gnosis-nodes--catalog-incremental-limit)
          (gnosis-nodes--refresh-full-catalog-safely))
         (t
          (condition-case nil
              (setq gnosis-nodes--node-catalog
                    (gnosis-nodes--incremental-catalog
                     catalog document-ids generation authority))
            (error
             (gnosis-nodes--refresh-full-catalog-safely)))))))))

(defun gnosis-nodes--buffer-bytes (buffer)
  "Return BUFFER contents encoded as exact UTF-8 bytes."
  (with-current-buffer buffer
    (save-restriction
      (widen)
      (encode-coding-string
       (buffer-substring-no-properties (point-min) (point-max))
       'utf-8 t))))

(defun gnosis-nodes--exact-buffer-p (buffer digest)
  "Return non-nil when unmodified BUFFER has exact DIGEST."
  (and (buffer-live-p buffer)
       (not (buffer-modified-p buffer))
       (equal digest
              (gnosis-nodes--digest
               (gnosis-nodes--buffer-bytes buffer)))))

(defun gnosis-nodes--local-document-source (node)
  "Return an exact local document source for NODE, or nil."
  (let* ((path
          (gnosis-nodes--safe-path
           gnosis-nodes-dir (plist-get node :logical_path)))
         (digest (plist-get node :content_sha256))
         (visiting (get-file-buffer path)))
    (cond
     ((gnosis-nodes--exact-buffer-p visiting digest)
      (list :buffer visiting :bytes (gnosis-nodes--buffer-bytes visiting)))
     ((buffer-live-p visiting) nil)
     (t
      (let ((disk (gnosis-nodes--working-bytes path)))
        (when (and disk (equal digest (gnosis-nodes--digest disk)))
          (let ((opened (find-file-noselect path)))
            (if (gnosis-nodes--exact-buffer-p opened digest)
                (list :buffer opened :bytes
                      (gnosis-nodes--buffer-bytes opened))
              (unless (buffer-modified-p opened) (kill-buffer opened))
              nil))))))))

(defun gnosis-nodes--historical-buffer (node bytes)
  "Return a read-only Org buffer for exact NODE revision BYTES."
  (let* ((digest (plist-get node :content_sha256))
         (revision (plist-get node :document_revision))
         (base-name
          (format "*Gnosis document %s@%d*" (substring digest 0 12) revision))
         (existing (get-buffer base-name)))
    (if (and (gnosis-nodes--exact-buffer-p existing digest)
             (buffer-local-value 'buffer-read-only existing)
             (with-current-buffer existing (derived-mode-p 'org-mode)))
        existing
      (let ((buffer (generate-new-buffer base-name)))
        (condition-case condition
            (progn
              (with-current-buffer buffer
                (org-mode)
                (insert (gnosis-core--org-string bytes))
                (unless (equal digest
                               (gnosis-nodes--digest
                                (gnosis-nodes--buffer-bytes buffer)))
                  (signal 'gnosis-nodes-error
                          '("Org mode changed the Gnosis document")))
                (setq-local gnosis-nodes--content-sha256 digest)
                (setq-local gnosis-nodes--document-revision revision)
                (setq buffer-read-only t)
                (set-buffer-modified-p nil))
              buffer)
          (error
           (kill-buffer buffer)
           (signal (car condition) (cdr condition))))))))

(defun gnosis-nodes--document-source (node)
  "Return exact buffer and bytes for NODE's digest-bound revision."
  (or
   (gnosis-nodes--local-document-source node)
   (let* ((digest (plist-get node :content_sha256))
          (bytes
           (gnosis-core-document-revision
            (plist-get node :document_id)
            (plist-get node :document_revision)
            digest)))
     (unless (equal digest (gnosis-nodes--digest bytes))
       (signal 'gnosis-nodes-error '("Gnosis document digest mismatch")))
     (list :buffer (gnosis-nodes--historical-buffer node bytes)
           :bytes bytes))))

(defun gnosis-nodes--byte-position (bytes start end)
  "Convert zero-based half-open byte span START..END in BYTES to a point."
  (let ((raw (string-to-unibyte bytes)))
    (unless (and (natnump start) (natnump end)
                 (< start end) (<= end (length raw)))
      (signal 'gnosis-nodes-error '("Invalid Gnosis node source span")))
    (condition-case nil
        (progn
          (gnosis-core--org-string raw)
          (gnosis-core--org-string (substring raw 0 end))
          (1+ (length
               (gnosis-core--org-string (substring raw 0 start)))))
      (gnosis-core-validation-error
       (signal 'gnosis-nodes-error
               '("Gnosis node span is not on a UTF-8 boundary"))))))

(defun gnosis-nodes--legacy-position (node)
  "Return the current buffer position for legacy spanless NODE."
  (if (= 0 (plist-get node :level))
      (if (equal (plist-get node :node_id)
                 (org-entry-get (point-min) "ID"))
          (point-min)
        (signal 'gnosis-nodes-error
                '("Legacy Gnosis file ID is absent from its document")))
    (let ((position
           (save-excursion
             (save-restriction
               (widen)
               (org-find-property "ID" (plist-get node :node_id))))))
      (or position
          (signal 'gnosis-nodes-error
                  '("Legacy Gnosis node ID is absent from its document"))))))

(defun gnosis-nodes--node-position (node bytes)
  "Return NODE point in exact document BYTES."
  (unless (equal (plist-get node :content_sha256)
                 (gnosis-nodes--digest bytes))
    (signal 'gnosis-nodes-error '("Gnosis node document revision is stale")))
  (let ((start (plist-get node :start_byte))
        (end (plist-get node :end_byte)))
    (cond
     ((and (eq start gnosis-core--json-null)
           (eq end gnosis-core--json-null))
      (gnosis-nodes--legacy-position node))
     ((and (natnump start) (natnump end))
      (gnosis-nodes--byte-position bytes start end))
     (t
      (signal 'gnosis-nodes-error '("Invalid Gnosis node source span"))))))

(defun gnosis-nodes--visit-node (node)
  "Open NODE's exact document revision and move point to the node."
  (let* ((source (gnosis-nodes--document-source node))
         (buffer (plist-get source :buffer))
         (bytes (plist-get source :bytes))
         position)
    (with-current-buffer buffer
      (setq position (gnosis-nodes--node-position node bytes))
      (setq-local gnosis-nodes--content-sha256
                  (plist-get node :content_sha256))
      (setq-local gnosis-nodes--document-revision
                  (plist-get node :document_revision)))
    (pop-to-buffer-same-window buffer)
    (unless (equal (plist-get node :content_sha256)
                   (gnosis-nodes--digest
                    (gnosis-nodes--buffer-bytes buffer)))
      (signal 'gnosis-nodes-error
              '("Gnosis document changed before node navigation")))
    (widen)
    (goto-char position)
    (when (derived-mode-p 'org-mode)
      (org-fold-show-context 'org-goto))
    buffer))

(defun gnosis-nodes--new-filename (title)
  "Return a safe Org filename for new root node TITLE."
  (let ((name (replace-regexp-in-string
               "[#/\\\\]" ""
               (replace-regexp-in-string "[[:space:]]+" "_" title))))
    (when (string-empty-p name)
      (user-error "Node title must contain a filename character"))
    (format "%s.org"
            (if gnosis-nodes-timestring
                (format "%s--%s"
                        (format-time-string gnosis-nodes-timestring) name)
              name))))

(defun gnosis-nodes--create-root (title)
  "Create and save a synchronized root node named TITLE.
Return its Org ID."
  (make-directory gnosis-nodes-dir t)
  (let* ((logical-path (gnosis-nodes--new-filename title))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (existing (file-exists-p path))
         id)
    (when (and existing
               (null gnosis-nodes-timestring)
               (not (y-or-n-p
                     (format "Node file already exists: %s.  Use it? "
                             logical-path))))
      (user-error "Aborted: duplicate filename"))
    (save-window-excursion
      (let ((buffer (find-file-noselect path))
            (org-id-track-globally nil))
        (switch-to-buffer buffer)
        (with-current-buffer buffer
          (unless existing
            (insert (format "#+title: %s\n#+filetags: \n" title))
            (org-mode)
            (goto-char (point-min))
            (setq id (org-id-get-create))
            (gnosis-nodes-mode 1)
            (save-buffer))
          (setq id (or id (org-entry-get (point-min) "ID")))
          (unless id
            (user-error "Existing node file has no root ID")))))
    id))

;;;###autoload
(defun gnosis-nodes-insert (arg)
  "Insert an Org ID link to a selected Gnosis node.
With prefix ARG, prompt for a custom description.  An active region takes
precedence and becomes the description.  A new title creates a root node."
  (interactive "P")
  (let* ((catalog (gnosis-nodes--catalog))
         (candidate
          (completing-read "Select gnosis node: "
                           (gnosis-nodes--completion-table catalog)))
         (node-id (gethash candidate (plist-get catalog :by-candidate)))
         (node (and node-id (gethash node-id (plist-get catalog :by-id))))
         (title (if node (plist-get node :title) candidate)))
    (when (string-empty-p title)
      (user-error "Node title cannot be empty"))
    (unless (equal (plist-get catalog :authority)
                   (gnosis-nodes--catalog-authority))
      (signal 'gnosis-nodes-error
              '("Gnosis authority changed during node selection")))
    (let ((id (or node-id (gnosis-nodes--create-root title)))
          (description
           (cond ((use-region-p)
                  (buffer-substring-no-properties
                   (region-beginning) (region-end)))
                 (arg (read-string "Description: "))
                 (t title))))
      (org-insert-link nil (format "id:%s" id) description))))

(defun gnosis-nodes--expand-template (text &optional base-level)
  "Expand braced Org heading markers in TEXT from BASE-LEVEL.
Without BASE-LEVEL, start one level below the current heading."
  (let ((base-level (or base-level (1+ (or (org-current-level) 0)))))
    (replace-regexp-in-string
     "{\\(\\*+\\)}"
     (lambda (match)
       (make-string (+ base-level (1- (length (match-string 1 match)))) ?*))
     text t)))

;;;###autoload
(defun gnosis-nodes-insert-template ()
  "Insert a configured Org template at point and save the document."
  (interactive)
  (unless gnosis-nodes-templates
    (user-error "No Gnosis node templates are configured"))
  (let* ((name (if (= 1 (length gnosis-nodes-templates))
                   (caar gnosis-nodes-templates)
                 (completing-read "Template: " gnosis-nodes-templates nil t)))
         (source (alist-get name gnosis-nodes-templates nil nil #'equal))
         (text (if (functionp source) (funcall source) source)))
    (unless (stringp text)
      (user-error "Gnosis template did not produce text"))
    (insert (gnosis-nodes--expand-template text))
    (save-buffer)))

(defun gnosis-nodes--add-tags (tags)
  "Add TAGS at the current Org heading, or to file tags before headings."
  (if (org-before-first-heading-p)
      (save-excursion
        (goto-char (point-min))
        (let ((case-fold-search t) existing bounds)
          (when (re-search-forward
                 "^#\\+filetags:[[:blank:]]*\\(.*\\)$" nil t)
            (setq existing (split-string (match-string-no-properties 1) ":" t)
                  bounds (cons (line-beginning-position) (line-end-position))))
          (let* ((merged (delete-dups (append existing tags)))
                 (line (concat "#+filetags: :" (string-join merged ":") ":")))
            (if bounds
                (progn (delete-region (car bounds) (cdr bounds)) (insert line))
              (goto-char (point-min))
              (insert line "\n"))
            merged)))
    (let ((merged (delete-dups (append (org-get-tags nil t) tags))))
      (org-set-tags merged)
      merged)))

;;;###autoload
(defun gnosis-nodes-add-tags (tags)
  "Add TAGS at the current heading or file level and save the document."
  (interactive (list (completing-read-multiple "Tags: ")))
  (gnosis-nodes--add-tags tags)
  (save-buffer))

(defun gnosis-nodes--journal-logical-path (title)
  "Return the confined journal logical path for TITLE."
  (concat (file-name-as-directory gnosis-journal-directory) title ".org"))

(defun gnosis-nodes--journal-matches (query)
  "Return server journal nodes whose titles match QUERY."
  (let ((after nil)
        (prefix (file-name-as-directory gnosis-journal-directory))
        (seen (make-hash-table :test #'equal))
        (pages 0)
        matches done)
    (while (not done)
      (let* ((page (gnosis-core-nodes query after 100))
             (next (plist-get page :next)))
        (cl-incf pages)
        (dolist (node (append (plist-get page :items) nil))
          (when (and (string-prefix-p prefix (plist-get node :logical_path))
                     (zerop (plist-get node :level))
                     (string-match-p (regexp-quote query)
                                     (plist-get node :title)))
            (push node matches)))
        (cond
         ((eq next gnosis-core--json-null) (setq done t))
         ((or (gethash next seen) (>= pages 50))
          (signal 'gnosis-core-protocol-error
                  '("Invalid Gnosis journal pagination")))
         (t (puthash next t seen) (setq after next)))))
    (vconcat (nreverse matches))))

(defun gnosis-nodes--create-journal-entry (title)
  "Create, save, and visit a journal entry named TITLE."
  (let* ((logical-path (gnosis-nodes--journal-logical-path title))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (body (gnosis-nodes--expand-template gnosis-journal-template 1))
         id)
    (when (file-exists-p path)
      (user-error "Journal file exists but is not available from gnosisd"))
    (make-directory (file-name-directory path) t)
    (find-file path)
    (insert (format "#+title: %s\n#+filetags: :journal:\n\n%s" title body))
    (org-mode)
    (goto-char (point-min))
    (setq id (org-id-get-create))
    (gnosis-nodes-mode 1)
    (save-buffer)
    id))

;;;###autoload
(defun gnosis-journal ()
  "Open today's journal entry, creating and saving it when absent."
  (interactive)
  (let* ((title (format-time-string gnosis-journal-date-format))
         (logical-path (gnosis-nodes--journal-logical-path title))
         (matches
          (vconcat
           (seq-filter
            (lambda (node)
              (equal logical-path (plist-get node :logical_path)))
            (append (gnosis-nodes--journal-matches title) nil)))))
    (pcase (length matches)
      (0 (gnosis-nodes--create-journal-entry title))
      (1 (gnosis-nodes--visit-node (aref matches 0)))
      (_ (signal 'gnosis-nodes-error '("Duplicate Gnosis journal entries"))))))

;;;###autoload
(defun gnosis-journal-find (query)
  "Find and visit a journal entry matching QUERY."
  (interactive "sJournal search: ")
  (let ((matches (gnosis-nodes--journal-matches query)))
    (unless (> (length matches) 0)
      (user-error "No matching Gnosis journal entry"))
    (let* ((table
            (mapcar (lambda (node)
                      (cons (plist-get node :title) node))
                    (append matches nil)))
           (choice (completing-read "Journal: " table nil t)))
      (gnosis-nodes--visit-node (alist-get choice table nil nil #'equal)))))

;;;###autoload
(defun gnosis-journal-insert ()
  "Insert an Org ID link to a selected journal entry."
  (interactive)
  (let* ((query (read-string "Journal search: "))
         (matches (gnosis-nodes--journal-matches query)))
    (unless (> (length matches) 0)
      (user-error "No matching Gnosis journal entry"))
    (let* ((table
            (mapcar (lambda (node)
                      (cons (plist-get node :title) node))
                    (append matches nil)))
           (choice (completing-read "Journal: " table nil t))
           (node (alist-get choice table nil nil #'equal)))
      (org-insert-link nil (format "id:%s" (plist-get node :node_id)) choice))))

(defun gnosis-nodes--literal-count (string)
  "Return the number of literal STRING occurrences in the current buffer."
  (save-excursion
    (goto-char (point-min))
    (let ((count 0) (case-fold-search nil))
      (while (search-forward string nil t)
        (when (gnosis-nodes--plain-text-range-p
               (match-beginning 0) (match-end 0))
          (cl-incf count)))
      count)))

(defun gnosis-nodes--plain-text-at-p (position)
  "Return non-nil when POSITION is replaceable plain Org text."
  (save-excursion
    (goto-char position)
    (pcase (org-element-type (org-element-context))
      ((or 'paragraph 'table-cell) t)
      ('headline
       (beginning-of-line)
       (and (looking-at org-complex-heading-regexp)
            (match-beginning 4)
            (<= (match-beginning 4) position)
            (< position (match-end 4)))))))

(defun gnosis-nodes--plain-text-range-p (begin end)
  "Return non-nil when every character from BEGIN to END is plain Org text."
  (cl-loop for position from begin below end
           always (gnosis-nodes--plain-text-at-p position)))

(defun gnosis-nodes--bulk-target-buffer (target)
  "Return TARGET's confined regular Org buffer."
  (let* ((path (if (bufferp target)
                   (buffer-file-name target)
                 (expand-file-name target)))
         (root (file-name-as-directory (file-truename gnosis-nodes-dir))))
    (unless (and path
                 (string-match-p "\\.org\\'" path)
                 (not (file-symlink-p path))
                 (file-regular-p path)
                 (file-in-directory-p (file-truename path) root))
      (user-error "Bulk link target is not a confined regular Org file"))
    (if (bufferp target) target (find-file-noselect path))))

;;;###autoload
(defun gnosis-bulk-link-string (string node-id targets)
  "Replace literal STRING with NODE-ID links in explicit finite TARGETS.
TARGETS contains buffers or filenames.  Preview the occurrence count, require
confirmation, save each changed document, stop on the first error, and return
the changed targets."
  (interactive
   (let* ((string (read-string "Literal string: "))
          (node-id (gnosis-nodes--read-catalog-node-id "Node: "))
          (files
           (completing-read-multiple
            "Org files: "
            (directory-files-recursively gnosis-nodes-dir "\\.org\\'")
            nil t)))
     (list string node-id files)))
  (unless (and (stringp string) (not (string-empty-p string)))
    (user-error "Bulk link string must be non-empty"))
  (when (> (length string) gnosis-nodes-max-bulk-link-string-length)
    (user-error "Bulk link string exceeds %d characters"
                gnosis-nodes-max-bulk-link-string-length))
  (let* ((buffers
          (mapcar #'gnosis-nodes--bulk-target-buffer targets))
         (count
          (apply #'+ (mapcar (lambda (buffer)
                               (with-current-buffer buffer
                                 (gnosis-nodes--literal-count string)))
                             buffers))))
    (cond
     ((= count 0) (message "No literal matches") nil)
     ((not (y-or-n-p (format "Replace %d literal occurrence%s? "
                             count (if (= count 1) "" "s"))))
      nil)
     (t
      (let (changed)
        (cl-mapc
         (lambda (target buffer)
           (with-current-buffer buffer
             (goto-char (point-min))
             (let ((case-fold-search nil) found)
               (while (search-forward string nil t)
                 (let ((begin (match-beginning 0))
                       (end (match-end 0)))
                   (when (gnosis-nodes--plain-text-range-p begin end)
                     (delete-region begin end)
                     (org-insert-link nil (format "id:%s" node-id) string)
                     (setq found t))))
               (when found
                 (save-buffer)
                 (push target changed)))))
         targets buffers)
        (nreverse changed))))))

;;;###autoload
(defun gnosis-nodes-find (&optional refresh)
  "Select a server node by name and visit its exact document location.
With prefix argument REFRESH, rebuild the node catalog first."
  (interactive "P")
  (let* ((catalog (gnosis-nodes--catalog refresh))
         (candidates (plist-get catalog :candidates)))
    (when (= 0 (length candidates))
      (user-error "The Gnosis node catalog is empty"))
    (let* ((candidate
            (completing-read
             "Gnosis node: "
             (gnosis-nodes--completion-table catalog) nil t))
           (node-id
            (gethash candidate (plist-get catalog :by-candidate))))
      (unless node-id
        (signal 'gnosis-nodes-error '("Unknown Gnosis node selection")))
      (unless (equal (plist-get catalog :authority)
                     (gnosis-nodes--catalog-authority))
        (signal 'gnosis-nodes-error
                '("Gnosis authority changed during node selection")))
      (let ((node (gnosis-core-node node-id)))
        (unless (equal (plist-get catalog :authority)
                       (gnosis-nodes--catalog-authority))
          (signal 'gnosis-nodes-error
                  '("Gnosis authority changed during node lookup")))
        (gnosis-nodes--visit-node node)))))

(keymap-popup-define gnosis-nodes-page-mode-map
  "Keymap for server-backed Gnosis node pages."
  :parent tabulated-list-mode-map
  :popup-key "?"
  :exit-key "C-g"
  :description "Gnosis Nodes"
  :group "Open"
  "RET" ("Visit node" gnosis-nodes-page-visit)
  :group "Page"
  "n" ("Next page" gnosis-nodes-page-next)
  "p" ("Previous page" gnosis-nodes-page-previous)
  "g" ("Refresh page" gnosis-nodes-page-refresh :stay-open t)
  "q" ("Quit" quit-window))

(define-derived-mode gnosis-nodes-page-mode tabulated-list-mode "Gnosis Nodes"
  "Major mode for one page of server-backed node results."
  (display-line-numbers-mode -1)
  (setq-local truncate-lines t
              tabulated-list-padding 2
              tabulated-list-format
              [("Title" 36 t) ("Path" 32 t) ("Tags" 24 t)])
  (tabulated-list-init-header))

(defun gnosis-nodes-page--entry (record)
  "Return a tabulated entry for node or content-hit RECORD."
  (let ((identity (or (plist-get record :node_id)
                      (plist-get record :document_id))))
    (list identity
          (vector (or (plist-get record :title) "Document content match")
                  (or (plist-get record :logical_path) "")
                  (mapconcat #'identity
                             (append (or (plist-get record :tags) []) nil)
                             ", ")))))

(defun gnosis-nodes-page--install (page cursor)
  "Install fetched PAGE for CURSOR in the current result buffer."
  (let* ((items (plist-get page :items))
         (records (make-hash-table :test #'equal)))
    (cl-loop for record across items
             do (puthash (or (plist-get record :node_id)
                             (plist-get record :document_id))
                         record records))
    (setq gnosis-nodes-page--cursor cursor
          gnosis-nodes-page--next (plist-get page :next)
          gnosis-nodes-page--records records
          tabulated-list-entries
          (cl-loop for record across items
                   collect (gnosis-nodes-page--entry record)))
    (tabulated-list-print t)
    (goto-char (point-min))))

(defun gnosis-nodes-page--load (cursor)
  "Load CURSOR through the current page fetcher."
  (unless gnosis-nodes-page--fetch
    (user-error "No Gnosis page source"))
  (gnosis-nodes-page--install
   (funcall gnosis-nodes-page--fetch cursor) cursor))

(defun gnosis-nodes-page-open (title fetch)
  "Open a node page named TITLE using single-argument FETCH."
  (let ((buffer (get-buffer-create (format "*Gnosis Nodes: %s*" title))))
    (pop-to-buffer-same-window buffer)
    (gnosis-nodes-page-mode)
    (setq-local gnosis-nodes-page--fetch fetch
                gnosis-nodes-page--history nil)
    (gnosis-nodes-page--load nil)
    (keymap-popup gnosis-nodes-page-mode-map)))

(defun gnosis-nodes-page-next ()
  "Open the next server result page."
  (interactive)
  (when (eq gnosis-nodes-page--next gnosis-core--json-null)
    (user-error "No next Gnosis node page"))
  (let* ((cursor gnosis-nodes-page--next)
         (page (funcall gnosis-nodes-page--fetch cursor))
         (previous gnosis-nodes-page--cursor))
    (gnosis-nodes-page--install page cursor)
    (push previous gnosis-nodes-page--history)))

(defun gnosis-nodes-page-previous ()
  "Return to the previous result page fetched in this buffer."
  (interactive)
  (unless gnosis-nodes-page--history
    (user-error "No previous Gnosis node page"))
  (let* ((cursor (car gnosis-nodes-page--history))
         (page (funcall gnosis-nodes-page--fetch cursor)))
    (gnosis-nodes-page--install page cursor)
    (pop gnosis-nodes-page--history)))

(defun gnosis-nodes-page-refresh ()
  "Refresh the current result page."
  (interactive)
  (gnosis-nodes-page--load gnosis-nodes-page--cursor))

(defun gnosis-nodes-page-visit ()
  "Visit the node or content-hit document at point."
  (interactive)
  (let* ((record (gethash (tabulated-list-get-id) gnosis-nodes-page--records))
         (node-id (or (plist-get record :node_id)
                      (let ((id (plist-get record :file_node_id)))
                        (unless (eq id gnosis-core--json-null) id)))))
    (if node-id
        (gnosis-nodes--visit-node (gnosis-core-node node-id))
      (let* ((node (list :document_id (plist-get record :document_id)
                         :document_revision (plist-get record :revision)
                         :content_sha256 (plist-get record :content_digest)
                         :logical_path (plist-get record :logical_path)))
             (source (gnosis-nodes--document-source node)))
        (pop-to-buffer-same-window (plist-get source :buffer))
        (goto-char (point-min))))))

;;;###autoload
(defun gnosis-nodes-search (query)
  "Open page-oriented server node-title results for QUERY."
  (interactive "sNode title: ")
  (gnosis-nodes-page-open
   (format "Title %s" query)
   (lambda (after) (gnosis-core-nodes query after 100))))

;;;###autoload
(defun gnosis-nodes-search-content (query)
  "Open page-oriented server document-content results for QUERY."
  (interactive "sDocument content: ")
  (gnosis-nodes-page-open
   (format "Content %s" query)
   (lambda (cursor) (gnosis-core-node-content-search query cursor 100))))

;;;###autoload
(defun gnosis-nodes-find-tag (tag)
  "Open page-oriented server node results for exact TAG."
  (interactive "sExact tag: ")
  (gnosis-nodes-page-open
   (format "Tag %s" tag)
   (lambda (after) (gnosis-core-nodes-by-tag tag after 100))))

(defun gnosis-nodes--graph (node-id direction)
  "Open NODE-ID graph page in DIRECTION."
  (gnosis-nodes-page-open
   (format "%s %s" (capitalize (substring (symbol-name direction) 1)) node-id)
   (lambda (after) (gnosis-core-node-graph node-id direction after 100))))

;;;###autoload
(defun gnosis-nodes-inbound (&optional node-id)
  "Open inbound links for NODE-ID or the current Org node."
  (interactive)
  (gnosis-nodes--graph
   (or node-id (org-id-get nil) (user-error "No Gnosis node at point"))
   :inbound))

;;;###autoload
(defun gnosis-nodes-outbound (&optional node-id)
  "Open outbound links for NODE-ID or the current Org node."
  (interactive)
  (gnosis-nodes--graph
   (or node-id (org-id-get nil) (user-error "No Gnosis node at point"))
   :outbound))

;;;###autoload
(defun gnosis-nodes-goto-id (node-id)
  "Visit authoritative NODE-ID detail."
  (interactive
   (list
    (or (when-let* ((context (org-element-context))
                    (type (org-element-type context))
                    ((eq type 'link))
                    ((equal "id" (org-element-property :type context))))
          (org-element-property :path context))
        (read-string "Gnosis node ID: "))))
  (gnosis-nodes--visit-node (gnosis-core-node node-id)))

(defalias 'gnosis-nodes-find-by-tag #'gnosis-nodes-find-tag)
(defalias 'gnosis-nodes-visit-backlinks #'gnosis-nodes-inbound)

(defun gnosis-nodes--nullable (value)
  "Return nil for protocol null VALUE, otherwise VALUE."
  (unless (eq value gnosis-core--json-null) value))

(defun gnosis-nodes--plan-document (base local remote active)
  "Plan reconciliation from BASE, LOCAL, REMOTE digests and ACTIVE state."
  (cond
   ((not active)
    (list :action (if local :conflict :keep)))
   ((equal local base)
    (list :action (if (equal remote base) :keep :materialize)))
   ((equal remote base) '(:action :keep))
   ((equal local remote) '(:action :keep))
   (t '(:action :conflict))))

(cl-defun gnosis-nodes--outbox-entry
    (kind document-id revision logical-path digest
          &optional previous-path payload)
  "Return durable pending operation for KIND and DOCUMENT-ID metadata.
REVISION, LOGICAL-PATH, DIGEST, and PREVIOUS-PATH describe expected state.
PAYLOAD captures exact bytes at enqueue time."
  (list :operation-id (gnosis-core-operation-id)
        :kind kind
        :document-id document-id
        :revision revision
        :logical-path logical-path
        :previous-path previous-path
        :digest digest
        :payload (and payload
                      (base64-encode-string (string-to-unibyte payload) t))
        :status :pending))

(defun gnosis-nodes--outbox-mark (entry status)
  "Return copy of outbox ENTRY with STATUS and stable operation identity."
  (plist-put (copy-sequence entry) :status status))

(defun gnosis-nodes--normalize-state (state)
  "Return STATE with symbolic outbox fields restored after JSON decoding."
  (let ((outbox (plist-get state :outbox)))
    (when (vectorp outbox)
      (dotimes (index (length outbox))
        (let ((entry (aref outbox index)))
          (dolist (key '(:kind :status :resolution))
            (when-let* ((value (plist-get entry key))
                        ((stringp value)))
              (setf (plist-get entry key) (intern (concat ":" value)))))
          (when (eq :sending (plist-get entry :status))
            (setf (plist-get entry :status) :uncertain)))))
    (let ((conflicts (plist-get state :conflicts)))
      (when (vectorp conflicts)
        (dotimes (index (length conflicts))
          (let ((entry (aref conflicts index)))
            (dolist (key '(:kind :status :resolution))
              (when-let* ((value (plist-get entry key))
                          ((stringp value)))
                (setf (plist-get entry key)
                      (intern (concat ":" value))))))))))
  (when-let* ((snapshot (plist-get state :snapshot)))
    (dolist (field '((:prepared-outbox :kind :status :resolution)
                     (:prepared-conflicts :kind :status :resolution)))
      (let ((entries (plist-get snapshot (car field))))
        (when (vectorp entries)
          (dotimes (index (length entries))
            (let ((entry (aref entries index)))
              (dolist (key (cdr field))
                (when-let* ((value (plist-get entry key))
                            ((stringp value)))
                  (setf (plist-get entry key)
                        (intern (concat ":" value)))))))))))
  state)

(defun gnosis-nodes--notes-root ()
  "Return canonical configured notes root."
  (file-truename (file-name-as-directory gnosis-nodes-dir)))

(defun gnosis-nodes--service-authority ()
  "Return canonical configured Gnosis service authority."
  (gnosis-core--validated-base-url))

(defun gnosis-nodes--read-state ()
  "Read and validate minimal durable synchronization state."
  (let ((file (gnosis-nodes--state-file)))
    (when (file-symlink-p file)
      (signal 'gnosis-nodes-path-error (list file)))
    (unless (file-readable-p file)
      (signal 'gnosis-nodes-error '("Gnosis synchronization is not bootstrapped")))
    (let* ((bytes (gnosis-nodes--managed-bytes
                   (file-name-directory file) file))
           (text (decode-coding-string bytes 'utf-8)))
      (unless (equal bytes (encode-coding-string text 'utf-8 t))
        (signal 'gnosis-nodes-error '("Invalid UTF-8 synchronization state")))
      (condition-case nil
          (with-temp-buffer
            (insert text)
            (goto-char (point-min))
            (let ((state
                   (json-parse-buffer
                    :object-type 'plist :array-type 'array
                    :null-object nil :false-object gnosis-core--json-false)))
              (unless (and (= (or (plist-get state :version) -1) 1)
                           (stringp (plist-get state :server-id))
                           (stringp (plist-get state :device-id))
                           (stringp (plist-get state :notes-root))
                           (stringp (plist-get state :service-authority))
                           (stringp (plist-get state :cursor))
                           (vectorp (plist-get state :bases))
                           (vectorp (plist-get state :outbox))
                           (vectorp (plist-get state :conflicts)))
                (signal 'gnosis-nodes-error
                        '("Invalid Gnosis synchronization state")))
              (unless (and (equal (plist-get state :notes-root)
                                  (gnosis-nodes--notes-root))
                           (equal (plist-get state :service-authority)
                                  (gnosis-nodes--service-authority)))
                (signal 'gnosis-nodes-error
                        '("Gnosis notes root or service authority changed")))
              (gnosis-nodes--normalize-state state)))
        (json-error
         (signal 'gnosis-nodes-error
                 '("Invalid Gnosis synchronization state")))))))

(defun gnosis-nodes--write-state (state)
  "Atomically persist minimal synchronization STATE."
  (let* ((file (gnosis-nodes--state-file))
         (directory (file-name-directory file)))
    (when (or (file-symlink-p file)
              (file-symlink-p (directory-file-name directory)))
      (signal 'gnosis-nodes-path-error (list file)))
    (make-directory directory t)
    (set-file-modes directory #o700)
    (let ((temporary (make-temp-file (expand-file-name ".state-" directory))))
      (unwind-protect
          (progn
            (let* ((write-region-inhibit-fsync nil)
                   (coding-system-for-write 'binary)
                   (bytes
                    (encode-coding-string
                     (json-serialize
                      (gnosis-nodes--json-state state)
                      :null-object nil
                      :false-object gnosis-core--json-false)
                     'utf-8 t)))
              (write-region bytes nil temporary nil 'silent))
            (set-file-modes temporary #o600)
            (rename-file temporary file t))
        (when (file-exists-p temporary)
          (delete-file temporary))))))

(defun gnosis-nodes--json-value (value)
  "Return VALUE with protocol nulls converted recursively for JSON."
  (cond
   ((eq value gnosis-core--json-null) nil)
   ((vectorp value) (vconcat (mapcar #'gnosis-nodes--json-value value)))
   ((consp value) (mapcar #'gnosis-nodes--json-value value))
   (t value)))

(defun gnosis-nodes--canonical-value (value)
  "Return VALUE with recursively sorted plist keys."
  (cond
   ((vectorp value)
    (vconcat (mapcar #'gnosis-nodes--canonical-value value)))
   ((and (consp value) (keywordp (car value)))
    (apply #'append
           (mapcar (lambda (pair)
                     (list (car pair)
                           (gnosis-nodes--canonical-value (cdr pair))))
                   (sort (cl-loop for (key item) on value by #'cddr
                                  collect (cons key item))
                         (lambda (left right)
                           (string< (symbol-name (car left))
                                    (symbol-name (car right))))))))
   ((consp value) (mapcar #'gnosis-nodes--canonical-value value))
   (t value)))

(defun gnosis-nodes--projection-fingerprint (projection)
  "Return canonical SHA-256 fingerprint for PROJECTION."
  (secure-hash
   'sha256
   (json-serialize
    (gnosis-nodes--json-value (gnosis-nodes--canonical-value projection))
    :null-object nil
    :false-object gnosis-core--json-false)))

(defun gnosis-nodes--json-state (state)
  "Return copy of STATE containing only JSON values."
  (let ((copy (copy-tree state t)))
    (setq copy (plist-put copy :notes-root (gnosis-nodes--notes-root))
          copy (plist-put copy :service-authority
                          (gnosis-nodes--service-authority)))
    (let ((outbox (plist-get copy :outbox)))
      (when (vectorp outbox)
        (dotimes (index (length outbox))
          (let ((entry (aref outbox index)))
            (dolist (key '(:kind :status :resolution))
              (when-let* ((value (plist-get entry key))
                          ((keywordp value)))
                (setf (plist-get entry key)
                      (substring (symbol-name value) 1))))))))
    (let ((conflicts (plist-get copy :conflicts)))
      (when (vectorp conflicts)
        (dotimes (index (length conflicts))
          (let ((entry (aref conflicts index)))
            (dolist (key '(:kind :status :resolution))
              (when-let* ((value (plist-get entry key))
                          ((keywordp value)))
                (setf (plist-get entry key)
                      (substring (symbol-name value) 1))))))))
    (when-let* ((snapshot (plist-get copy :snapshot)))
      (dolist (field '((:prepared-outbox :kind :status :resolution)
                       (:prepared-conflicts :kind :status :resolution)))
        (let ((entries (plist-get snapshot (car field))))
          (when (vectorp entries)
            (dotimes (index (length entries))
              (let ((entry (aref entries index)))
                (dolist (key (cdr field))
                  (when-let* ((value (plist-get entry key))
                              ((keywordp value)))
                    (setf (plist-get entry key)
                          (substring (symbol-name value) 1))))))))))
    (gnosis-nodes--json-value copy)))

(defun gnosis-nodes--base (state &optional document-id logical-path)
  "Return base in STATE matching DOCUMENT-ID or LOGICAL-PATH."
  (seq-find
   (lambda (base)
     (or (and document-id
              (equal document-id (plist-get base :document-id)))
         (and logical-path
              (equal logical-path (plist-get base :logical-path)))))
   (append (plist-get state :bases) nil)))

(defun gnosis-nodes--replace-vector-item (vector old new)
  "Return VECTOR with OLD replaced by NEW, or NEW appended."
  (vconcat (if old (delq old (append vector nil)) (append vector nil))
           (vector new)))

(defun gnosis-nodes--projection-digest (projection)
  "Return authoritative content digest from document PROJECTION."
  (gnosis-nodes--nullable
   (if (equal "snapshot" (plist-get projection :operation))
       (plist-get (plist-get projection :projection) :content_sha256)
     (plist-get projection :content_sha256))))

(defun gnosis-nodes--record-base (state projection)
  "Record authoritative document PROJECTION in STATE."
  (let* ((document-id (plist-get projection :document_id))
         (old (gnosis-nodes--base state document-id))
         (base (list :document-id document-id
                     :revision (plist-get projection :revision)
                     :logical-path (plist-get projection :logical_path)
                     :digest (gnosis-nodes--projection-digest projection)
                     :fingerprint (gnosis-nodes--projection-fingerprint projection)
                     :active (not (equal "tombstone"
                                         (plist-get projection :operation))))))
    (setq state
          (plist-put state :bases
                     (gnosis-nodes--replace-vector-item
                      (plist-get state :bases) old base)))
    (let ((conflicts
           (seq-remove
            (lambda (conflict)
              (or
               (and (equal document-id (plist-get conflict :document-id))
                    (equal (plist-get base :digest)
                           (plist-get conflict :accepted-digest))
                    (> (plist-get base :revision)
                       (or (plist-get conflict :remote-revision) -1)))
               (when-let* ((operation-id (plist-get conflict :operation-id))
                           (entry
                            (seq-find
                             (lambda (item)
                               (and (eq :applied (plist-get item :status))
                                    (equal operation-id
                                           (plist-get item :operation-id))))
                             (append (plist-get state :outbox) nil)))
                           (authoritative
                            (plist-get (plist-get entry :result) :authoritative)))
                 (and (equal document-id
                             (plist-get authoritative :document_id))
                      (= (plist-get projection :revision)
                         (plist-get authoritative :revision))))))
            (append (plist-get state :conflicts) nil))))
      (setf (plist-get state :conflicts) (vconcat conflicts)))
    (setq state (gnosis-nodes--promote-outbox state))
    (let ((outbox (plist-get state :outbox)))
      (plist-put
       state :outbox
       (vconcat
        (seq-remove
         (lambda (entry)
           (let* ((result (plist-get entry :result))
                  (authoritative (plist-get result :authoritative))
                  (entry-document-id
                   (or (plist-get entry :document-id)
                       (plist-get authoritative :document_id)))
                  (entry-revision
                   (or (plist-get result :revision)
                       (plist-get authoritative :revision)
                       0)))
             (and (eq :applied (plist-get entry :status))
                  (equal document-id entry-document-id)
                  (<= entry-revision (plist-get base :revision)))))
         outbox))))))

(defun gnosis-nodes--conflict-directory (document-id)
  "Return private sidecar directory for DOCUMENT-ID."
  (expand-file-name
   (concat "conflicts/" (secure-hash 'sha256 document-id))
   (gnosis-nodes--state-directory)))

(defun gnosis-nodes--record-conflict (state base projection local-bytes remote-bytes)
  "Persist conflict sidecars and return updated STATE.
BASE and PROJECTION identify versions; LOCAL-BYTES remains in its working file.
REMOTE-BYTES contains validated authoritative content, or nil for deletion."
  (let* ((document-id (plist-get projection :document_id))
         (existing
          (seq-find (lambda (item)
                      (equal document-id (plist-get item :document-id)))
                    (append (plist-get state :conflicts) nil)))
         (directory (gnosis-nodes--conflict-directory document-id))
         (base-file (expand-file-name "base.org" directory))
         (remote-file (expand-file-name "remote.org" directory))
         (local-path (or (and base (plist-get base :logical-path))
                         (plist-get projection :logical_path)))
         (kind (cond
                ((equal "tombstone" (plist-get projection :operation)) :tombstone)
                ((null local-bytes) :missing-local)
                ((not (equal local-path (plist-get projection :logical_path)))
                 :rename)
                (t :content)))
         (record (list :document-id document-id
                       :kind kind
                       :local-path local-path
                       :logical-path (plist-get projection :logical_path)
                       :base-revision (and base (plist-get base :revision))
                       :local-digest (and local-bytes
                                          (gnosis-nodes--digest local-bytes))
                       :remote-digest (gnosis-nodes--projection-digest projection)
                       :remote-revision (plist-get projection :revision)
                       :projection projection
                       :base-file base-file :remote-file remote-file)))
    (when (and existing (eq :queued (plist-get existing :status)))
      (setq record (plist-put record :status :queued)
            record (plist-put record :operation-id
                              (plist-get existing :operation-id))))
    (unless gnosis-nodes--preflight
      (make-directory directory t)
      (gnosis-nodes--write-bytes
       base-file
       (if-let* ((digest (plist-get base :digest)))
           (or (cdr (assoc digest gnosis-nodes--conflict-base-blobs))
               (gnosis-core-blob digest))
         ""))
      (gnosis-nodes--write-bytes remote-file (or remote-bytes "")))
    (when (and gnosis-nodes--preflight (plist-get base :digest)
               (not (assoc (plist-get base :digest)
                           gnosis-nodes--conflict-base-blobs)))
      (let ((digest (plist-get base :digest)))
        (push (cons digest (gnosis-core-blob digest))
              gnosis-nodes--conflict-base-blobs)))
    (plist-put state :conflicts
               (vconcat
                (seq-remove (lambda (item)
                              (equal document-id (plist-get item :document-id)))
                            (append (plist-get state :conflicts) nil))
                (vector record)))))

(defun gnosis-nodes--accepted-remote-effect-p (record projection)
  "Return non-nil when RECORD's remote PROJECTION is already materialized."
  (and
   (eq :accept-remote (plist-get record :resolution))
   (let* ((operation (plist-get projection :operation))
         (remote-path (plist-get projection :logical_path))
         (local-path (plist-get record :local-path))
         (destination (gnosis-nodes--safe-path gnosis-nodes-dir remote-path))
         (destination-buffer (get-file-buffer destination))
         (remote-bytes (gnosis-nodes--working-bytes destination))
         (remote-digest (gnosis-nodes--projection-digest projection)))
    (if (equal operation "tombstone")
        (let ((backup (expand-file-name
                       "local-backup.org"
                       (gnosis-nodes--conflict-directory
                        (plist-get record :document-id)))))
          (and (null remote-bytes)
               (equal (plist-get record :local-digest)
                      (gnosis-nodes--digest
                       (gnosis-nodes--managed-bytes
                        (gnosis-nodes--conflict-directory
                         (plist-get record :document-id))
                        backup)))))
      (and (not (buffer-live-p destination-buffer))
           remote-bytes
           (equal remote-digest (gnosis-nodes--digest remote-bytes))
           (or (equal local-path remote-path)
               (null (gnosis-nodes--working-bytes
                      (gnosis-nodes--safe-path gnosis-nodes-dir local-path)))))))))

(defun gnosis-nodes--authoritative-confirms-projection-p (authoritative projection)
  "Return non-nil when AUTHORITATIVE exactly confirms PROJECTION."
  (let ((active (not (equal "tombstone"
                            (plist-get projection :operation)))))
    (and authoritative projection
         (equal (plist-get authoritative :document_id)
                (plist-get projection :document_id))
         (= (plist-get authoritative :revision)
            (plist-get projection :revision))
         (equal (plist-get authoritative :logical_path)
                (plist-get projection :logical_path))
         (equal (gnosis-nodes--nullable
                 (plist-get authoritative :content_sha256))
                (gnosis-nodes--projection-digest projection))
         (eq (eq t (plist-get authoritative :active)) active))))

(defun gnosis-nodes--confirmation-for-projection (state projection)
  "Return applied STATE entry that exactly confirms PROJECTION."
  (seq-find
   (lambda (entry)
     (and (eq :applied (plist-get entry :status))
          (gnosis-nodes--authoritative-confirms-projection-p
           (plist-get (plist-get entry :result) :authoritative)
           projection)))
   (append (plist-get state :outbox) nil)))

(defun gnosis-nodes--accepted-merge-effect-p (record projection)
  "Return non-nil when accepted merge RECORD exactly materializes PROJECTION."
  (let* ((logical-path (plist-get projection :logical_path))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (buffer (get-file-buffer path))
         (bytes (gnosis-nodes--working-bytes path))
         (digest (gnosis-nodes--projection-digest projection)))
    (and (eq :accept-merged (plist-get record :resolution))
         (equal digest (plist-get record :accepted-digest))
         (not (and (buffer-live-p buffer) (buffer-modified-p buffer)))
         bytes
         (equal digest (gnosis-nodes--digest bytes)))))

(defun gnosis-nodes--apply-document (state projection &optional blobs)
  "Reconcile document PROJECTION into working files and return STATE."
  (let* ((document-id (plist-get projection :document_id))
         (logical-path (plist-get projection :logical_path))
         (conflict
          (seq-find (lambda (item)
                      (equal document-id (plist-get item :document-id)))
                    (append (plist-get state :conflicts) nil)))
         (remote-revision (plist-get projection :revision))
         (_conflict-head
          (when conflict
            (let ((conflict-revision (plist-get conflict :remote-revision))
                  (conflict-projection (plist-get conflict :projection)))
              (when (or (< remote-revision conflict-revision)
                        (and (= remote-revision conflict-revision)
                             (not
                              (equal
                               (gnosis-nodes--projection-fingerprint projection)
                               (and conflict-projection
                                    (gnosis-nodes--projection-fingerprint
                                     conflict-projection))))))
                (signal 'gnosis-nodes-error
                        '("Gnosis conflict head changed projection"))))))
         (remote-digest (gnosis-nodes--projection-digest projection))
         (active (not (equal "tombstone" (plist-get projection :operation))))
         (base (gnosis-nodes--base state document-id logical-path))
         (_identity
          (when (and base
                     (not (equal document-id
                                 (plist-get base :document-id))))
            (signal 'gnosis-nodes-error
                    '("Gnosis logical path changed document identity"))))
         (_stale
          (when (and base (< remote-revision (plist-get base :revision)))
            (signal 'gnosis-nodes-error
                    '("Stale Gnosis document projection"))))
         (base-digest (and base (plist-get base :digest)))
         (base-path (and base (plist-get base :logical-path)))
         (working-path (or base-path logical-path))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir working-path))
         (local-bytes (gnosis-nodes--working-bytes path))
         (local-digest (and local-bytes (gnosis-nodes--digest local-bytes)))
         (fingerprint (gnosis-nodes--projection-fingerprint projection))
         (confirmation
          (gnosis-nodes--confirmation-for-projection state projection)))
    (cond
     ((and conflict confirmation
           (equal (plist-get conflict :operation-id)
                  (plist-get confirmation :operation-id))
           (gnosis-nodes--accepted-merge-effect-p conflict projection))
      (gnosis-nodes--record-base state projection))
     ((and conflict confirmation
           (equal (plist-get conflict :operation-id)
                  (plist-get confirmation :operation-id))
           (gnosis-nodes--accepted-remote-effect-p conflict projection))
      (gnosis-nodes--finish-accept-remote state conflict projection))
     ((and conflict
           (> (plist-get projection :revision)
              (plist-get conflict :remote-revision)))
      (gnosis-nodes--record-conflict
       state base projection local-bytes
       (and remote-digest
            (or (cdr (assoc remote-digest blobs))
                (gnosis-core-blob remote-digest)))))

     ((and conflict
           (eq :accept-remote (plist-get conflict :resolution)))
      (gnosis-nodes--accept-remote-conflict state conflict))
     (conflict state)
     ((and confirmation
           (let ((destination
                  (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
                 (source (and base-path
                              (gnosis-nodes--safe-path
                               gnosis-nodes-dir base-path))))
             (and (not (seq-some
                        (lambda (buffer)
                          (and (buffer-live-p buffer)
                               (buffer-modified-p buffer)))
                        (list (get-file-buffer destination)
                              (and source (get-file-buffer source)))))
                  (not (and source
                            (not (equal source destination))
                            (get-file-buffer source)
                            (get-file-buffer destination)))
                  (if active
                      (equal remote-digest
                             (and (file-regular-p destination)
                                  (not (file-symlink-p destination))
                                  (gnosis-nodes--digest
                                   (gnosis-nodes--working-bytes destination))))
                    (null local-bytes))
                  (or (null base-path)
                      (equal base-path logical-path)
                      (null local-bytes)))))
      (unless gnosis-nodes--preflight
        (when active
          (let* ((destination
                  (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
                 (source (and base-path
                              (gnosis-nodes--safe-path
                               gnosis-nodes-dir base-path)))
                 (source-buffer (and source (get-file-buffer source))))
            (gnosis-nodes--write-document
             logical-path (gnosis-nodes--working-bytes destination))
            (when (and (buffer-live-p source-buffer)
                       (not (equal source destination)))
              (with-current-buffer source-buffer
                (set-visited-file-name destination t)
                (revert-buffer :ignore-auto :noconfirm))))))
      (gnosis-nodes--record-base state projection))
     ((and base (= (plist-get base :revision)
                   (plist-get projection :revision)))
      (unless (equal (plist-get base :fingerprint) fingerprint)
        (signal 'gnosis-nodes-error
                '("Gnosis equal document revision changed projection")))
      state)
     (t
      (let* ((remote-bytes
              (and remote-digest
                   (or (cdr (assoc remote-digest blobs))
                       (gnosis-core-blob remote-digest))))
             (unresolved
              (seq-find
               (lambda (entry)
                 (and (memq (plist-get entry :status)
                            '(:pending :blocked :sending :uncertain :rejected))
                      (or (equal document-id (plist-get entry :document-id))
                          (equal logical-path (plist-get entry :logical-path))
                          (equal logical-path (plist-get entry :old-logical-path)))))
               (append (plist-get state :outbox) nil)))
             (plan (if unresolved
                       '(:action :conflict)
                     (if (and base-path (not (equal base-path logical-path)))
                         (if (and active
                                  (equal local-digest base-digest)
                                  (not (file-exists-p
                                        (gnosis-nodes--safe-path
                                         gnosis-nodes-dir logical-path))))
                             '(:action :move)
                           '(:action :conflict))
                     (if (and active (null remote-digest) local-bytes (null base))
                         '(:action :conflict)
                       (gnosis-nodes--plan-document
                          base-digest local-digest remote-digest active))))))
        (pcase (plist-get plan :action)
          (:materialize
           (gnosis-nodes--write-document logical-path remote-bytes)
           (gnosis-nodes--record-base state projection))
          (:move
           (gnosis-nodes--move-document base-path logical-path remote-bytes)
           (gnosis-nodes--record-base state projection))
          (:record
           (gnosis-nodes--record-base state projection))
          (:conflict
           (gnosis-nodes--record-conflict
            state base projection local-bytes remote-bytes))
          (:keep
           (if (equal local-digest remote-digest)
               (gnosis-nodes--record-base state projection)
             state))))))))

(defun gnosis-nodes--apply-item (state item &optional blobs)
  "Apply replica ITEM to STATE when it is a document projection."
  (if (equal "document" (plist-get item :entity_kind))
      (let ((projection (plist-get item :projection)))
        (when (hash-table-p gnosis-nodes--changed-document-ids)
          (puthash (plist-get projection :document_id) t
                   gnosis-nodes--changed-document-ids))
        (gnosis-nodes--apply-document state projection blobs))
    state))

(defun gnosis-nodes--valid-document-p (projection)
  "Return non-nil when document PROJECTION has required invariants."
  (let ((digest (gnosis-nodes--nullable
                 (plist-get projection :content_sha256)))
        (blob-url (gnosis-nodes--nullable (plist-get projection :blob_url)))
        (parsed (gnosis-nodes--nullable (plist-get projection :projection)))
        (operation (plist-get projection :operation)))
    (and (listp projection)
         (stringp (plist-get projection :document_id))
         (stringp (plist-get projection :logical_path))
         (integerp (plist-get projection :revision))
         (> (plist-get projection :revision) 0)
         (member operation '("create" "update" "rename" "snapshot" "tombstone"))
         (or (null digest)
             (and (stringp digest)
                  (string-match-p "\\`[0-9a-f]\\{64\\}\\'" digest)))
         (pcase operation
           ("tombstone" (and (null digest) (null blob-url) (null parsed)))
           ("snapshot"
            (let ((snapshot-digest (plist-get parsed :content_sha256)))
              (and (listp parsed) parsed
                   (stringp snapshot-digest)
                   (string-match-p "\\`[0-9a-f]\\{64\\}\\'" snapshot-digest)
                   (null digest) (null blob-url))))
           (_ (and (listp parsed) parsed
                   (stringp digest)
                   (equal blob-url (format "/v1/blobs/%s" digest))
                   (equal digest (plist-get parsed :content_sha256))))))))

(defun gnosis-nodes--valid-item-p (item &optional change-p)
  "Return non-nil when replica ITEM is valid; require cursor for CHANGE-P."
  (and (listp item)
       (or (not change-p) (stringp (plist-get item :cursor)))
       (pcase (plist-get item :entity_kind)
         ("document" (gnosis-nodes--valid-document-p
                      (plist-get item :projection)))
         ((or "asset" "thema" "settings" "review_state")
          (listp (plist-get item :projection)))
         (_ nil))))

(defun gnosis-nodes--decode-cursor (token &optional snapshot-p)
  "Return validated decoded TOKEN, or nil.
Require snapshot cursor shape when SNAPSHOT-P is non-nil."
  (condition-case nil
      (when (and (stringp token)
                 (string-match-p "\\`[A-Za-z0-9_-]+\\'" token))
        (let* ((translated
                (replace-regexp-in-string
                 "_" "/" (replace-regexp-in-string "-" "+" token t t) t t))
               (remainder (% (length translated) 4))
               (padded (concat translated
                               (make-string (if (= remainder 0)
                                                0
                                              (- 4 remainder))
                                            ?=)))
               (raw (base64-decode-string padded))
               (canonical
                (replace-regexp-in-string
                 "=+\\'" ""
                 (replace-regexp-in-string
                  "/" "_" (replace-regexp-in-string
                           "+" "-" (base64-encode-string raw t) t t)
                  t t)))
               (text (decode-coding-string raw 'utf-8))
               (decoded
                (json-parse-string
                 text :object-type 'plist :array-type 'array
                 :null-object nil :false-object nil))
               (keys (cl-loop for (key _) on decoded by #'cddr collect key))
               (expected (if snapshot-p
                             '(:version :snapshot_id :server_id
                               :cut_cursor :cut_token :after)
                           '(:version :server_id :position :token)))
               (numeric-keys (if snapshot-p
                                 '(:cut_cursor :after)
                               '(:position))))
          (and (/= remainder 1)
               (equal token canonical)
               (equal raw (encode-coding-string text 'utf-8))
               (= (length keys) (length expected))
               (seq-every-p (lambda (key) (memq key expected)) keys)
               (integerp (plist-get decoded :version))
               (= (plist-get decoded :version) 1)
               (stringp (plist-get decoded :server_id))
               (seq-every-p
                (lambda (key)
                  (let ((value (plist-get decoded key)))
                    (and (integerp value)
                         (<= 0 value #xffffffffffffffff))))
                numeric-keys)
               (if snapshot-p
                   (and (stringp (plist-get decoded :snapshot_id))
                        (stringp (plist-get decoded :cut_token)))
                 (stringp (plist-get decoded :token)))
               decoded)))
    (error nil)))

(defun gnosis-nodes--cursor-advances-p (from next &optional snapshot-p)
  "Return non-nil when NEXT strictly advances FROM on one server.
When SNAPSHOT-P is non-nil, also require one snapshot identity."
  (let ((from-data (gnosis-nodes--decode-cursor from snapshot-p))
        (next-data (gnosis-nodes--decode-cursor next snapshot-p)))
    (and from-data next-data
         (equal (plist-get from-data :server_id)
                (plist-get next-data :server_id))
         (or (not snapshot-p)
             (and (equal (plist-get from-data :snapshot_id)
                         (plist-get next-data :snapshot_id))
                  (equal (plist-get from-data :cut_cursor)
                         (plist-get next-data :cut_cursor))
                  (equal (plist-get from-data :cut_token)
                         (plist-get next-data :cut_token))))
         (< (plist-get from-data (if snapshot-p :after :position))
            (plist-get next-data (if snapshot-p :after :position))))))

(defun gnosis-nodes--snapshot-page-valid-p (page snapshot-id
                                                 &optional requested-next
                                                 expected-server)
  "Return non-nil when snapshot PAGE continues SNAPSHOT-ID consistently.
REQUESTED-NEXT is the continuation used to request PAGE.
EXPECTED-SERVER, when non-nil, must own every returned cursor."
  (let ((cursor (gnosis-nodes--nullable (plist-get page :cursor)))
        (next (gnosis-nodes--nullable (plist-get page :next)))
        (items (plist-get page :items)))
    (and (= (or (plist-get page :version) -1) 1)
         (stringp (plist-get page :snapshot_id))
         (or (null snapshot-id)
             (equal snapshot-id (plist-get page :snapshot_id)))
         (vectorp items)
         (seq-every-p #'gnosis-nodes--valid-item-p items)
         (not (eq (null cursor) (null next)))
         (if next
             (let ((decoded (gnosis-nodes--decode-cursor next t)))
               (and decoded
                    (or (null expected-server)
                        (equal expected-server
                               (plist-get decoded :server_id)))
                    (equal (plist-get decoded :snapshot_id)
                           (plist-get page :snapshot_id))
                    (or (null requested-next)
                        (gnosis-nodes--cursor-advances-p
                         requested-next next t))))
           (let ((decoded (gnosis-nodes--decode-cursor cursor)))
             (and decoded
                  (or (null expected-server)
                      (equal expected-server
                             (plist-get decoded :server_id)))
                  (or (null requested-next)
                      (let ((requested
                             (gnosis-nodes--decode-cursor requested-next t)))
                        (and requested
                             (equal (plist-get requested :server_id)
                                    (plist-get decoded :server_id))
                             (= (plist-get requested :cut_cursor)
                                (plist-get decoded :position))
                             (equal (plist-get requested :cut_token)
                                    (plist-get decoded :token)))))))))))

(defun gnosis-nodes--change-page-valid-p (page from)
  "Return non-nil when complete change PAGE consistently advances FROM."
  (let ((items (plist-get page :items))
        (next (plist-get page :next))
        cursors decoded-cursors)
    (when (vectorp items)
      (setq cursors (mapcar (lambda (item) (plist-get item :cursor))
                            (append items nil))
            decoded-cursors (mapcar #'gnosis-nodes--decode-cursor cursors)))
    (and (= (or (plist-get page :version) -1) 1)
         (equal from (plist-get page :from))
         (stringp next)
         (vectorp items)
         (seq-every-p (lambda (item) (gnosis-nodes--valid-item-p item t)) items)
         (if (= (length items) 0)
             (and (equal next from) (not (eq t (plist-get page :has_more))))
           (equal next (plist-get (aref items (1- (length items))) :cursor)))
         (not (member from cursors))
         (= (length cursors) (length (delete-dups (copy-sequence cursors))))
         (gnosis-nodes--decode-cursor from)
         (seq-every-p #'identity decoded-cursors)
         (let ((previous from)
               valid)
           (setq valid t)
           (dolist (cursor cursors valid)
             (unless (gnosis-nodes--cursor-advances-p previous cursor)
               (setq valid nil))
             (setq previous cursor)))
         (or (not (eq t (plist-get page :has_more)))
             (> (length items) 0)))))

(defun gnosis-nodes--snapshot-spool-file (index)
  "Return private snapshot spool filename for INDEX."
  (expand-file-name (format "snapshot/page-%06d.json" index)
                    (gnosis-nodes--state-directory)))

(defun gnosis-nodes--read-snapshot-page (record)
  "Read checksummed snapshot page from spool RECORD."
  (let* ((file (plist-get record :file))
         (bytes (gnosis-nodes--managed-bytes
                 (expand-file-name "snapshot/" (gnosis-nodes--state-directory))
                 file)))
    (unless (and bytes
                 (equal (plist-get record :sha256)
                        (gnosis-nodes--digest bytes)))
      (signal 'gnosis-nodes-error '("Gnosis snapshot spool checksum failed")))
    (with-temp-buffer
      (set-buffer-multibyte nil)
      (insert bytes)
      (goto-char (point-min))
      (json-parse-buffer :object-type 'plist :array-type 'array
                         :null-object nil :false-object nil))))

(defun gnosis-nodes--snapshot-pages-valid-p (pages)
  "Return non-nil when recovered PAGES form one complete prefix."
  (let ((snapshot-id nil)
        (requested-next nil)
        (remaining pages)
        seen
        valid)
    (setq valid t)
    (while (and valid remaining)
      (let* ((page (car remaining))
             (next (gnosis-nodes--nullable (plist-get page :next))))
        (setq valid
              (and (gnosis-nodes--snapshot-page-valid-p
                    page snapshot-id requested-next)
                   (not (member next seen))
                   (or (null (cdr remaining)) next)))
        (when next (push next seen))
        (setq snapshot-id (plist-get page :snapshot_id)
              requested-next next
              remaining (cdr remaining))))
    valid))

(defun gnosis-nodes--snapshot-blob-file (digest)
  "Return private snapshot blob path for DIGEST."
  (expand-file-name (concat "snapshot/blobs/" digest)
                    (gnosis-nodes--state-directory)))

(defun gnosis-nodes--read-snapshot-blob (record)
  "Return verified bytes from snapshot blob RECORD."
  (let* ((digest (plist-get record :sha256))
         (file (plist-get record :file))
         (bytes (gnosis-nodes--managed-bytes
                 (expand-file-name "snapshot/blobs/"
                                   (gnosis-nodes--state-directory))
                 file)))
    (unless (and bytes (equal digest (gnosis-nodes--digest bytes)))
      (signal 'gnosis-nodes-error '("Gnosis snapshot blob checksum failed")))
    bytes))

(defun gnosis-nodes--validate-snapshot-manifest (pages)
  "Validate complete document ownership manifest in snapshot PAGES."
  (let ((documents (make-hash-table :test #'equal))
        (paths (make-hash-table :test #'equal)))
    (dolist (page pages)
      (dolist (item (append (plist-get page :items) nil))
        (when (equal "document" (plist-get item :entity_kind))
          (let* ((projection (plist-get item :projection))
                 (document-id (plist-get projection :document_id))
                 (logical-path (plist-get projection :logical_path)))
            (when (or (gethash document-id documents)
                      (gethash logical-path paths))
              (signal 'gnosis-nodes-error
                      '("Duplicate document ownership in snapshot")))
            (gnosis-nodes--safe-path gnosis-nodes-dir logical-path)
            (puthash document-id t documents)
            (puthash logical-path t paths)))))))

(defun gnosis-nodes--snapshot-blobs (state pages)
  "Spool all document blobs in PAGES and return STATE plus byte alist."
  (let ((records (append (plist-get (plist-get state :snapshot) :blobs) nil))
        blobs)
    (dolist (record records)
      (push (cons (plist-get record :sha256)
                  (gnosis-nodes--read-snapshot-blob record))
            blobs))
    (dolist (page pages)
      (mapc
       (lambda (item)
         (when (equal "document" (plist-get item :entity_kind))
           (when-let* ((projection (plist-get item :projection))
                       (digest (gnosis-nodes--projection-digest projection))
                       ((null (assoc digest blobs))))
             (let* ((bytes (gnosis-core-blob digest))
                    (file (gnosis-nodes--snapshot-blob-file digest))
                    (record (list :file file :sha256 digest)))
               (gnosis-nodes--write-bytes file bytes)
               (push record records)
               (push (cons digest bytes) blobs)))))
       (append (plist-get page :items) nil)))
    (setf (plist-get (plist-get state :snapshot) :blobs) (vconcat records))
    (gnosis-nodes--write-state state)
    (cons state blobs)))

(defun gnosis-nodes--preflight-snapshot (state pages blobs)
  "Validate every file effect against STATE for snapshot PAGES.
Use BLOBS for remote bytes before materialization.
Return historical base blobs needed to create conflict evidence."
  (let ((gnosis-nodes--preflight t)
        (gnosis-nodes--conflict-base-blobs nil)
        (prepared (copy-tree state t)))
    (dolist (page pages)
      (mapc (lambda (item)
              (setq prepared (gnosis-nodes--apply-item prepared item blobs)))
            (append (plist-get page :items) nil)))
    gnosis-nodes--conflict-base-blobs))

(defun gnosis-nodes--snapshot-item-blobs (snapshot item)
  "Return verified blob alist needed by ITEM from SNAPSHOT."
  (when-let* (((equal "document" (plist-get item :entity_kind)))
              (projection (plist-get item :projection))
              (digest (gnosis-nodes--projection-digest projection))
              (record
               (seq-find (lambda (candidate)
                           (equal digest (plist-get candidate :sha256)))
                         (append (plist-get snapshot :blobs) nil))))
    (list (cons digest (gnosis-nodes--read-snapshot-blob record)))))

(defun gnosis-nodes--snapshot-spool-blob (snapshot digest bytes)
  "Add verified DIGEST and BYTES to SNAPSHOT spool when absent."
  (unless (seq-find (lambda (record)
                      (equal digest (plist-get record :sha256)))
                    (append (plist-get snapshot :blobs) nil))
    (let ((file (gnosis-nodes--snapshot-blob-file digest)))
      (gnosis-nodes--write-bytes file bytes)
      (setf (plist-get snapshot :blobs)
            (vconcat (plist-get snapshot :blobs)
                     (vector (list :file file :sha256 digest))))))
  snapshot)

(defun gnosis-nodes--snapshot-finish-stage (state snapshot expected-fingerprint)
  "Advance completed SNAPSHOT stage in STATE."
  (pcase (plist-get snapshot :stage)
    ("manifest"
     (let* ((shapes (mapcar #'read
                            (append (plist-get snapshot :shapes) nil)))
            (fingerprint
             (secure-hash
              'sha256
              (prin1-to-string
               (sort (delete-dups shapes)
                     (lambda (left right)
                       (string< (prin1-to-string left)
                                (prin1-to-string right))))))))
       (when (and expected-fingerprint
                  (not (equal fingerprint expected-fingerprint)))
         (signal 'gnosis-nodes-error
                 '("Snapshot protocol fingerprint changed")))
       (setf (plist-get state :protocol-fingerprint) fingerprint
             (plist-get snapshot :stage) "blobs"
             (plist-get snapshot :page-index) 0
             (plist-get snapshot :item-index) 0)))
    ("blobs"
     (setf (plist-get snapshot :stage) "preflight"
           (plist-get snapshot :page-index) 0
           (plist-get snapshot :item-index) 0
           (plist-get snapshot :prepared-bases) (plist-get state :bases)
           (plist-get snapshot :prepared-outbox) (plist-get state :outbox)
           (plist-get snapshot :prepared-conflicts) (plist-get state :conflicts)))
    ("preflight"
     (setf (plist-get snapshot :stage) "apply"
           (plist-get snapshot :page-index) 0
           (plist-get snapshot :item-index) 0))
    ("apply"
     (setq state (plist-put state :cursor (plist-get snapshot :cursor))
           state (plist-put state :snapshot nil))
     (gnosis-nodes--write-state state)
     (delete-directory
      (expand-file-name "snapshot" (gnosis-nodes--state-directory)) t)))
  (when (plist-get state :snapshot)
    (setq state (plist-put state :snapshot snapshot)))
  state)

(defun gnosis-nodes--snapshot-process-item (state snapshot item)
  "Process one ITEM for current SNAPSHOT stage and return STATE."
  (pcase (plist-get snapshot :stage)
    ("manifest"
     (when (equal "document" (plist-get item :entity_kind))
       (let* ((projection (plist-get item :projection))
              (document-id (plist-get projection :document_id))
              (logical-path (plist-get projection :logical_path)))
         (when (or (member document-id (append (plist-get snapshot :documents) nil))
                   (member logical-path (append (plist-get snapshot :paths) nil)))
           (signal 'gnosis-nodes-error
                   '("Duplicate document ownership in snapshot")))
         (gnosis-nodes--safe-path gnosis-nodes-dir logical-path)
         (setf (plist-get snapshot :documents)
               (vconcat (plist-get snapshot :documents) (vector document-id))
               (plist-get snapshot :paths)
               (vconcat (plist-get snapshot :paths) (vector logical-path)))))
     (let ((shape (prin1-to-string (gnosis-nodes--value-shape item))))
       (unless (member shape (append (plist-get snapshot :shapes) nil))
         (setf (plist-get snapshot :shapes)
               (vconcat (plist-get snapshot :shapes) (vector shape))))))
    ("blobs"
     (when-let* (((equal "document" (plist-get item :entity_kind)))
                 (projection (plist-get item :projection))
                 (digest (gnosis-nodes--projection-digest projection))
                 ((not (seq-find
                        (lambda (record)
                          (equal digest (plist-get record :sha256)))
                        (append (plist-get snapshot :blobs) nil)))))
       (gnosis-nodes--snapshot-spool-blob
        snapshot digest (gnosis-core-blob digest))))
    ("preflight"
     (let ((prepared (copy-tree state t))
           (gnosis-nodes--preflight t)
           (gnosis-nodes--conflict-base-blobs nil))
       (setf (plist-get prepared :snapshot) nil
             (plist-get prepared :bases) (plist-get snapshot :prepared-bases)
             (plist-get prepared :outbox) (plist-get snapshot :prepared-outbox)
             (plist-get prepared :conflicts) (plist-get snapshot :prepared-conflicts))
       (setq prepared
             (gnosis-nodes--apply-item
              prepared item (gnosis-nodes--snapshot-item-blobs snapshot item)))
       (dolist (blob gnosis-nodes--conflict-base-blobs)
         (gnosis-nodes--snapshot-spool-blob snapshot (car blob) (cdr blob)))
       (setf (plist-get snapshot :prepared-bases) (plist-get prepared :bases)
             (plist-get snapshot :prepared-outbox) (plist-get prepared :outbox)
             (plist-get snapshot :prepared-conflicts)
             (plist-get prepared :conflicts))))
    ("apply"
     (let* ((projection (plist-get item :projection))
            (base (and (equal "document" (plist-get item :entity_kind))
                       (gnosis-nodes--base
                        state (plist-get projection :document_id)
                        (plist-get projection :logical_path))))
            (base-digest (and base (plist-get base :digest)))
            (gnosis-nodes--conflict-base-blobs
             (when base-digest
               (gnosis-nodes--snapshot-item-blobs
                snapshot
                (list :entity_kind "document"
                      :projection (list :content_sha256 base-digest))))))
       (setq state
             (gnosis-nodes--apply-item
              state item (gnosis-nodes--snapshot-item-blobs snapshot item))))))
  state)

(defun gnosis-nodes--finalize-snapshot (state expected-fingerprint)
  "Advance STATE snapshot finalization and checkpoint one action budget.
Reject a completed manifest that differs from EXPECTED-FINGERPRINT."
  (let ((actions gnosis-nodes-max-snapshot-actions-per-pass))
    (while (and (plist-get state :snapshot) (> actions 0))
      (let* ((snapshot (plist-get state :snapshot))
             (pages (plist-get snapshot :pages))
             (page-index (or (plist-get snapshot :page-index) 0))
             (item-index (or (plist-get snapshot :item-index) 0)))
        (if (>= page-index (length pages))
            (progn
              (setq state
                    (gnosis-nodes--snapshot-finish-stage
                     state snapshot expected-fingerprint))
              (cl-decf actions))
          (let* ((page (gnosis-nodes--read-snapshot-page
                        (aref pages page-index)))
                 (items (plist-get page :items)))
            (if (>= item-index (length items))
                (progn
                  (setf (plist-get snapshot :page-index) (1+ page-index)
                        (plist-get snapshot :item-index) 0)
                  (cl-decf actions))
              (setq state
                    (gnosis-nodes--snapshot-process-item
                     state snapshot (aref items item-index)))
              (setf (plist-get snapshot :item-index) (1+ item-index))
              (cl-decf actions))))))
    (when (plist-get state :snapshot)
      (gnosis-nodes--write-state state))
    state))

(defun gnosis-nodes--snapshot (state &optional expected-fingerprint)
  "Durably fetch, validate, then materialize a full snapshot into STATE.
When EXPECTED-FINGERPRINT is non-nil, reject protocol-shape drift."
  (catch 'gnosis-nodes--snapshot-pass
    (let* ((resume (plist-get state :snapshot))
         (next (and resume (plist-get resume :next)))
         (pages (and resume (append (plist-get resume :pages) nil)))
         (snapshot-id (and resume (plist-get resume :snapshot-id)))
         (index (length pages))
         (done (and resume (null next)))
         seen
         page
         (pages-left gnosis-nodes-max-pages-per-pass))
    (when pages
      (condition-case nil
          (let* ((validated (or (plist-get resume :validated-pages) 0))
                 (stop (min (length pages)
                            (+ validated gnosis-nodes-max-pages-per-pass)))
                 (records (seq-subseq pages validated stop))
                 (decoded-pages
                  (mapcar #'gnosis-nodes--read-snapshot-page records))
                 (requested-next (plist-get resume :validated-next))
                 valid)
            (setq valid t)
            (dolist (candidate decoded-pages)
              (unless (gnosis-nodes--snapshot-page-valid-p
                       candidate snapshot-id requested-next
                       (plist-get state :server-id))
                (setq valid nil))
              (setq requested-next
                    (gnosis-nodes--nullable (plist-get candidate :next))))
            (unless valid
              (signal 'gnosis-nodes-error '("Invalid recovered snapshot")))
            (setf (plist-get resume :validated-pages) stop
                  (plist-get resume :validated-next) requested-next)
            (setq state (plist-put state :snapshot resume))
            (gnosis-nodes--write-state state)
            (when (< stop (length pages))
              (throw 'gnosis-nodes--snapshot-pass state))
            (setq page (car (last decoded-pages))
                  seen (append (plist-get resume :seen) nil)))
        (error
         (ignore-errors
           (delete-directory
            (expand-file-name "snapshot" (gnosis-nodes--state-directory)) t))
         (setq state (plist-put state :snapshot nil)
               resume nil next nil pages nil snapshot-id nil index 0 done nil
               page nil seen nil)
         (gnosis-nodes--write-state state)
         (signal 'gnosis-nodes-error
                 '("Discarded invalid recovered snapshot")))))
    (while (and (not done) (> pages-left 0))
      (cl-decf pages-left)
      (setq page (gnosis-core-sync-snapshot next))
      (unless (gnosis-nodes--snapshot-page-valid-p
               page snapshot-id next (plist-get state :server-id))
        (signal 'gnosis-nodes-error '("Invalid Gnosis snapshot page")))
      (when-let* ((continuation (gnosis-nodes--nullable (plist-get page :next))))
        (when (member continuation seen)
          (signal 'gnosis-nodes-error '("Repeated Gnosis snapshot continuation")))
        (push continuation seen))
      (setq snapshot-id (plist-get page :snapshot_id))
      (let* ((file (gnosis-nodes--snapshot-spool-file index))
             (bytes (encode-coding-string
                     (json-serialize (gnosis-nodes--json-value page)
                                     :null-object nil
                                     :false-object gnosis-core--json-false)
                     'utf-8 t))
             (record (list :file file :sha256 (gnosis-nodes--digest bytes))))
        (gnosis-nodes--write-bytes
         file bytes)
        (setq pages (append pages (list record)))
        (setq state
              (plist-put state :snapshot
                         (list :snapshot-id snapshot-id
                               :next (gnosis-nodes--nullable
                                      (plist-get page :next))
                               :cursor (plist-get page :cursor)
                               :seen (vconcat seen)
                               :pages (vconcat pages)
                               :blobs []
                               :validated-pages
                               (plist-get resume :validated-pages)
                               :validated-next
                               (plist-get resume :validated-next))))
        (gnosis-nodes--write-state state))
      (setq next (gnosis-nodes--nullable (plist-get page :next))
            done (null next)
            index (1+ index)))
    (if (not done)
        state
      (let ((snapshot (plist-get state :snapshot)))
        (unless (plist-get snapshot :stage)
          (setf (plist-get snapshot :stage) "manifest"
                (plist-get snapshot :page-index) 0
                (plist-get snapshot :item-index) 0
                (plist-get snapshot :documents) []
                (plist-get snapshot :paths) []
                (plist-get snapshot :shapes) [])
          (setq state (plist-put state :snapshot snapshot))
          (gnosis-nodes--write-state state))
        (gnosis-nodes--finalize-snapshot state expected-fingerprint))))))

(defun gnosis-nodes--pull (state)
  "Pull every change after STATE cursor, committing each validated page."
  (let ((seen (list (plist-get state :cursor)))
        (pages-left (if (integerp gnosis-nodes--pages-left)
                        gnosis-nodes--pages-left
                      gnosis-nodes-max-pages-per-pass))
        page done)
    (setq gnosis-nodes--pull-complete t)
    (while (and (not done) (> pages-left 0))
      (cl-decf pages-left)
      (when (integerp gnosis-nodes--pages-left)
        (setq gnosis-nodes--pages-left pages-left))
      (setq page (gnosis-core-sync-changes (plist-get state :cursor)))
      (unless (gnosis-nodes--change-page-valid-p
               page (plist-get state :cursor))
        (signal 'gnosis-nodes-error '("Invalid Gnosis change page")))
      (when (and (eq t (plist-get page :has_more))
                 (member (plist-get page :next) seen))
        (signal 'gnosis-nodes-error '("Repeated Gnosis change cursor")))
      (push (plist-get page :next) seen)
      (mapc (lambda (change)
              (setq state (gnosis-nodes--apply-item state change)))
            (append (plist-get page :items) nil))
      (setq state (plist-put state :cursor (plist-get page :next)))
      (gnosis-nodes--write-state state)
      (setq done (or gnosis-nodes--stop-requested
                     (not (eq t (plist-get page :has_more))))))
    (unless done
      (setq gnosis-nodes--pull-complete nil))
    state))

(defun gnosis-nodes--outbox-entry-for (state document-id logical-path)
  "Return newest unresolved entry for DOCUMENT-ID or LOGICAL-PATH in STATE."
  (seq-find
   (lambda (entry)
     (and (memq (plist-get entry :status)
                '(:pending :blocked :sending :uncertain :applied :rejected))
          (or (and document-id
                   (equal document-id (plist-get entry :document-id)))
              (equal logical-path (plist-get entry :logical-path)))))
   (reverse (append (plist-get state :outbox) nil))))

(defun gnosis-nodes--entry-with-bytes (entry bytes)
  "Return ENTRY updated with exact BYTES without changing operation identity."
  (let ((copy (copy-sequence entry)))
    (setq copy (plist-put copy :digest (gnosis-nodes--digest bytes))
          copy (plist-put copy :payload
                          (base64-encode-string
                           (string-to-unibyte bytes) t)))
    copy))

(defun gnosis-nodes--queue-save (state base bytes)
  "Queue exact BYTES after any unresolved operation for BASE in STATE."
  (let* ((document-id (plist-get base :document-id))
         (logical-path (plist-get base :logical-path))
         (head (gnosis-nodes--outbox-entry-for state document-id logical-path)))
    (cond
     ((and (equal (gnosis-nodes--digest bytes) (plist-get base :digest))
           (memq (plist-get head :status) '(:pending :blocked))
           (memq (plist-get head :kind) '(:create :replace)))
      (dolist (entry (append (plist-get state :outbox) nil))
        (when (and (memq (plist-get entry :status) '(:pending :blocked))
                   (memq (plist-get entry :kind) '(:create :replace))
                   (or (equal document-id (plist-get entry :document-id))
                       (equal logical-path (plist-get entry :logical-path))))
          (setf (plist-get entry :status) :superseded
                (plist-get entry :depends-on) nil)))
      state)
     ((and (equal (gnosis-nodes--digest bytes) (plist-get base :digest))
           (memq (plist-get head :status) '(:pending :blocked))
           (memq (plist-get head :kind) '(:rename :delete)))
      state)
     ((seq-find (lambda (conflict)
                  (equal document-id (plist-get conflict :document-id)))
                (append (plist-get state :conflicts) nil))
      state)
     ((and (memq (plist-get head :status) '(:pending :blocked))
           (memq (plist-get head :kind) '(:create :replace)))
      (let* ((outbox (plist-get state :outbox))
             (index (seq-position outbox head)))
        (aset outbox index (gnosis-nodes--entry-with-bytes head bytes))
        state))
     ((memq (plist-get head :status)
            '(:pending :blocked :sending :uncertain :applied))
      (let ((entry
             (gnosis-nodes--outbox-entry
              :replace document-id nil logical-path (gnosis-nodes--digest bytes)
              nil bytes)))
        (setf (plist-get entry :status) :blocked
              (plist-get entry :depends-on) (plist-get head :operation-id))
        (gnosis-nodes--enqueue state entry)))
     ((eq (plist-get head :status) :rejected)
      (let ((entry
             (gnosis-nodes--outbox-entry
              :replace document-id (plist-get base :revision)
              logical-path (gnosis-nodes--digest bytes) nil bytes)))
        (setf (plist-get head :status) :superseded)
        (gnosis-nodes--enqueue state entry)))
     (t
      (gnosis-nodes--enqueue
       state
       (gnosis-nodes--outbox-entry
        :replace document-id (plist-get base :revision)
        logical-path (gnosis-nodes--digest bytes) nil bytes))))))

(defun gnosis-nodes--promote-outbox (state)
  "Promote dependency-blocked entries in STATE with canonical feed bases."
  (let ((outbox (plist-get state :outbox)))
    (dotimes (index (length outbox))
      (let* ((entry (aref outbox index))
             (dependency-id (plist-get entry :depends-on))
             (dependency
              (seq-find (lambda (candidate)
                          (equal dependency-id
                                 (plist-get candidate :operation-id)))
                        (append outbox nil)))
             (authoritative (plist-get (plist-get dependency :result)
                                       :authoritative))
             (base (and authoritative
                        (gnosis-nodes--base
                         state (plist-get authoritative :document_id)
                         (plist-get authoritative :logical_path)))))
        (when (and (eq :blocked (plist-get entry :status))
                   (eq :applied (plist-get dependency :status))
                   base
                   (>= (plist-get base :revision)
                       (plist-get authoritative :revision)))
          (setq entry (plist-put entry :document-id
                                 (plist-get base :document-id))
                entry (plist-put entry :revision (plist-get base :revision))
                entry (if (memq (plist-get entry :kind) '(:replace :delete))
                          (plist-put entry :logical-path
                                     (plist-get base :logical-path))
                        entry)
                entry (plist-put entry :status :pending)
                entry (plist-put entry :depends-on nil))
          (aset outbox index entry))))
    state))

(defun gnosis-nodes--enqueue (state entry)
  "Append pending ENTRY to STATE."
  (plist-put state :outbox
             (vconcat (plist-get state :outbox) (vector entry))))

(defun gnosis-nodes--enqueue-after-head (state entry document-id logical-path)
  "Append ENTRY after unresolved head for DOCUMENT-ID or LOGICAL-PATH in STATE."
  (when-let* ((head (gnosis-nodes--outbox-entry-for
                     state document-id logical-path)))
    (if (eq :rejected (plist-get head :status))
        (setf (plist-get head :status) :superseded)
      (setf (plist-get entry :status) :blocked
            (plist-get entry :depends-on) (plist-get head :operation-id))))
  (gnosis-nodes--enqueue state entry))

(defun gnosis-nodes--scan (state &optional selected-path)
  "Enqueue changed bytes after a bounded scan of STATE.
When SELECTED-PATH is non-nil, inspect only that enrolled document."
  (let* ((bases (plist-get state :bases))
         (length (length bases))
         (stored-start (plist-get state :local-scan-index))
         (start (if (and (natnump stored-start) (< stored-start length))
                    stored-start
                  0))
         (limit (if selected-path
                    length
                  (min length gnosis-nodes-max-local-scan-per-pass)))
         (index start)
         (inspected 0))
    (while (< inspected limit)
      (let* ((base (aref bases index))
             (logical-path (plist-get base :logical-path)))
        (setq index (mod (1+ index) length))
        (cl-incf inspected)
        (when (and (plist-get base :active)
                   (or (null selected-path)
                       (equal selected-path logical-path)))
          (let* ((path (gnosis-nodes--safe-path
                        gnosis-nodes-dir logical-path))
                 (bytes (gnosis-nodes--working-bytes path))
                 (document-id (plist-get base :document-id))
                 (head (gnosis-nodes--outbox-entry-for
                        state document-id logical-path))
                 (conflict
                  (seq-find
                   (lambda (item)
                     (equal document-id (plist-get item :document-id)))
                   (append (plist-get state :conflicts) nil))))
            (if bytes
                (let ((digest (gnosis-nodes--digest bytes)))
                  (when (or head (not (equal digest (plist-get base :digest))))
                    (setq state
                          (gnosis-nodes--queue-save state base bytes))))
              (when (and (null head) (null conflict))
                (let* ((digest (plist-get base :digest))
                       (remote-bytes (gnosis-core-blob digest))
                       (projection
                        (list :document_id document-id
                              :revision (plist-get base :revision)
                              :logical_path logical-path
                              :content_sha256 digest
                              :blob_url (format "/v1/blobs/%s" digest)
                              :operation "update"
                              :projection (list :content_sha256 digest))))
                  (setq state
                        (gnosis-nodes--record-conflict
                         state base projection nil remote-bytes)))))))))
    (unless selected-path
      (plist-put state :local-scan-index index)))
  state)

(cl-defun gnosis-nodes--send-entry
    (entry &optional (payload nil payload-supplied-p))
  "Send pending outbox ENTRY once and return mutation response.
Use already validated PAYLOAD when supplied."
  (let ((operation-id (plist-get entry :operation-id))
        (document-id (plist-get entry :document-id))
        (revision (plist-get entry :revision))
        (logical-path (plist-get entry :logical-path))
        (payload
         (if payload-supplied-p
             payload
           (and (plist-get entry :payload)
                (base64-decode-string (plist-get entry :payload))))))
    (pcase (plist-get entry :kind)
      (:create
       (gnosis-core-document-create
        operation-id logical-path payload))
      (:replace
       (gnosis-core-document-replace
        document-id operation-id revision logical-path payload))
      (:rename
       (gnosis-core-document-rename
        document-id operation-id revision logical-path
        (plist-get entry :digest)))
      (:delete
       (gnosis-core-document-tombstone
        document-id operation-id revision logical-path))
      (_ (signal 'gnosis-nodes-error '("Invalid Gnosis pending operation"))))))

(defun gnosis-nodes--unchanged-head-matches-base-p (base head)
  "Return non-nil when reduced mutation HEAD exactly confirms BASE."
  (and base head
       (equal (plist-get base :document-id) (plist-get head :document_id))
       (= (plist-get base :revision) (plist-get head :revision))
       (equal (plist-get base :logical-path) (plist-get head :logical_path))
       (equal (plist-get base :digest)
              (gnosis-nodes--nullable (plist-get head :content_sha256)))
       (eq (and (plist-get base :active) t)
           (eq (plist-get head :active) t))))

(defun gnosis-nodes--entry-confirms-authoritative-p (entry authoritative)
  "Return non-nil when applied ENTRY intent exactly matches AUTHORITATIVE."
  (let* ((kind (plist-get entry :kind))
         (active (eq t (plist-get authoritative :active)))
         (digest (gnosis-nodes--nullable
                  (plist-get authoritative :content_sha256))))
    (and (eq :applied (plist-get entry :status))
         (memq kind '(:create :replace :rename :delete))
         (or (eq kind :create)
             (equal (plist-get entry :document-id)
                    (plist-get authoritative :document_id)))
         (equal (plist-get entry :logical-path)
                (plist-get authoritative :logical_path))
         (or (eq kind :delete)
             (equal (gnosis-nodes--nullable (plist-get entry :digest)) digest))
         (eq active (not (eq kind :delete))))))

(defun gnosis-nodes--entry-effect-projection (entry authoritative)
  "Return local-effect projection for confirmed ENTRY and AUTHORITATIVE."
  (let ((projection (copy-sequence authoritative)))
    (plist-put projection :operation
               (pcase (plist-get entry :kind)
                 (:create "create")
                 (:replace "update")
                 (:rename "rename")
                 (:delete "tombstone")))))

(defun gnosis-nodes--reconcile-applied-effect-p
    (entry record projection &optional dependent)
  "Reconcile exact local effect for applied ENTRY and return non-nil.
RECORD and PROJECTION describe the confirming feed conflict.  DEPENDENT is a
boundedly selected durable dependent local intent when present."
  (let* ((kind (plist-get entry :kind))
         (logical-path (plist-get projection :logical_path))
         (local-path (or (plist-get record :local-path) logical-path))
         (source-path (if (eq kind :rename)
                          (plist-get entry :previous-path)
                        local-path))
         (destination (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (source (gnosis-nodes--safe-path gnosis-nodes-dir source-path))
         (destination-buffer (get-file-buffer destination))
         (source-buffer (get-file-buffer source))
         (digest (gnosis-nodes--projection-digest projection)))
    (pcase kind
      (:delete
       (and (equal local-path logical-path)
            (not (and (buffer-live-p source-buffer)
                      (buffer-modified-p source-buffer)))
            (null (gnosis-nodes--working-bytes source))))
      (:rename
       (and (not (equal source-path logical-path))
            (not (and (buffer-live-p destination-buffer)
                      (buffer-modified-p destination-buffer)))
            (not (and (buffer-live-p source-buffer)
                      (buffer-modified-p source-buffer)))
            (let ((source-bytes (gnosis-nodes--working-bytes source))
                  (destination-bytes
                   (gnosis-nodes--working-bytes destination)))
              (and (or source-bytes destination-bytes)
                   (or (null source-bytes)
                       (equal digest (gnosis-nodes--digest source-bytes)))
                   (or (null destination-bytes)
                       (equal digest (gnosis-nodes--digest destination-bytes))
                       (and dependent
                            (equal (plist-get dependent :digest)
                                   (gnosis-nodes--digest destination-bytes))))
                   (cond
                    ((null source-bytes) t)
                    ((buffer-live-p destination-buffer)
                     (and (not (buffer-live-p source-buffer))
                          (progn
                            (unless gnosis-nodes--preflight
                              (delete-file source))
                            t)))
                    (t
                     (gnosis-nodes--move-document
                      source-path logical-path source-bytes)
                     t))))))
      (_
       (let ((bytes (gnosis-nodes--working-bytes destination)))
         (and (not (and (buffer-live-p destination-buffer)
                        (buffer-modified-p destination-buffer)))
              bytes
              (equal digest (gnosis-nodes--digest bytes))))))))

(defun gnosis-nodes--reconcile-applied-result
    (state entry authoritative record &optional dependent)
  "Reconcile exact applied ENTRY and RECORD against AUTHORITATIVE in STATE."
  (when (gnosis-nodes--entry-confirms-authoritative-p entry authoritative)
    (let* ((record-projection (plist-get record :projection))
           (feed-confirmed
            (gnosis-nodes--authoritative-confirms-projection-p
             authoritative record-projection))
           (base (and (not feed-confirmed)
                      (gnosis-nodes--base
                       state (plist-get authoritative :document_id))))
           (projection
            (if feed-confirmed record-projection
              (gnosis-nodes--entry-effect-projection entry authoritative))))
      (when (and (or feed-confirmed
                     (and base
                          (gnosis-nodes--unchanged-head-matches-base-p
                           base authoritative)))
                 (gnosis-nodes--reconcile-applied-effect-p
                  entry record projection dependent))
        (setf (plist-get state :conflicts)
              (vconcat
               (delq record (append (plist-get state :conflicts) nil))))
        (if feed-confirmed
            (setq state (gnosis-nodes--record-base state projection))
          (setq state (gnosis-nodes--promote-outbox state))
          (setf (plist-get state :outbox)
                (vconcat
                 (seq-remove
                  (lambda (item)
                    (equal (plist-get item :operation-id)
                           (plist-get entry :operation-id)))
                  (append (plist-get state :outbox) nil))))))))
  state)

(defun gnosis-nodes--start-applied-recovery (state entry authoritative)
  "Persist bounded recovery for applied ENTRY and AUTHORITATIVE in STATE."
  (plist-put
   state :applied-recovery
   (list :phase "conflict"
         :operation-id (plist-get entry :operation-id)
         :kind (substring (symbol-name (plist-get entry :kind)) 1)
         :document-id (plist-get entry :document-id)
         :revision (plist-get entry :revision)
         :logical-path (plist-get entry :logical-path)
         :previous-path (plist-get entry :previous-path)
         :digest (plist-get entry :digest)
         :payload (plist-get entry :payload)
         :authoritative (copy-tree authoritative) :cursor 0
         :remaining (length (plist-get state :conflicts)))))

(defun gnosis-nodes--recovery-entry (recovery)
  "Return applied outbox entry reconstructed from durable RECOVERY."
  (list :operation-id (plist-get recovery :operation-id)
        :kind (intern (concat ":" (plist-get recovery :kind)))
        :document-id (plist-get recovery :document-id)
        :revision (plist-get recovery :revision)
        :logical-path (plist-get recovery :logical-path)
        :previous-path (plist-get recovery :previous-path)
        :digest (plist-get recovery :digest)
        :payload (plist-get recovery :payload)
        :status :applied))

(defun gnosis-nodes--bounded-recovery-find
    (items cursor remaining predicate &optional budget)
  "Scan ITEMS by PREDICATE fairly from CURSOR while bounded by REMAINING.
Use BUDGET as the optional visit cap.
Return match, next cursor, remaining count, and visits."
  (let* ((length (length items))
         (left (min (or remaining length) length))
         (visited 0)
         match-index
         match)
    (while (and (not match) (> left 0)
                (< visited (or budget gnosis-nodes-max-outbox-scan-per-pass)))
      (let* ((index (mod cursor length))
             (item (aref items index)))
        (cl-incf cursor)
        (cl-decf left)
        (cl-incf visited)
        (when (funcall predicate item)
          (setq match item
                match-index index))))
    (list :match match :cursor (if (> length 0) (mod cursor length) 0)
          :match-index match-index :remaining left :visited visited)))

(defun gnosis-nodes--prune-unchanged-applied (state entry authoritative)
  "Prune unchanged applied ENTRY when STATE base confirms AUTHORITATIVE."
  (if (when-let* ((base (gnosis-nodes--base
                        state (plist-get authoritative :document_id))))
        (gnosis-nodes--unchanged-head-matches-base-p base authoritative))
      (let ((operation-id (plist-get entry :operation-id)))
        (setq state (gnosis-nodes--promote-outbox state))
        (plist-put
         state :outbox
         (vconcat
          (seq-remove
           (lambda (item)
             (equal (plist-get item :operation-id) operation-id))
           (append (plist-get state :outbox) nil)))))
    state))

(defun gnosis-nodes--advance-applied-recovery (state budget)
  "Advance one durable applied-result recovery phase in STATE within BUDGET."
  (let* ((recovery (plist-get state :applied-recovery))
         (phase (plist-get recovery :phase))
         (entry (gnosis-nodes--recovery-entry recovery))
         (authoritative (plist-get recovery :authoritative))
         (items (if (member phase '("conflict" "confirm-conflict"))
                    (plist-get state :conflicts)
                  (plist-get state :outbox)))
         (result
          (gnosis-nodes--bounded-recovery-find
           items (plist-get recovery :cursor) (plist-get recovery :remaining)
           (if (member phase '("conflict" "confirm-conflict"))
               (lambda (candidate)
                 (or (gnosis-nodes--authoritative-confirms-projection-p
                      authoritative (plist-get candidate :projection))
                     (and (plist-get entry :operation-id)
                          (equal (plist-get entry :operation-id)
                                 (plist-get candidate :operation-id)))))
             (lambda (candidate)
               (and (eq :replace (plist-get candidate :kind))
                    (memq (plist-get candidate :status)
                          '(:blocked :pending :sending :uncertain))
                    (equal (plist-get candidate :depends-on)
                           (plist-get entry :operation-id))
                    (equal (plist-get candidate :logical-path)
                           (plist-get entry :logical-path)))))
           budget))
         (match (plist-get result :match))
         (remaining (plist-get result :remaining)))
    (setq recovery (plist-put recovery :cursor (plist-get result :cursor)))
    (setq recovery (plist-put recovery :remaining remaining))
    (cond
     ((and (equal phase "conflict") match
           (eq :rename (plist-get entry :kind)))
      (setq recovery (plist-put recovery :phase "dependent"))
      (setq recovery (plist-put recovery :cursor 0))
      (setq recovery
            (plist-put recovery :remaining (length (plist-get state :outbox))))
      (setq state (plist-put state :applied-recovery recovery)))
     ((and (member phase '("conflict" "confirm-conflict")) match)
      (setq state
            (gnosis-nodes--reconcile-applied-result
             state entry authoritative match
             (and (plist-get recovery :dependent-digest)
                  (list :digest (plist-get recovery :dependent-digest)))))
      (setq state (plist-put state :applied-recovery nil)))
     ((and (equal phase "dependent") (or match (= remaining 0)))
      (setq recovery (plist-put recovery :phase "confirm-conflict"))
      (setq recovery
            (plist-put recovery :dependent-digest
                       (and match (plist-get match :digest))))
      (setq recovery (plist-put recovery :cursor 0))
      (setq recovery
            (plist-put recovery :remaining
                       (length (plist-get state :conflicts))))
      (setq state (plist-put state :applied-recovery recovery)))
     ((= remaining 0)
      (setq state
            (gnosis-nodes--prune-unchanged-applied state entry authoritative))
      (setq state (plist-put state :applied-recovery nil)))
     (t
      (setq state (plist-put state :applied-recovery recovery))))
    (list :state state :visited (plist-get result :visited))))

(defun gnosis-nodes--run-applied-recovery-pass (state budget)
  "Advance applied recovery in STATE with total inspection BUDGET."
  (let ((left budget)
        result)
    (while (and (plist-get state :applied-recovery) (> left 0))
      (setq result (gnosis-nodes--advance-applied-recovery state left))
      (setq state (plist-get result :state))
      (if (> (plist-get result :visited) 0)
          (cl-decf left (plist-get result :visited))
        (when (plist-get state :applied-recovery)
          (setq left 0))))
    (setq gnosis-nodes--recovery-visits (- budget left))
    state))

(defun gnosis-nodes--send-outbox (state)
  "Send pending operations in STATE once; persist unknown delivery explicitly."
  (if (plist-get state :applied-recovery)
      (let ((gnosis-nodes--recovery-visits 0))
        (setq state
              (gnosis-nodes--run-applied-recovery-pass
               state gnosis-nodes-max-outbox-scan-per-pass))
        (gnosis-nodes--write-state state)
        (if (and (not (plist-get state :applied-recovery))
                 (< gnosis-nodes--recovery-visits
                    gnosis-nodes-max-outbox-scan-per-pass))
            (let ((gnosis-nodes-max-outbox-scan-per-pass
                   (- gnosis-nodes-max-outbox-scan-per-pass
                      gnosis-nodes--recovery-visits)))
              (gnosis-nodes--send-outbox state))
          state))
    (let* ((outbox (plist-get state :outbox))
         (length (length outbox))
         (stored-start (plist-get state :outbox-scan-index))
         (start (if (natnump stored-start) stored-start 0))
         (inspected 0)
         (sent 0))
    (when (or (< start 0) (>= start (max length 1)))
      (setq start 0))
    (catch 'gnosis-nodes--stop-outbox
      (while (and (< inspected length)
                  (< inspected gnosis-nodes-max-outbox-scan-per-pass))
        (let* ((index (mod (+ start inspected) length))
               (entry (aref outbox index))
               payload)
          (when (and (eq :pending (plist-get entry :status))
                     (>= sent gnosis-nodes-max-mutations-per-pass))
            (setq state (plist-put state :outbox-scan-index index))
            (throw 'gnosis-nodes--stop-outbox nil))
          (cl-incf inspected)
          (setq state (plist-put state :outbox-scan-index
                                 (mod (+ index 1) length)))
          (when (eq :applied (plist-get entry :status))
            (when-let* ((authoritative
                        (plist-get (plist-get entry :result) :authoritative)))
              (setq state
                    (gnosis-nodes--start-applied-recovery
                     state entry authoritative))
              (setq state
                    (gnosis-nodes--run-applied-recovery-pass
                     state (- gnosis-nodes-max-outbox-scan-per-pass
                              inspected)))
              (when (and (< (length (plist-get state :outbox)) length)
                         (< (+ inspected gnosis-nodes--recovery-visits)
                            gnosis-nodes-max-outbox-scan-per-pass))
                (let ((gnosis-nodes-max-outbox-scan-per-pass
                       (- gnosis-nodes-max-outbox-scan-per-pass
                          inspected gnosis-nodes--recovery-visits)))
                  (setq state (gnosis-nodes--send-outbox state))))
              (gnosis-nodes--write-state state)
              (throw 'gnosis-nodes--stop-outbox nil)))
          (when (eq :pending (plist-get entry :status))
            (when (memq (plist-get entry :kind) '(:create :replace))
              (let ((encoded (plist-get entry :payload)))
                (unless encoded
                  (signal 'gnosis-nodes-error
                          '("Outbox content payload missing")))
                (setq payload (base64-decode-string encoded))
                (unless (equal (plist-get entry :digest)
                               (gnosis-nodes--digest payload))
                  (signal 'gnosis-nodes-error
                          '("Outbox payload digest mismatch")))
                (gnosis-core--org-string payload)))
              (cl-incf sent)
              (setq gnosis-nodes--mutation-sent t)
              (aset outbox index (gnosis-nodes--outbox-mark entry :sending))
              (gnosis-nodes--write-state state)
              (condition-case nil
                  (let* ((result (gnosis-nodes--send-entry entry payload))
                         (authoritative
                          (copy-tree (plist-get result :authoritative))))
                    (setq entry (plist-put entry :result result))
                    (aset outbox index
                          (gnosis-nodes--outbox-mark
                           entry (if (equal "applied" (plist-get result :status))
                                     :applied :rejected)))
                    (when (equal "applied" (plist-get result :status))
                      (gnosis-nodes--write-state state)
                      (setq state
                            (gnosis-nodes--start-applied-recovery
                             state entry authoritative))
                      (setq state
                            (gnosis-nodes--run-applied-recovery-pass
                             state (- gnosis-nodes-max-outbox-scan-per-pass
                                      inspected)))
                      (gnosis-nodes--write-state state)
                      (throw 'gnosis-nodes--stop-outbox nil))
                    (gnosis-nodes--write-state state)
                    (when (equal "rejected" (plist-get result :status))
                      (throw 'gnosis-nodes--stop-outbox nil)))
                (gnosis-core-mutation-uncertain
                 (aset outbox index
                       (gnosis-nodes--outbox-mark entry :uncertain))
                 (gnosis-nodes--write-state state)
                 (throw 'gnosis-nodes--stop-outbox nil)))))))
    (when (> inspected 0)
      (gnosis-nodes--write-state state))
    state)))

(defmacro gnosis-nodes--with-lock (&rest body)
  "Run BODY while holding exclusive private synchronization lock."
  (declare (indent 0) (debug t))
  `(let ((lock (expand-file-name "sync.lock" (gnosis-nodes--state-directory))))
     (make-directory (file-name-directory lock) t)
     (set-file-modes (file-name-directory lock) #o700)
     (condition-case nil
         (lock-file lock)
       (file-locked
        (signal 'gnosis-nodes-busy '("Gnosis synchronization is busy"))))
     (unwind-protect (progn ,@body)
       (unlock-file lock))))

(defun gnosis-nodes--value-shape (value)
  "Return closed structural shape for protocol VALUE."
  (cond
   ((vectorp value)
    (list :array (delete-dups
                  (mapcar #'gnosis-nodes--value-shape (append value nil)))))
   ((and (listp value) (keywordp (car value)))
    (cl-loop for (key item) on value by #'cddr
             collect key
             collect (gnosis-nodes--value-shape item)))
   (t :scalar)))

(defun gnosis-nodes--snapshot-fingerprint (pages)
  "Return protocol-shape fingerprint for complete snapshot PAGES."
  (let ((shapes
         (cl-loop for page in pages
                  append (mapcar #'gnosis-nodes--value-shape
                                 (append (plist-get page :items) nil)))))
    (secure-hash
     'sha256
     (prin1-to-string
      (sort (delete-dups shapes)
            (lambda (left right)
              (string< (prin1-to-string left) (prin1-to-string right))))))))

;;;###autoload
(defun gnosis-nodes-bootstrap ()
  "Initialize local synchronization from a complete authoritative snapshot."
  (interactive)
  (make-directory gnosis-nodes-dir t)
  (let (complete)
    (gnosis-nodes--with-lock
      (let* ((existing (and (file-exists-p (gnosis-nodes--state-file))
                            (gnosis-nodes--read-state)))
             (identity (gnosis-core-sync-identity))
             (remote (gnosis-core-sync-state))
             (state
              (or existing
                  (list :version 1
                        :server-id (plist-get remote :server_id)
                        :device-id (plist-get identity :device_id)
                        :cursor (plist-get remote :cursor)
                        :bases [] :outbox [] :conflicts []))))
        (when (and existing
                   (plist-get existing :protocol-fingerprint)
                   (null (plist-get existing :snapshot)))
          (user-error "Gnosis synchronization is already bootstrapped"))
        (unless (and (equal (plist-get state :server-id)
                            (plist-get remote :server_id))
                     (equal (plist-get state :device-id)
                            (plist-get identity :device_id)))
          (signal 'gnosis-nodes-error '("Gnosis bootstrap identity changed")))
        (unless existing (gnosis-nodes--write-state state))
        (setq state (gnosis-nodes--snapshot state))
        (gnosis-nodes--write-state state)
        (setq complete (null (plist-get state :snapshot)))))
    (message (if complete
                 "Gnosis Org synchronization bootstrapped"
               "Gnosis Org bootstrap paused; run gnosis-nodes-bootstrap again"))))

(defun gnosis-nodes--assert-identity (state)
  "Reject STATE when current server or device identity differs."
  (let ((identity (gnosis-core-sync-identity))
        (remote (gnosis-core-sync-state)))
    (unless (and (equal (plist-get state :device-id)
                        (plist-get identity :device_id))
                 (equal (plist-get state :server-id)
                        (plist-get remote :server_id)))
      (signal 'gnosis-nodes-error '("Gnosis synchronization identity changed")))))

;;;###autoload
(defun gnosis-nodes-reset ()
  "Replace local sync metadata from a trusted fresh snapshot.
Server identity and prior protocol fingerprint must still match."
  (interactive)
  (unless (yes-or-no-p "Discard local Gnosis sync metadata and rebootstrap? ")
    (user-error "Reset cancelled"))
  (let (complete)
    (gnosis-nodes--with-lock
      (let* ((trusted (gnosis-nodes--read-state))
             (identity (gnosis-core-sync-identity))
             (remote (gnosis-core-sync-state))
             (original-file (gnosis-nodes--state-file))
             (temporary-file (concat original-file ".reset"))
             (fresh (list :version 1
                          :server-id (plist-get remote :server_id)
                          :device-id (plist-get identity :device_id)
                          :cursor (plist-get remote :cursor)
                          :bases [] :outbox [] :conflicts []))
             (staged
              (if (file-exists-p temporary-file)
                  (let ((gnosis-nodes-state-file temporary-file))
                    (gnosis-nodes--read-state))
                fresh)))
        (unless (stringp (plist-get trusted :protocol-fingerprint))
          (user-error "Gnosis trusted protocol fingerprint is missing"))
        (unless (and (equal (plist-get trusted :server-id)
                            (plist-get staged :server-id))
                     (equal (plist-get trusted :device-id)
                            (plist-get staged :device-id))
                     (equal (plist-get fresh :server-id)
                            (plist-get staged :server-id))
                     (equal (plist-get fresh :device-id)
                            (plist-get staged :device-id)))
          (signal 'gnosis-nodes-error '("Gnosis reset identity changed")))
        (let ((gnosis-nodes-state-file temporary-file))
          (unless (file-exists-p temporary-file)
            (gnosis-nodes--write-state staged))
          (setq staged
                (gnosis-nodes--snapshot
                 staged (plist-get trusted :protocol-fingerprint)))
          (gnosis-nodes--write-state staged))
        (setq complete (null (plist-get staged :snapshot)))
        (when complete
          (let ((gnosis-nodes-state-file original-file))
            (gnosis-nodes--write-state staged))
          (delete-file temporary-file))))
    (message (if complete
                 "Gnosis Org synchronization metadata reset"
               "Gnosis Org reset paused; run gnosis-nodes-reset again"))))

;;;###autoload
(defun gnosis-nodes-sync (&optional scope full)
  "Synchronize Gnosis Org data for SCOPE.
SCOPE is one local file, `:all' for all eligible Org files, or nil for no new
enrollment.  With FULL non-nil, finish all available pull and local scan work."
  (interactive
   (list
    (pcase (read-char-choice "Sync [f]ile or [a]ll files? " '(?f ?a))
      (?f (or buffer-file-name
              (user-error "Current buffer does not visit a file")))
      (?a :all))
    current-prefix-arg))
  (when gnosis-nodes--busy
    (user-error "Gnosis synchronization is already running"))
  (let ((gnosis-nodes--busy t)
        (gnosis-nodes--pages-left
         (if full most-positive-fixnum gnosis-nodes-max-pages-per-pass))
        (gnosis-nodes-max-local-scan-per-pass
         (if full most-positive-fixnum gnosis-nodes-max-local-scan-per-pass))
        (gnosis-nodes--mutation-sent nil)
        (gnosis-nodes--changed-document-ids
         (make-hash-table :test #'equal))
        starting-generation
        final-state)
    (setq gnosis-nodes--stop-requested nil)
    (gnosis-nodes--with-lock
      (let ((state (gnosis-nodes--read-state)))
        (when (or (plist-get state :snapshot)
                  (null (plist-get state :protocol-fingerprint)))
          (user-error "Gnosis bootstrap is incomplete; run gnosis-nodes-bootstrap"))
        (gnosis-nodes--assert-identity state)
        (setq starting-generation (plist-get state :cursor))
        (setq state
              (pcase scope
                (:all (gnosis-nodes--enroll-all state))
                ((pred stringp) (gnosis-nodes--enroll-file state scope))
                (_ state)))
        (gnosis-nodes--write-state state)
        (unless gnosis-nodes--stop-requested
          (setq state (gnosis-nodes--pull state)))
        (unless (or gnosis-nodes--stop-requested
                    (not gnosis-nodes--pull-complete))
          (setq state
                (gnosis-nodes--scan
                 state (and (stringp scope)
                            (file-relative-name scope gnosis-nodes-dir))))
          (gnosis-nodes--write-state state)
          (setq state (gnosis-nodes--send-outbox state)))
        (unless (or gnosis-nodes--stop-requested
                    (not gnosis-nodes--mutation-sent)
                    (not gnosis-nodes--pull-complete))
          (setq state (gnosis-nodes--pull state))
          (gnosis-nodes--write-state state))
        (setq final-state state)))
    (when (and (not gnosis-nodes--stop-requested)
               gnosis-nodes--pull-complete)
      (gnosis-nodes--refresh-catalog-after-sync
       (gnosis-nodes--changed-document-list)
       starting-generation
       (plist-get final-state :cursor))))
  (message (cond
            (gnosis-nodes--stop-requested
             "Gnosis Org synchronization stopped")
            ((not gnosis-nodes--pull-complete)
             "Gnosis Org synchronization paused at durable page boundary")
            (t "Gnosis Org synchronization complete"))))

;;;###autoload
(defun gnosis-nodes-db-sync (&optional full)
  "Compatibility entry point for interactive `gnosis-nodes-sync'.
Preserve optional prefix FULL.  A noninteractive call synchronizes all enrolled
files, matching the legacy whole-database command boundary."
  (interactive "P")
  (if (called-interactively-p 'interactive)
      (let ((current-prefix-arg full))
        (call-interactively #'gnosis-nodes-sync))
    (gnosis-nodes-sync :all full)))

;;;###autoload
(defun gnosis-nodes-retry-uncertain (operation-id)
  "Explicitly requeue uncertain OPERATION-ID with its exact durable payload."
  (interactive
   (let* ((state (gnosis-nodes--read-state))
          (ids (cl-loop for entry across (plist-get state :outbox)
                        when (eq :uncertain (plist-get entry :status))
                        collect (plist-get entry :operation-id))))
     (unless ids (user-error "No uncertain Gnosis operations"))
     (list (completing-read "Retry uncertain operation: " ids nil t))))
  (unless (yes-or-no-p (format "Replay operation %s byte-identically? " operation-id))
    (user-error "Gnosis replay cancelled"))
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (outbox (plist-get state :outbox))
           (entry (seq-find
                   (lambda (item)
                     (and (eq :uncertain (plist-get item :status))
                          (equal operation-id (plist-get item :operation-id))))
                   (append outbox nil))))
      (unless entry (user-error "Uncertain Gnosis operation no longer exists"))
      (setf (plist-get entry :status) :pending)
      (gnosis-nodes--write-state state))))

;;;###autoload
(defun gnosis-nodes-stop ()
  "Request stop at next durable synchronization boundary."
  (interactive)
  (setq gnosis-nodes--stop-requested t))

(defun gnosis-nodes--current-org-file-p ()
  "Return non-nil when the current buffer visits a synchronized Org file."
  (and buffer-file-name
       (derived-mode-p 'org-mode)
       (string-suffix-p ".org" buffer-file-name t)
       (file-in-directory-p buffer-file-name gnosis-nodes-dir)))

(defun gnosis-nodes--saved-buffer-base (state logical-path head)
  "Return saved buffer base from STATE for LOGICAL-PATH and outbox HEAD."
  (let ((authoritative
         (plist-get (plist-get head :result) :authoritative)))
    (or (gnosis-nodes--base state nil logical-path)
        (and head
             (eq :create (plist-get head :kind))
             (list :document-id (plist-get authoritative :document_id)
                   :revision (plist-get authoritative :revision)
                   :logical-path logical-path
                   :digest (plist-get head :digest)
                   :active t)))))

(defun gnosis-nodes--queue-current-buffer ()
  "Durably queue the current saved Gnosis Org file."
  (when (and gnosis-nodes-mode (gnosis-nodes--current-org-file-p))
    (gnosis-nodes--with-lock
      (let* ((state (gnosis-nodes--read-state))
             (logical-path (file-relative-name buffer-file-name
                                               gnosis-nodes-dir))
             (head (gnosis-nodes--outbox-entry-for state nil logical-path))
             (base (gnosis-nodes--saved-buffer-base
                    state logical-path head)))
        (cond
         (base
          (let* ((bytes (gnosis-nodes--working-bytes buffer-file-name))
                 (digest (gnosis-nodes--digest bytes)))
            (when (or head
                      (not (equal digest (plist-get base :digest))))
              (setq state (gnosis-nodes--queue-save state base bytes))
              (gnosis-nodes--write-state state))))
         ((null head)
          (gnosis-nodes--write-state
           (gnosis-nodes--enroll-file state buffer-file-name))))))))

(defun gnosis-nodes--sync-current-buffer-after-save ()
  "Synchronize the current Gnosis Org file after saving."
  (when (and gnosis-nodes-mode (gnosis-nodes--current-org-file-p))
    (condition-case err
        (progn
          (gnosis-nodes--queue-current-buffer)
          (gnosis-nodes-sync buffer-file-name))
      (error
       (message "Gnosis synchronization failed: %s"
                (error-message-string err))))))

;;;###autoload
(define-minor-mode gnosis-nodes-mode
  "Synchronize the current Org document with Gnosis after each save."
  :lighter " Gnosis"
  (if gnosis-nodes-mode
      (progn
        (unless (gnosis-nodes--current-org-file-p)
          (setq gnosis-nodes-mode nil)
          (user-error "Buffer is not an Org file inside `gnosis-nodes-dir'"))
        (add-hook 'after-save-hook
                  #'gnosis-nodes--sync-current-buffer-after-save nil t))
    (remove-hook 'after-save-hook
                 #'gnosis-nodes--sync-current-buffer-after-save t)))

(defun gnosis-nodes--idle-sync ()
  "Run idle synchronization without surfacing expected user errors."
  (unless gnosis-nodes--busy
    (condition-case err
        (gnosis-nodes-sync)
      (error (message "%s" (error-message-string err))))))

(defun gnosis-nodes--maybe-enable-mode ()
  "Enable `gnosis-nodes-mode' for Org files inside `gnosis-nodes-dir'."
  (when (gnosis-nodes--current-org-file-p)
    (gnosis-nodes-mode 1)))

;;;###autoload
(define-minor-mode gnosis-nodes-auto-sync-mode
  "Automatically synchronize Gnosis Org documents and pending operations."
  :global t :lighter nil
  (when (timerp gnosis-nodes--idle-timer)
    (cancel-timer gnosis-nodes--idle-timer))
  (setq gnosis-nodes--idle-timer
        (and gnosis-nodes-auto-sync-mode
             (run-with-idle-timer gnosis-nodes-idle-seconds t
                                  #'gnosis-nodes--idle-sync)))
  (if gnosis-nodes-auto-sync-mode
      (progn
        (add-hook 'find-file-hook #'gnosis-nodes--maybe-enable-mode)
        (dolist (buffer (buffer-list))
          (with-current-buffer buffer
            (gnosis-nodes--maybe-enable-mode))))
    (remove-hook 'find-file-hook #'gnosis-nodes--maybe-enable-mode)
    (dolist (buffer (buffer-list))
      (with-current-buffer buffer
        (when gnosis-nodes-mode
          (gnosis-nodes-mode -1))))))

(gnosis-nodes-auto-sync-mode 1)

(defun gnosis-nodes--enroll-file (state file)
  "Return STATE with eligible Org FILE enrolled if not already tracked."
  (let* ((file (expand-file-name file))
         (logical-path (file-relative-name file gnosis-nodes-dir)))
    (unless (and (file-regular-p file)
                 (string-suffix-p ".org" file t)
                 (file-in-directory-p file gnosis-nodes-dir))
      (user-error "File must be an Org file below `gnosis-nodes-dir'"))
    (gnosis-nodes--safe-path gnosis-nodes-dir logical-path)
    (unless (or (gnosis-nodes--base state nil logical-path)
                (gnosis-nodes--outbox-entry-for state nil logical-path))
      (let ((bytes (gnosis-nodes--working-bytes file)))
        (setq state
              (gnosis-nodes--enqueue
               state (gnosis-nodes--outbox-entry
                      :create nil nil logical-path
                      (gnosis-nodes--digest bytes) nil bytes)))))
    state))

(defun gnosis-nodes--enroll-all (state)
  "Return STATE with all eligible Org files explicitly enrolled."
  (dolist (file (sort (seq-filter
                       (lambda (candidate)
                         (and (not (file-symlink-p candidate))
                              (condition-case nil
                                  (progn
                                    (gnosis-nodes--safe-path
                                     gnosis-nodes-dir
                                     (file-relative-name candidate
                                                         gnosis-nodes-dir))
                                    t)
                                (gnosis-nodes-path-error nil))))
                       (directory-files-recursively
                        gnosis-nodes-dir "\\.org\\'"))
                      #'string<))
    (setq state (gnosis-nodes--enroll-file state file)))
  state)

;;;###autoload
(defun gnosis-nodes-track-file (file)
  "Explicitly enroll Org FILE without contacting the server."
  (interactive "fOrg file to track: ")
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (logical-path (file-relative-name (expand-file-name file)
                                             gnosis-nodes-dir)))
      (when (or (gnosis-nodes--base state nil logical-path)
                (gnosis-nodes--outbox-entry-for state nil logical-path))
        (user-error "Org file is already tracked"))
      (gnosis-nodes--write-state
       (gnosis-nodes--enroll-file state file)))))

;;;###autoload
(defun gnosis-nodes-rename-file (file new-logical-path)
  "Explicitly queue tracked FILE rename to NEW-LOGICAL-PATH."
  (interactive "fTracked Org file: \nsNew logical path: ")
  (gnosis-nodes--safe-path gnosis-nodes-dir new-logical-path)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (old (file-relative-name (expand-file-name file)
                                    gnosis-nodes-dir))
           (base (gnosis-nodes--base state nil old)))
      (unless base (user-error "Org file is not tracked"))
      (setq state
            (gnosis-nodes--enqueue-after-head
             state (gnosis-nodes--outbox-entry
                    :rename (plist-get base :document-id)
                    (plist-get base :revision) new-logical-path
                    (plist-get base :digest) old)
             (plist-get base :document-id) old))
      (gnosis-nodes--write-state state))))

;;;###autoload
(defun gnosis-nodes-tombstone-file (file)
  "Explicitly tombstone tracked FILE while keeping local bytes."
  (interactive "fTracked Org file to tombstone: ")
  (unless (yes-or-no-p "Tombstone remote document and keep local file? ")
    (user-error "Tombstone cancelled"))
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (logical-path (file-relative-name (expand-file-name file)
                                             gnosis-nodes-dir))
           (base (gnosis-nodes--base state nil logical-path)))
      (unless base (user-error "Org file is not tracked"))
      (setq state
            (gnosis-nodes--enqueue-after-head
             state (gnosis-nodes--outbox-entry
                    :delete (plist-get base :document-id)
                    (plist-get base :revision) logical-path nil)
             (plist-get base :document-id) logical-path))
      (gnosis-nodes--write-state state))))

(defun gnosis-nodes--conflict-label (index record)
  "Return selection label for conflict INDEX and RECORD."
  (format "%d  %s  %s"
          (1+ index)
          (or (plist-get record :logical-path)
              (plist-get record :local-path)
              "<deleted>")
          (plist-get record :kind)))

(defun gnosis-nodes--select-conflict (conflicts)
  "Return selected (INDEX . RECORD) from CONFLICTS."
  (unless (> (length conflicts) 0)
    (user-error "No Gnosis Org conflicts"))
  (let* ((choices
          (cl-loop for record across conflicts
                   for index from 0
                   collect (cons (gnosis-nodes--conflict-label index record)
                                 index)))
         (index (if (= (length choices) 1)
                    0
                  (cdr (assoc (completing-read "Conflict: " choices nil t)
                              choices)))))
    (cons index (aref conflicts index))))

;;;###autoload
(defun gnosis-nodes-list-conflicts ()
  "Display recorded conflicts and return their labels."
  (interactive)
  (let* ((conflicts (plist-get (gnosis-nodes--read-state) :conflicts))
         (labels (cl-loop for record across conflicts
                          for index from 0
                          collect (gnosis-nodes--conflict-label index record))))
    (when (called-interactively-p 'interactive)
      (with-output-to-temp-buffer "*Gnosis Conflicts*"
        (princ (if labels
                   (concat (mapconcat #'identity labels "\n") "\n")
                 "No Gnosis Org conflicts\n"))))
    labels))

(defun gnosis-nodes--dispose-ediff-session (session-id)
  "Remove disposable artifacts for Ediff SESSION-ID."
  (when-let* ((session (gethash session-id gnosis-nodes--ediff-sessions)))
    (remhash session-id gnosis-nodes--ediff-sessions)
    (let ((merge-file (plist-get session :merge-file)))
      (when (and (stringp merge-file)
                 (file-in-directory-p merge-file
                                      (gnosis-nodes--state-directory))
                 (file-regular-p merge-file))
        (delete-file merge-file)))
    (dolist (buffer (plist-get session :disposable-buffers))
      (when (buffer-live-p buffer) (kill-buffer buffer)))))

(defun gnosis-nodes--abandon-ediff-session (session-id)
  "Invalidate and remove abandoned Ediff SESSION-ID artifacts."
  (when-let* ((session (gethash session-id gnosis-nodes--ediff-sessions)))
    (condition-case nil
        (progn
          (gnosis-nodes--with-lock
            (let* ((state (gnosis-nodes--read-state))
                   (conflicts (plist-get state :conflicts))
                   changed)
              (cl-loop for record across conflicts
                       when (and
                             (equal session-id
                                    (plist-get record :merge-session-id))
                             (eq :editing (plist-get record :status)))
                       do (setf (plist-get record :status) :unresolved
                                (plist-get record :merge-file) nil
                                (plist-get record :merge-start-digest) nil
                                (plist-get record :merge-session-id) nil
                                changed t))
              (when changed (gnosis-nodes--write-state state))))
          (gnosis-nodes--dispose-ediff-session session-id))
      (gnosis-nodes-busy
       (run-at-time 0.1 nil #'gnosis-nodes--abandon-ediff-session
                    session-id)))))

(defun gnosis-nodes--ediff-session-active-p (record)
  "Return non-nil when RECORD belongs to its matching live Ediff session."
  (let* ((session-id (plist-get record :merge-session-id))
         (session (and (stringp session-id)
                       (gethash session-id gnosis-nodes--ediff-sessions))))
    (and session
         (equal (plist-get record :document-id)
                (plist-get session :document-id))
         (equal (plist-get record :merge-file)
                (plist-get session :merge-file))
         (buffer-live-p (plist-get session :control-buffer)))))

;;;###autoload
(defun gnosis-nodes-resolve-conflict ()
  "Select a conflict and open Ediff with local, remote, and base buffers.
Edit and save Ediff's merge buffer; accept it before quitting Ediff."
  (interactive)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (conflicts (plist-get state :conflicts))
           (selection (gnosis-nodes--select-conflict conflicts))
           (index (car selection))
           (record (cdr selection)))
      (when (eq :rename (plist-get record :kind))
        (user-error "Choose local or remote resolution for rename conflicts"))
      (let* ((local (gnosis-nodes--safe-path
                     gnosis-nodes-dir
                     (or (plist-get record :local-path)
                         (plist-get record :logical-path))))
             (bytes (gnosis-nodes--working-bytes local))
             (conflict-directory
              (gnosis-nodes--conflict-directory
               (plist-get record :document-id)))
             (local-buffer (find-file-noselect local))
             (remote-file (plist-get record :remote-file))
             (base-file (plist-get record :base-file))
             (_remote-bytes
              (gnosis-nodes--managed-bytes conflict-directory remote-file))
             (_base-bytes
              (gnosis-nodes--managed-bytes conflict-directory base-file))
             (remote-existing (get-file-buffer remote-file))
             (base-existing (get-file-buffer base-file))
             (remote-buffer (find-file-noselect remote-file))
             (base-buffer (find-file-noselect base-file))
             (session-id
              (secure-hash
               'sha256
               (format "%s:%s:%s" (plist-get record :document-id)
                       (float-time) (random))))
             merge-file control-buffer)
        (unless bytes (user-error "Local conflict file no longer exists"))
        (make-directory conflict-directory t)
        (setq merge-file
              (make-temp-name (expand-file-name "merge-" conflict-directory)))
        (setf (plist-get record :merge-start-digest) (gnosis-nodes--digest bytes)
              (plist-get record :merge-file) merge-file
              (plist-get record :merge-session-id) session-id
              (plist-get record :status) :editing)
        (aset conflicts index record)
        (gnosis-nodes--write-state state)
        (require 'ediff)
        (setq control-buffer
              (ediff-merge-buffers-with-ancestor
               local-buffer remote-buffer base-buffer nil nil merge-file))
        (puthash session-id
                 (list :document-id (plist-get record :document-id)
                       :merge-file merge-file
                       :control-buffer control-buffer
                       :disposable-buffers
                       (delq nil
                             (list (unless remote-existing remote-buffer)
                                   (unless base-existing base-buffer))))
                 gnosis-nodes--ediff-sessions)
        (when (buffer-live-p control-buffer)
          (with-current-buffer control-buffer
            (add-hook 'ediff-after-quit-hook-internal
                      (lambda ()
                        (gnosis-nodes--abandon-ediff-session session-id))
                      nil t)))))))

(defun gnosis-nodes--remote-conflict-current-p (state record)
  "Return non-nil when RECORD remote head remains current after STATE cursor."
  (let* ((document-id (plist-get record :document-id))
         (latest
          (seq-reduce
           (lambda (left candidate)
             (if (and (equal document-id (plist-get candidate :document-id))
                      (> (plist-get candidate :remote-revision)
                         (plist-get left :remote-revision)))
                 candidate left))
           (seq-filter
            (lambda (candidate)
              (equal document-id (plist-get candidate :document-id)))
            (append (plist-get state :conflicts) nil))
           record))
         (cursor (plist-get state :cursor))
         (current (and (= (plist-get record :remote-revision)
                          (plist-get latest :remote-revision))
                       (equal (plist-get record :remote-digest)
                              (plist-get latest :remote-digest))))
         (pages-left gnosis-nodes-max-pages-per-pass)
         page done)
    (while (and current (not done) (> pages-left 0))
      (cl-decf pages-left)
      (setq page (gnosis-core-sync-changes cursor))
      (unless (gnosis-nodes--change-page-valid-p page cursor)
        (signal 'gnosis-nodes-error '("Invalid Gnosis change page")))
      (dolist (item (append (plist-get page :items) nil))
        (when (and (equal "document" (plist-get item :entity_kind))
                   (equal (plist-get record :document-id)
                          (plist-get (plist-get item :projection) :document_id)))
          (let ((projection (plist-get item :projection)))
            (setq current
                  (and (= (plist-get record :remote-revision)
                          (plist-get projection :revision))
                       (equal (gnosis-nodes--nullable
                               (plist-get record :remote-digest))
                              (gnosis-nodes--projection-digest projection)))))))
      (setq cursor (plist-get page :next)
            done (not (eq t (plist-get page :has_more)))))
    (and done current)))

;;;###autoload
(defun gnosis-nodes-accept-conflict ()
  "Select and accept saved merge buffer as a revisioned replacement.
Revalidate merge-buffer bytes and remote head before queueing."
  (interactive)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (conflicts (plist-get state :conflicts))
           (selection (gnosis-nodes--select-conflict conflicts))
           (index (car selection))
           (record (cdr selection)))
      (let* ((logical-path (plist-get record :logical-path))
             (local-path (or (plist-get record :local-path) logical-path))
             (path (gnosis-nodes--safe-path gnosis-nodes-dir local-path))
             (merge-file (plist-get record :merge-file))
             (conflict-directory
              (gnosis-nodes--conflict-directory
               (plist-get record :document-id)))
             (local-buffer (get-file-buffer path))
             (merge-buffer (and merge-file (get-file-buffer merge-file)))
             (local-bytes (gnosis-nodes--working-bytes path))
             (bytes (and merge-file
                         (gnosis-nodes--managed-bytes
                          conflict-directory merge-file)))
             (digest (and bytes (gnosis-nodes--digest bytes))))
        (unless (equal local-path logical-path)
          (user-error "Choose local or remote resolution for rename conflicts"))
        (unless bytes (user-error "Local merge file no longer exists"))
        (when (or (and (buffer-live-p local-buffer)
                       (buffer-modified-p local-buffer))
                  (and (buffer-live-p merge-buffer)
                       (buffer-modified-p merge-buffer)))
          (user-error "Save buffers before accepting merge"))
        (unless (and (eq :editing (plist-get record :status))
                     (stringp (plist-get record :merge-start-digest)))
          (user-error "Open conflict in Ediff before accepting merge"))
        (unless (gnosis-nodes--ediff-session-active-p record)
          (user-error "Matching Ediff session is no longer active"))
        (unless (equal (and local-bytes (gnosis-nodes--digest local-bytes))
                       (plist-get record :merge-start-digest))
          (user-error "Original local file changed; synchronize again"))
        (unless (gnosis-nodes--remote-conflict-current-p state record)
          (user-error "Remote conflict head changed; synchronize again"))
        (let ((original-state (copy-tree state t))
              (session-id (plist-get record :merge-session-id))
              entry)
          (setq state (gnosis-nodes--supersede-local-chain state record t))
          (setq entry
               (gnosis-nodes--outbox-entry
                :replace (plist-get record :document-id)
                (plist-get record :remote-revision) logical-path digest nil bytes))
          (setq state
                (gnosis-nodes--enqueue-after-head
                 state entry (plist-get record :document-id) logical-path))
          (setf (plist-get record :accepted-digest) digest
                (plist-get record :operation-id) (plist-get entry :operation-id)
                (plist-get record :resolution) :accept-merged
                (plist-get record :status) :queued
                (plist-get record :merge-file) nil
                (plist-get record :merge-start-digest) nil
                (plist-get record :merge-session-id) nil)
          (aset conflicts index record)
          (condition-case error-data
              (progn
                (gnosis-nodes--write-document local-path bytes)
                (gnosis-nodes--write-state state)
                (gnosis-nodes--dispose-ediff-session session-id))
            (error
             (gnosis-nodes--write-document local-path local-bytes)
             (let* ((recovery-conflicts (plist-get original-state :conflicts))
                    (recovery (aref recovery-conflicts index)))
               (setf (plist-get recovery :status) :unresolved
                     (plist-get recovery :merge-file) nil
                     (plist-get recovery :merge-start-digest) nil
                     (plist-get recovery :merge-session-id) nil)
               (gnosis-nodes--write-state original-state)
               (gnosis-nodes--dispose-ediff-session session-id))
             (signal (car error-data) (cdr error-data))))
          (message "Gnosis conflict merge queued"))))))

(defun gnosis-nodes--entry-confirms-conflict-p (entry record)
  "Return non-nil when applied ENTRY confirms RECORD remote projection."
  (let ((authoritative (plist-get (plist-get entry :result) :authoritative))
        (projection (plist-get record :projection)))
    (and (eq :applied (plist-get entry :status))
         authoritative projection
         (equal (plist-get authoritative :document_id)
                (plist-get projection :document_id))
         (= (plist-get authoritative :revision)
            (plist-get projection :revision))
         (equal (plist-get authoritative :logical_path)
                (plist-get projection :logical_path))
         (equal (gnosis-nodes--nullable
                 (plist-get authoritative :content_sha256))
                (gnosis-nodes--nullable
                 (plist-get projection :content_sha256)))
         (eq (plist-get authoritative :active)
             (if (equal "tombstone" (plist-get projection :operation))
                 gnosis-core--json-false
               t)))))

(defun gnosis-nodes--supersede-local-chain (state record &optional retain-dispatched)
  "Supersede safe local intent in STATE related to RECORD.
When RETAIN-DISPATCHED is non-nil, preserve the chain if it has sent intent."
  (let* ((document-id (plist-get record :document-id))
         (paths (delq nil (list (plist-get record :local-path)
                                (plist-get record :logical-path)
                                (plist-get record :previous-path))))
         (related
          (seq-filter
           (lambda (entry)
             (or (and document-id
                      (equal document-id (plist-get entry :document-id)))
                 (member (plist-get entry :logical-path) paths)
                 (member (plist-get entry :previous-path) paths)
                 (member (plist-get entry :new-path) paths)))
           (append (plist-get state :outbox) nil)))
         (dangerous
          (seq-find (lambda (entry)
                      (or (memq (plist-get entry :status)
                                '(:sending :uncertain))
                          (and (eq :applied (plist-get entry :status))
                               (not (gnosis-nodes--entry-confirms-conflict-p
                                     entry record)))))
                    related)))
    (if dangerous
        (unless retain-dispatched
          (user-error "Pending delivery must reconcile before resolution"))
      (dolist (entry related)
        (when (or (memq (plist-get entry :status)
                        '(:pending :blocked :rejected))
                  (gnosis-nodes--entry-confirms-conflict-p entry record))
          (setf (plist-get entry :status) :superseded
                (plist-get entry :depends-on) nil)))))
  state)

(defun gnosis-nodes--keep-local-conflict (state record local-bytes)
  "Queue explicit keep-local resolution for RECORD in STATE.
LOCAL-BYTES contains exact current local intent when a working file exists."
  (let* ((document-id (plist-get record :document-id))
         (remote-revision (plist-get record :remote-revision))
         (remote-path (plist-get record :logical-path))
         (local-path (or (plist-get record :local-path) remote-path))
         (digest (and local-bytes (gnosis-nodes--digest local-bytes)))
         entry)
    (setq state (gnosis-nodes--supersede-local-chain state record t))
    (pcase (plist-get record :kind)
      (:missing-local
       (setq entry (gnosis-nodes--outbox-entry
                    :delete document-id remote-revision remote-path
                    (plist-get record :remote-digest))))
      (:tombstone
       (unless local-bytes
         (user-error "Local conflict file no longer exists"))
       (setq entry (gnosis-nodes--outbox-entry
                    :create nil nil local-path digest nil local-bytes))
       (setf (plist-get state :bases)
             (vconcat
              (seq-remove
               (lambda (base)
                 (equal document-id (plist-get base :document-id)))
               (append (plist-get state :bases) nil)))))
      (:rename
       (unless local-bytes
         (user-error "Local conflict file no longer exists"))
       (setq entry (gnosis-nodes--outbox-entry
                    :rename document-id remote-revision local-path
                    (plist-get record :remote-digest) remote-path))
       (setq state
             (gnosis-nodes--enqueue-after-head state entry document-id remote-path))
       (unless (equal digest (plist-get record :remote-digest))
         (setq entry (gnosis-nodes--outbox-entry
                      :replace document-id remote-revision local-path digest nil
                      local-bytes)
               state (gnosis-nodes--enqueue-after-head
                      state entry document-id local-path))))
      (_
       (unless local-bytes
         (user-error "Local conflict file no longer exists"))
       (setq entry (gnosis-nodes--outbox-entry
                    :replace document-id remote-revision remote-path digest nil
                    local-bytes))))
    (unless (eq :rename (plist-get record :kind))
      (setq state
            (gnosis-nodes--enqueue-after-head state entry document-id remote-path)))
    (setf (plist-get record :status) :queued
          (plist-get record :operation-id) (plist-get entry :operation-id))
    (setf (plist-get state :conflicts)
          (gnosis-nodes--replace-vector-item
           (plist-get state :conflicts)
           (seq-find (lambda (item)
                       (equal document-id (plist-get item :document-id)))
                     (append (plist-get state :conflicts) nil))
           record))
    state))

(defun gnosis-nodes--delete-local-conflict-file (logical-path backup-file)
  "Back up and delete exact working LOGICAL-PATH at BACKUP-FILE."
  (let* ((path (gnosis-nodes--safe-path gnosis-nodes-dir logical-path))
         (buffer (get-file-buffer path)))
    (when (and (buffer-live-p buffer) (buffer-modified-p buffer))
      (signal 'gnosis-nodes-conflict (list logical-path)))
    (when (file-exists-p path)
      (gnosis-nodes--write-bytes backup-file (gnosis-nodes--working-bytes path))
      (delete-file path))
    (when (buffer-live-p buffer)
      (kill-buffer buffer))))

(defun gnosis-nodes--finish-accept-remote (state record projection)
  "Finish accepted remote RECORD and PROJECTION in STATE."
  (setq state (gnosis-nodes--supersede-local-chain state record)
        state (gnosis-nodes--record-base state projection))
  (plist-put
   state :conflicts
   (vconcat
    (seq-remove (lambda (item)
                  (equal (plist-get item :document-id)
                         (plist-get record :document-id)))
                (append (plist-get state :conflicts) nil)))))

(defun gnosis-nodes--accept-remote-conflict (state record)
  "Apply RECORD remote projection and clear it from STATE."
  (let* ((projection (plist-get record :projection))
         (local-path (or (plist-get record :local-path)
                         (plist-get record :logical-path)))
         (remote-path (plist-get record :logical-path))
         (path (gnosis-nodes--safe-path gnosis-nodes-dir local-path))
         (local-bytes (gnosis-nodes--working-bytes path))
         (local-digest (and local-bytes (gnosis-nodes--digest local-bytes))))
    (unless projection
      (user-error "Conflict predates explicit remote resolution; synchronize again"))
    (if (gnosis-nodes--accepted-remote-effect-p record projection)
        (gnosis-nodes--finish-accept-remote state record projection)
      (unless (equal local-digest (plist-get record :local-digest))
        (user-error "Local conflict file changed; synchronize again"))
      (setq state (gnosis-nodes--supersede-local-chain state record))
      (let ((old record))
        (setq record (plist-put record :resolution :accept-remote))
        (setf (plist-get state :conflicts)
              (gnosis-nodes--replace-vector-item
               (plist-get state :conflicts) old record)))
      (unless gnosis-nodes--preflight
        (gnosis-nodes--write-state state))
      (if (eq :tombstone (plist-get record :kind))
          (gnosis-nodes--delete-local-conflict-file
           local-path
           (expand-file-name "local-backup.org"
                             (gnosis-nodes--conflict-directory
                              (plist-get record :document-id))))
        (let ((remote-bytes
               (gnosis-nodes--managed-bytes
                (gnosis-nodes--conflict-directory
                 (plist-get record :document-id))
                (plist-get record :remote-file))))
          (unless (equal (gnosis-nodes--digest remote-bytes)
                         (plist-get record :remote-digest))
            (signal 'gnosis-nodes-error '("Invalid remote conflict sidecar")))
          (if (and (not (equal local-path remote-path))
                   (file-exists-p path))
              (gnosis-nodes--move-document local-path remote-path remote-bytes)
            (gnosis-nodes--write-document remote-path remote-bytes))))
      (gnosis-nodes--finish-accept-remote state record projection))))

;;;###autoload
(defun gnosis-nodes-keep-local-conflict ()
  "Select a conflict and resolve it by queueing exact local intent."
  (interactive)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (conflicts (plist-get state :conflicts))
           (record (cdr (gnosis-nodes--select-conflict conflicts))))
      (unless (gnosis-nodes--remote-conflict-current-p state record)
        (user-error "Remote head changed; synchronize before resolving"))
      (let* ((path (gnosis-nodes--safe-path
                    gnosis-nodes-dir
                    (or (plist-get record :local-path)
                        (plist-get record :logical-path))))
             (bytes (gnosis-nodes--working-bytes path)))
        (setq state (gnosis-nodes--keep-local-conflict state record bytes))
        (gnosis-nodes--write-state state)
        (message "Gnosis local conflict resolution queued")))))

;;;###autoload
(defun gnosis-nodes-accept-remote-conflict ()
  "Select a conflict and resolve it by accepting validated remote state."
  (interactive)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (conflicts (plist-get state :conflicts))
           (record (cdr (gnosis-nodes--select-conflict conflicts))))
      (unless (gnosis-nodes--remote-conflict-current-p state record)
        (user-error "Remote head changed; synchronize before resolving"))
      (setq state (gnosis-nodes--accept-remote-conflict state record))
      (gnosis-nodes--write-state state)
      (message "Gnosis remote conflict state accepted"))))

(provide 'gnosis-nodes)
;;; gnosis-nodes.el ends here
