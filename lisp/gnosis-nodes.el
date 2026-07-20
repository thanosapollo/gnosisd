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
(require 'seq)
(require 'subr-x)
(require 'gnosis-core)

(defgroup gnosis-nodes nil
  "Offline-editable Gnosis Org files."
  :group 'gnosis)

(defcustom gnosis-nodes-directory (expand-file-name "Gnosis" user-emacs-directory)
  "Root directory containing synchronized Org files."
  :type 'directory
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

(define-error 'gnosis-nodes-error "Gnosis Org synchronization failed")
(define-error 'gnosis-nodes-path-error "Unsafe Gnosis Org path" 'gnosis-nodes-error)
(define-error 'gnosis-nodes-conflict "Gnosis Org conflict" 'gnosis-nodes-error)

(defvar gnosis-nodes--idle-timer nil)
(defvar gnosis-nodes--stop-requested nil)
(defvar gnosis-nodes--busy nil)
(defvar gnosis-nodes-mode nil)

(defun gnosis-nodes--state-file ()
  "Return absolute synchronization state filename."
  (or gnosis-nodes-state-file
      (locate-user-emacs-file "gnosis/sync/state.json")))

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
      (when (or (file-symlink-p (directory-file-name root))
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
  (let ((root (file-name-as-directory
               (expand-file-name
                (if (string-prefix-p (gnosis-nodes--state-directory)
                                     (expand-file-name path))
                    (gnosis-nodes--state-directory)
                  gnosis-nodes-directory)))))
    (when (file-symlink-p (directory-file-name root))
      (signal 'gnosis-nodes-path-error (list root)))
    (when (gnosis-nodes--symlink-component-p root path)
      (signal 'gnosis-nodes-path-error (list path)))
    (make-directory (file-name-directory path) t)
    (when (string-prefix-p (gnosis-nodes--state-directory)
                           (expand-file-name path))
      (set-file-modes (file-name-directory path) #o700))
    (let ((temporary (make-temp-file
                      (expand-file-name ".gnosis-write-"
                                        (file-name-directory path)))))
      (unwind-protect
          (progn
            (with-temp-buffer
              (set-buffer-multibyte nil)
              (insert bytes)
              (let ((coding-system-for-write 'no-conversion))
                (write-region nil nil temporary nil 'silent)))
            (rename-file temporary path t))
        (when (file-exists-p temporary)
          (delete-file temporary))))))

(defun gnosis-nodes--write-document (logical-path bytes)
  "Atomically materialize exact Org BYTES at LOGICAL-PATH."
  (let* ((path (gnosis-nodes--safe-path gnosis-nodes-directory logical-path))
         (buffer (get-file-buffer path)))
    (when (and (buffer-live-p buffer) (buffer-modified-p buffer))
      (signal 'gnosis-nodes-conflict (list logical-path)))
    (gnosis-nodes--write-bytes path (string-to-unibyte bytes))
    (when (buffer-live-p buffer)
      (with-current-buffer buffer
        (revert-buffer :ignore-auto :noconfirm)))))

(defun gnosis-nodes--read-bytes (path)
  "Return exact bytes from PATH."
  (with-temp-buffer
    (set-buffer-multibyte nil)
    (insert-file-contents-literally path)
    (buffer-string)))

(defun gnosis-nodes--digest (bytes)
  "Return SHA-256 hex digest of unibyte BYTES."
  (secure-hash 'sha256 (string-to-unibyte bytes)))

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
          (dolist (key '(:kind :status))
            (when-let* ((value (plist-get entry key))
                        ((stringp value)))
              (setf (plist-get entry key) (intern (concat ":" value)))))
          (when (eq :sending (plist-get entry :status))
            (setf (plist-get entry :status) :uncertain))))))
  state)

(defun gnosis-nodes--notes-root ()
  "Return canonical configured notes root."
  (file-truename (file-name-as-directory gnosis-nodes-directory)))

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
    (with-temp-buffer
      (insert-file-contents file)
      (let ((state
             (json-parse-buffer :object-type 'plist :array-type 'array
                                :null-object nil :false-object nil)))
        (unless (and (= (or (plist-get state :version) -1) 1)
                     (stringp (plist-get state :server-id))
                     (stringp (plist-get state :device-id))
                     (stringp (plist-get state :notes-root))
                     (stringp (plist-get state :service-authority))
                     (stringp (plist-get state :cursor))
                     (vectorp (plist-get state :bases))
                     (vectorp (plist-get state :outbox))
                     (vectorp (plist-get state :conflicts)))
          (signal 'gnosis-nodes-error '("Invalid Gnosis synchronization state")))
        (unless (and (equal (plist-get state :notes-root)
                            (gnosis-nodes--notes-root))
                     (equal (plist-get state :service-authority)
                            (gnosis-nodes--service-authority)))
          (signal 'gnosis-nodes-error
                  '("Gnosis notes root or service authority changed")))
        (gnosis-nodes--normalize-state state)))))

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
            (with-temp-file temporary
              (insert
               (json-serialize
                (gnosis-nodes--json-state state)
                :null-object nil
                :false-object gnosis-core--json-false)))
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
            (dolist (key '(:kind :status))
              (when-let* ((value (plist-get entry key))
                          ((keywordp value)))
                (setf (plist-get entry key)
                      (substring (symbol-name value) 1))))))))
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

(defun gnosis-nodes--record-base (state projection)
  "Record authoritative document PROJECTION in STATE."
  (let* ((document-id (plist-get projection :document_id))
         (old (gnosis-nodes--base state document-id))
         (base (list :document-id document-id
                     :revision (plist-get projection :revision)
                     :logical-path (plist-get projection :logical_path)
                     :digest (gnosis-nodes--nullable
                              (plist-get projection :content_sha256))
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
              (and (equal document-id (plist-get conflict :document-id))
                   (equal (plist-get base :digest)
                          (plist-get conflict :accepted-digest))
                   (> (plist-get base :revision)
                      (or (plist-get conflict :remote-revision) -1))))
            (append (plist-get state :conflicts) nil))))
      (setf (plist-get state :conflicts) (vconcat conflicts)))
    (gnosis-nodes--promote-outbox state)))

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
         (directory (gnosis-nodes--conflict-directory document-id))
         (base-file (expand-file-name "base.org" directory))
         (remote-file (expand-file-name "remote.org" directory))
         (record (list :document-id document-id
                       :local-path (or (and base (plist-get base :logical-path))
                                       (plist-get projection :logical_path))
                       :logical-path (plist-get projection :logical_path)
                       :local-digest (and local-bytes
                                          (gnosis-nodes--digest local-bytes))
                       :remote-digest (plist-get projection :content_sha256)
                       :remote-revision (plist-get projection :revision)
                       :base-file base-file :remote-file remote-file)))
    (make-directory directory t)
    (gnosis-nodes--write-bytes base-file
                               (if-let* ((digest (plist-get base :digest)))
                                   (gnosis-core-blob digest)
                                 ""))
    (gnosis-nodes--write-bytes remote-file (or remote-bytes ""))
    (plist-put state :conflicts
               (vconcat (plist-get state :conflicts) (vector record)))))

(defun gnosis-nodes--apply-document (state projection &optional blobs)
  "Reconcile document PROJECTION into working files and return STATE."
  (let* ((document-id (plist-get projection :document_id))
         (logical-path (plist-get projection :logical_path))
         (remote-digest
          (gnosis-nodes--nullable (plist-get projection :content_sha256)))
         (active (not (equal "tombstone" (plist-get projection :operation))))
         (base (gnosis-nodes--base state document-id logical-path))
         (base-digest (and base (plist-get base :digest)))
         (base-path (and base (plist-get base :logical-path)))
         (working-path (or base-path logical-path))
         (path (gnosis-nodes--safe-path gnosis-nodes-directory working-path))
         (local-bytes (and (file-exists-p path) (gnosis-nodes--read-bytes path)))
         (local-digest (and local-bytes (gnosis-nodes--digest local-bytes)))
         (fingerprint (gnosis-nodes--projection-fingerprint projection)))
    (if (and base (= (plist-get base :revision)
                     (plist-get projection :revision)))
        (progn
          (unless (equal (plist-get base :fingerprint) fingerprint)
            (signal 'gnosis-nodes-error
                    '("Gnosis equal document revision changed projection")))
          state)
      (let* ((remote-bytes
              (and remote-digest
                   (or (cdr (assoc remote-digest blobs))
                       (gnosis-core-blob remote-digest))))
             (plan (if (and base-path (not (equal base-path logical-path)))
                       '(:action :conflict)
                     (if (and active (null remote-digest) local-bytes (null base))
                         '(:action :conflict)
                       (gnosis-nodes--plan-document
                        base-digest local-digest remote-digest active)))))
        (pcase (plist-get plan :action)
          (:materialize
           (gnosis-nodes--write-document logical-path remote-bytes)
           (gnosis-nodes--record-base state projection))
          (:conflict
           (gnosis-nodes--record-conflict
            state base projection local-bytes remote-bytes))
          (:keep
           (if (equal local-digest remote-digest)
               (gnosis-nodes--record-base state projection)
             state)))))))

(defun gnosis-nodes--apply-item (state item &optional blobs)
  "Apply replica ITEM to STATE when it is a document projection."
  (if (equal "document" (plist-get item :entity_kind))
      (gnosis-nodes--apply-document state (plist-get item :projection) blobs)
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
         (if (equal operation "tombstone")
             (and (null digest) (null blob-url) (null parsed))
           (and (listp parsed)
                (eq (null digest) (null blob-url))
                (or (null blob-url) (stringp blob-url)))))))

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

(defun gnosis-nodes--snapshot-page-valid-p (page snapshot-id)
  "Return non-nil when snapshot PAGE continues SNAPSHOT-ID consistently."
  (let ((cursor (gnosis-nodes--nullable (plist-get page :cursor)))
        (next (gnosis-nodes--nullable (plist-get page :next)))
        (items (plist-get page :items)))
    (and (= (or (plist-get page :version) -1) 1)
         (stringp (plist-get page :snapshot_id))
         (or (null snapshot-id)
             (equal snapshot-id (plist-get page :snapshot_id)))
         (vectorp items)
         (seq-every-p #'gnosis-nodes--valid-item-p items)
         (not (eq (null cursor) (null next))))))

(defun gnosis-nodes--change-page-valid-p (page from)
  "Return non-nil when complete change PAGE consistently advances FROM."
  (let ((items (plist-get page :items))
        (next (plist-get page :next))
        cursors)
    (when (vectorp items)
      (setq cursors (mapcar (lambda (item) (plist-get item :cursor))
                            (append items nil))))
    (and (= (or (plist-get page :version) -1) 1)
         (equal from (plist-get page :from))
         (stringp next)
         (vectorp items)
         (seq-every-p (lambda (item) (gnosis-nodes--valid-item-p item t)) items)
         (or (= (length items) 0)
             (equal next (plist-get (aref items (1- (length items))) :cursor)))
         (not (member from cursors))
         (= (length cursors) (length (delete-dups (copy-sequence cursors))))
         (or (not (eq t (plist-get page :has_more)))
             (> (length items) 0)))))

(defun gnosis-nodes--snapshot-spool-file (index)
  "Return private snapshot spool filename for INDEX."
  (expand-file-name (format "snapshot/page-%06d.json" index)
                    (gnosis-nodes--state-directory)))

(defun gnosis-nodes--read-snapshot-page (record)
  "Read checksummed snapshot page from spool RECORD."
  (let* ((file (plist-get record :file))
         (bytes (and (stringp file) (file-readable-p file)
                     (gnosis-nodes--read-bytes file))))
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
        (remaining pages)
        seen
        valid)
    (setq valid t)
    (while (and valid remaining)
      (let* ((page (car remaining))
             (next (gnosis-nodes--nullable (plist-get page :next))))
        (setq valid
              (and (gnosis-nodes--snapshot-page-valid-p page snapshot-id)
                   (not (member next seen))
                   (or (null (cdr remaining)) next)))
        (when next (push next seen))
        (setq snapshot-id (plist-get page :snapshot_id)
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
         (bytes (and (stringp file) (file-readable-p file)
                     (gnosis-nodes--read-bytes file))))
    (unless (and bytes (equal digest (gnosis-nodes--digest bytes)))
      (signal 'gnosis-nodes-error '("Gnosis snapshot blob checksum failed")))
    bytes))

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
                       (digest (gnosis-nodes--nullable
                                (plist-get projection :content_sha256)))
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

(defun gnosis-nodes--snapshot (state &optional expected-fingerprint)
  "Durably fetch, validate, then materialize a full snapshot into STATE.
When EXPECTED-FINGERPRINT is non-nil, reject protocol-shape drift."
  (let* ((resume (plist-get state :snapshot))
         (next (and resume (plist-get resume :next)))
         (pages (and resume (append (plist-get resume :pages) nil)))
         (snapshot-id (and resume (plist-get resume :snapshot-id)))
         (index (length pages))
         (done (and resume (null next)))
         seen
         page)
    (when pages
      (condition-case nil
          (let ((recovered (mapcar #'gnosis-nodes--read-snapshot-page pages)))
            (unless (gnosis-nodes--snapshot-pages-valid-p recovered)
              (signal 'gnosis-nodes-error '("Invalid recovered snapshot")))
            (setq page (car (last recovered))
                  seen (delq nil (mapcar (lambda (item)
                                          (gnosis-nodes--nullable
                                           (plist-get item :next)))
                                        recovered)))
            (dolist (record (append (plist-get resume :blobs) nil))
              (gnosis-nodes--read-snapshot-blob record)))
        (error
         (ignore-errors
           (delete-directory
            (expand-file-name "snapshot" (gnosis-nodes--state-directory)) t))
         (setq state (plist-put state :snapshot nil)
               resume nil next nil pages nil snapshot-id nil index 0 done nil
               page nil seen nil)
         (gnosis-nodes--write-state state))))
    (while (not done)
      (setq page (gnosis-core-sync-snapshot next))
      (unless (gnosis-nodes--snapshot-page-valid-p page snapshot-id)
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
                               :pages (vconcat pages))))
        (gnosis-nodes--write-state state))
      (setq next (gnosis-nodes--nullable (plist-get page :next))
            done (null next)
            index (1+ index)))
    (let ((stored-pages (mapcar #'gnosis-nodes--read-snapshot-page pages)))
      (unless (gnosis-nodes--snapshot-pages-valid-p stored-pages)
        (signal 'gnosis-nodes-error '("Invalid Gnosis snapshot sequence")))
      (let ((fingerprint (gnosis-nodes--snapshot-fingerprint stored-pages)))
        (when (and expected-fingerprint
                   (not (equal fingerprint expected-fingerprint)))
          (signal 'gnosis-nodes-error
                  '("Snapshot protocol fingerprint changed")))
        (setq state (plist-put state :protocol-fingerprint fingerprint)))
      (pcase-let* ((`(,prepared . ,blobs)
                    (gnosis-nodes--snapshot-blobs state stored-pages)))
        (setq state prepared)
        (dolist (stored stored-pages)
          (mapc (lambda (item)
                  (setq state (gnosis-nodes--apply-item state item blobs)))
                (append (plist-get stored :items) nil)))))
    (setq state (plist-put state :cursor (plist-get page :cursor)))
    (plist-put state :snapshot nil)))

(defun gnosis-nodes--pull (state)
  "Pull every change after STATE cursor, committing each validated page."
  (let (page done)
    (while (not done)
      (setq page (gnosis-core-sync-changes (plist-get state :cursor)))
      (unless (gnosis-nodes--change-page-valid-p
               page (plist-get state :cursor))
        (signal 'gnosis-nodes-error '("Invalid Gnosis change page")))
      (mapc (lambda (change)
              (setq state (gnosis-nodes--apply-item state change)))
            (append (plist-get page :items) nil))
      (setq state (plist-put state :cursor (plist-get page :next)))
      (gnosis-nodes--write-state state)
      (setq done (or gnosis-nodes--stop-requested
                     (not (eq t (plist-get page :has_more))))))
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
     ((seq-find (lambda (conflict)
                  (equal document-id (plist-get conflict :document-id)))
                (append (plist-get state :conflicts) nil))
      state)
     ((memq (plist-get head :status) '(:pending :blocked))
      (let* ((outbox (plist-get state :outbox))
             (index (seq-position outbox head)))
        (aset outbox index (gnosis-nodes--entry-with-bytes head bytes))
        state))
     ((memq (plist-get head :status) '(:sending :uncertain :applied))
      (let ((entry
             (gnosis-nodes--outbox-entry
              :replace document-id nil logical-path (gnosis-nodes--digest bytes)
              nil bytes)))
        (setf (plist-get entry :status) :blocked
              (plist-get entry :depends-on) (plist-get head :operation-id))
        (gnosis-nodes--enqueue state entry)))
     ((eq (plist-get head :status) :rejected) state)
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
                entry (plist-put entry :status :pending))
          (aset outbox index entry))))
    state))

(defun gnosis-nodes--enqueue (state entry)
  "Append pending ENTRY to STATE."
  (plist-put state :outbox
             (vconcat (plist-get state :outbox) (vector entry))))

(defun gnosis-nodes--scan (state)
  "Enqueue changed bytes only for explicitly tracked documents in STATE."
  (dolist (base (append (plist-get state :bases) nil) state)
    (let* ((logical-path (plist-get base :logical-path))
           (path (gnosis-nodes--safe-path gnosis-nodes-directory logical-path)))
      (when (and (file-exists-p path)
                 (plist-get base :active))
        (let* ((bytes (gnosis-nodes--read-bytes path))
               (digest (gnosis-nodes--digest bytes)))
          (unless (equal digest (plist-get base :digest))
            (setq state (gnosis-nodes--queue-save state base bytes))))))))

(defun gnosis-nodes--send-entry (entry)
  "Send pending outbox ENTRY once and return mutation response."
  (let ((operation-id (plist-get entry :operation-id))
        (document-id (plist-get entry :document-id))
        (revision (plist-get entry :revision))
        (logical-path (plist-get entry :logical-path))
        (payload (and (plist-get entry :payload)
                      (base64-decode-string (plist-get entry :payload)))))
    (pcase (plist-get entry :kind)
      (:create
       (gnosis-core-document-create
        operation-id logical-path payload))
      (:replace
       (gnosis-core-document-replace
        document-id operation-id revision logical-path payload))
      (:rename
       (gnosis-core-document-rename
        document-id operation-id revision logical-path))
      (:delete
       (gnosis-core-document-tombstone document-id operation-id revision))
      (_ (signal 'gnosis-nodes-error '("Invalid Gnosis pending operation"))))))

(defun gnosis-nodes--send-outbox (state)
  "Send pending operations in STATE once; persist unknown delivery explicitly."
  (let ((outbox (plist-get state :outbox)))
    (catch 'gnosis-nodes--stop-outbox
      (dotimes (index (length outbox))
        (let ((entry (aref outbox index)))
          (when (eq :pending (plist-get entry :status))
            (aset outbox index (gnosis-nodes--outbox-mark entry :sending))
            (gnosis-nodes--write-state state)
            (condition-case nil
                (let ((result (gnosis-nodes--send-entry entry)))
                  (setq entry (plist-put entry :result result))
                  (aset outbox index
                        (gnosis-nodes--outbox-mark
                         entry (if (equal "applied" (plist-get result :status))
                                   :applied :rejected)))
                  (gnosis-nodes--write-state state)
                  (when (equal "rejected" (plist-get result :status))
                    (throw 'gnosis-nodes--stop-outbox nil)))
              (gnosis-core-mutation-uncertain
               (aset outbox index
                     (gnosis-nodes--outbox-mark entry :uncertain))
               (gnosis-nodes--write-state state)
               (throw 'gnosis-nodes--stop-outbox nil))))))))
  state)

(defmacro gnosis-nodes--with-lock (&rest body)
  "Run BODY while holding exclusive private synchronization lock."
  (declare (indent 0) (debug t))
  `(let ((lock (expand-file-name "sync.lock" (gnosis-nodes--state-directory))))
     (make-directory (file-name-directory lock) t)
     (set-file-modes (file-name-directory lock) #o700)
     (condition-case nil
         (lock-file lock)
       (file-locked
        (signal 'gnosis-nodes-error '("Gnosis synchronization is busy"))))
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
  (make-directory gnosis-nodes-directory t)
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
      (when (and existing (plist-get existing :protocol-fingerprint))
        (user-error "Gnosis synchronization is already bootstrapped"))
      (unless (and (equal (plist-get state :server-id)
                          (plist-get remote :server_id))
                   (equal (plist-get state :device-id)
                          (plist-get identity :device_id)))
        (signal 'gnosis-nodes-error '("Gnosis bootstrap identity changed")))
      (unless existing (gnosis-nodes--write-state state))
      (setq state (gnosis-nodes--snapshot state))
      (gnosis-nodes--write-state state)))
  (message "Gnosis Org synchronization bootstrapped"))

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
                        :bases [] :outbox [] :conflicts [])))
      (unless (stringp (plist-get trusted :protocol-fingerprint))
        (user-error "Gnosis trusted protocol fingerprint is missing"))
      (unless (and (equal (plist-get trusted :server-id)
                          (plist-get fresh :server-id))
                   (equal (plist-get trusted :device-id)
                          (plist-get fresh :device-id)))
        (signal 'gnosis-nodes-error '("Gnosis reset identity changed")))
      (let ((gnosis-nodes-state-file temporary-file))
        (unwind-protect
            (progn
              (gnosis-nodes--write-state fresh)
              (setq fresh
                    (gnosis-nodes--snapshot
                     fresh (plist-get trusted :protocol-fingerprint)))
              (let ((gnosis-nodes-state-file original-file))
                (gnosis-nodes--write-state fresh)))
          (when (file-exists-p temporary-file)
            (delete-file temporary-file))))))
  (message "Gnosis Org synchronization metadata reset"))

;;;###autoload
(defun gnosis-nodes-sync ()
  "Pull authoritative change pages, then send pending operations once."
  (interactive)
  (when gnosis-nodes--busy
    (user-error "Gnosis synchronization is already running"))
  (let ((gnosis-nodes--busy t))
    (setq gnosis-nodes--stop-requested nil)
    (gnosis-nodes--with-lock
      (let ((state (gnosis-nodes--read-state)))
        (when (or (plist-get state :snapshot)
                  (null (plist-get state :protocol-fingerprint)))
          (user-error "Gnosis bootstrap is incomplete; run gnosis-nodes-bootstrap"))
        (gnosis-nodes--assert-identity state)
        (unless gnosis-nodes--stop-requested
          (setq state (gnosis-nodes--pull state)))
        (unless gnosis-nodes--stop-requested
          (setq state (gnosis-nodes--scan state))
          (gnosis-nodes--write-state state)
          (gnosis-nodes--send-outbox state)))))
  (message (if gnosis-nodes--stop-requested
               "Gnosis Org synchronization stopped"
             "Gnosis Org synchronization complete")))

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

(defun gnosis-nodes--queue-current-buffer ()
  "Durably queue saved tracked buffer bytes."
  (when (and gnosis-nodes-mode buffer-file-name
             (file-in-directory-p buffer-file-name gnosis-nodes-directory))
    (gnosis-nodes--with-lock
      (let* ((state (gnosis-nodes--read-state))
             (logical-path (file-relative-name buffer-file-name
                                               gnosis-nodes-directory))
             (base (gnosis-nodes--base state nil logical-path)))
        (when base
          (let* ((bytes (gnosis-nodes--read-bytes buffer-file-name))
                 (digest (gnosis-nodes--digest bytes)))
            (unless (equal digest (plist-get base :digest))
              (setq state (gnosis-nodes--queue-save state base bytes))
              (gnosis-nodes--write-state state))))))))

;;;###autoload
(define-minor-mode gnosis-nodes-mode
  "Durably queue saves for one explicitly tracked Org document."
  :lighter " Gnosis"
  (if gnosis-nodes-mode
      (progn
        (unless (and buffer-file-name
                     (file-in-directory-p buffer-file-name gnosis-nodes-directory))
          (setq gnosis-nodes-mode nil)
          (user-error "Buffer is outside `gnosis-nodes-directory'"))
        (let* ((state (gnosis-nodes--read-state))
               (logical-path (file-relative-name buffer-file-name
                                                 gnosis-nodes-directory)))
          (unless (or (gnosis-nodes--base state nil logical-path)
                      (gnosis-nodes--outbox-entry-for state nil logical-path))
            (gnosis-nodes-track-file buffer-file-name)))
        (add-hook 'after-save-hook #'gnosis-nodes--queue-current-buffer nil t))
    (remove-hook 'after-save-hook #'gnosis-nodes--queue-current-buffer t)))

(defun gnosis-nodes--idle-sync ()
  "Run idle synchronization without surfacing expected user errors."
  (unless gnosis-nodes--busy
    (condition-case err
        (gnosis-nodes-sync)
      (error (message "%s" (error-message-string err))))))

;;;###autoload
(define-minor-mode global-gnosis-nodes-mode
  "Synchronize Gnosis Org documents on idle timer."
  :global t :lighter nil
  (when (timerp gnosis-nodes--idle-timer)
    (cancel-timer gnosis-nodes--idle-timer))
  (setq gnosis-nodes--idle-timer
        (and global-gnosis-nodes-mode
             (run-with-idle-timer gnosis-nodes-idle-seconds t
                                  #'gnosis-nodes--idle-sync))))

;;;###autoload
(defun gnosis-nodes-track-file (file)
  "Explicitly enroll Org FILE as a new synchronized document."
  (interactive "fTrack Org file: ")
  (let* ((file (expand-file-name file))
         (logical-path (file-relative-name file gnosis-nodes-directory)))
    (unless (and (file-regular-p file)
                 (file-in-directory-p file gnosis-nodes-directory))
      (user-error "File must be below `gnosis-nodes-directory'"))
    (gnosis-nodes--safe-path gnosis-nodes-directory logical-path)
    (gnosis-nodes--with-lock
      (let ((state (gnosis-nodes--read-state)))
        (when (or (gnosis-nodes--base state nil logical-path)
                  (gnosis-nodes--outbox-entry-for state nil logical-path))
          (user-error "Org file is already tracked"))
        (let* ((bytes (gnosis-nodes--read-bytes file))
               (digest (gnosis-nodes--digest bytes)))
          (setq state
                (gnosis-nodes--enqueue
                 state (gnosis-nodes--outbox-entry
                        :create nil nil logical-path digest nil bytes)))
          (gnosis-nodes--write-state state))))))

;;;###autoload
(defun gnosis-nodes-track-all-files ()
  "Explicitly enroll every Org file below `gnosis-nodes-directory'."
  (interactive)
  (unless (yes-or-no-p "Track every Org file below Gnosis nodes directory? ")
    (user-error "Gnosis all-file enrollment cancelled"))
  (gnosis-nodes--with-lock
    (let ((state (gnosis-nodes--read-state)))
      (dolist (file (sort (seq-filter
                           (lambda (candidate)
                             (and (not (file-symlink-p candidate))
                                  (condition-case nil
                                      (progn
                                        (gnosis-nodes--safe-path
                                         gnosis-nodes-directory
                                         (file-relative-name
                                          candidate gnosis-nodes-directory))
                                        t)
                                    (gnosis-nodes-path-error nil))))
                           (directory-files-recursively
                            gnosis-nodes-directory "\\.org\\'"))
                          #'string<))
        (let* ((logical-path (file-relative-name file gnosis-nodes-directory))
               (bytes (gnosis-nodes--read-bytes file)))
          (unless (or (gnosis-nodes--base state nil logical-path)
                      (gnosis-nodes--outbox-entry-for state nil logical-path))
            (setq state
                  (gnosis-nodes--enqueue
                   state (gnosis-nodes--outbox-entry
                          :create nil nil logical-path
                          (gnosis-nodes--digest bytes) nil bytes))))))
      (gnosis-nodes--write-state state))))

;;;###autoload
(defun gnosis-nodes-rename-file (file new-logical-path)
  "Explicitly queue tracked FILE rename to NEW-LOGICAL-PATH."
  (interactive "fTracked Org file: \nsNew logical path: ")
  (gnosis-nodes--safe-path gnosis-nodes-directory new-logical-path)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (old (file-relative-name (expand-file-name file)
                                    gnosis-nodes-directory))
           (base (gnosis-nodes--base state nil old)))
      (unless base (user-error "Org file is not tracked"))
      (setq state
            (gnosis-nodes--enqueue
             state (gnosis-nodes--outbox-entry
                    :rename (plist-get base :document-id)
                    (plist-get base :revision) new-logical-path
                    (plist-get base :digest) old)))
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
                                             gnosis-nodes-directory))
           (base (gnosis-nodes--base state nil logical-path)))
      (unless base (user-error "Org file is not tracked"))
      (setq state
            (gnosis-nodes--enqueue
             state (gnosis-nodes--outbox-entry
                    :delete (plist-get base :document-id)
                    (plist-get base :revision) logical-path nil)))
      (gnosis-nodes--write-state state))))

;;;###autoload
(defun gnosis-nodes-resolve-conflict ()
  "Open first recorded conflict in three-way Ediff merge."
  (interactive)
  (let* ((state (gnosis-nodes--read-state))
         (record (and (> (length (plist-get state :conflicts)) 0)
                      (aref (plist-get state :conflicts) 0))))
    (unless record (user-error "No Gnosis Org conflicts"))
    (let* ((local (gnosis-nodes--safe-path
                   gnosis-nodes-directory
                   (or (plist-get record :local-path)
                       (plist-get record :logical-path))))
           (local-buffer (find-file-noselect local))
           (remote-buffer (find-file-noselect (plist-get record :remote-file)))
           (base-buffer (find-file-noselect (plist-get record :base-file))))
      (require 'ediff)
      (ediff-merge-buffers-with-ancestor
       local-buffer remote-buffer base-buffer nil nil local))))

(defun gnosis-nodes--remote-conflict-current-p (state record)
  "Return non-nil when RECORD remote head remains current after STATE cursor."
  (let ((cursor (plist-get state :cursor)) (current t) page done)
    (while (and current (not done))
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
                              (gnosis-nodes--nullable
                               (plist-get projection :content_sha256))))))))
      (setq cursor (plist-get page :next)
            done (not (eq t (plist-get page :has_more)))))
    current))

;;;###autoload
(defun gnosis-nodes-accept-conflict ()
  "Accept current local merge for first conflict as revisioned replacement.
Revalidate local merged bytes and remote head before durable queueing."
  (interactive)
  (gnosis-nodes--with-lock
    (let* ((state (gnosis-nodes--read-state))
           (conflicts (plist-get state :conflicts))
           (record (and (> (length conflicts) 0) (aref conflicts 0))))
      (unless record (user-error "No Gnosis Org conflicts"))
      (let* ((logical-path (plist-get record :logical-path))
             (path (gnosis-nodes--safe-path
                    gnosis-nodes-directory
                    (or (plist-get record :local-path) logical-path)))
             (bytes (and (file-exists-p path) (gnosis-nodes--read-bytes path)))
             (digest (and bytes (gnosis-nodes--digest bytes))))
        (unless bytes (user-error "Local merge file no longer exists"))
        (unless (gnosis-nodes--remote-conflict-current-p state record)
          (user-error "Remote conflict head changed; synchronize again"))
        (let ((entry
               (gnosis-nodes--outbox-entry
                :replace (plist-get record :document-id)
                (plist-get record :remote-revision) logical-path digest nil bytes)))
          (setq state (gnosis-nodes--enqueue state entry))
          (setf (plist-get record :accepted-digest) digest
                (plist-get record :operation-id) (plist-get entry :operation-id)
                (plist-get record :status) :queued)
          (aset conflicts 0 record))
        (gnosis-nodes--write-state state)
        (message "Gnosis conflict merge queued")))))

(provide 'gnosis-nodes)
;;; gnosis-nodes.el ends here
