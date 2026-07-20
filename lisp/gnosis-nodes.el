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
When nil, use `.gnosis-sync/state.json' below `gnosis-nodes-directory'."
  :type '(choice (const :tag "Below synchronization root" nil) file)
  :group 'gnosis-nodes)

(define-error 'gnosis-nodes-error "Gnosis Org synchronization failed")
(define-error 'gnosis-nodes-path-error "Unsafe Gnosis Org path" 'gnosis-nodes-error)
(define-error 'gnosis-nodes-conflict "Gnosis Org conflict" 'gnosis-nodes-error)

(defun gnosis-nodes--state-file ()
  "Return absolute synchronization state filename."
  (expand-file-name
   (or gnosis-nodes-state-file ".gnosis-sync/state.json")
   gnosis-nodes-directory))

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
  (let ((root (file-name-as-directory (expand-file-name gnosis-nodes-directory))))
    (when (gnosis-nodes--symlink-component-p root path)
      (signal 'gnosis-nodes-path-error (list path)))
    (make-directory (file-name-directory path) t)
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
  (gnosis-nodes--write-bytes
   (gnosis-nodes--safe-path gnosis-nodes-directory logical-path)
   (string-to-unibyte bytes)))

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
    (list :action (if (equal local base) :delete :conflict)))
   ((equal local base)
    (list :action (if (equal remote base) :keep :materialize)))
   ((equal remote base) '(:action :keep))
   ((equal local remote) '(:action :keep))
   (t '(:action :conflict))))

(cl-defun gnosis-nodes--outbox-entry
    (kind document-id revision logical-path digest &optional previous-path)
  "Return durable pending operation for KIND and DOCUMENT-ID metadata.
REVISION, LOGICAL-PATH, DIGEST, and PREVIOUS-PATH describe expected state."
  (list :operation-id (gnosis-core-operation-id)
        :kind kind
        :document-id document-id
        :revision revision
        :logical-path logical-path
        :previous-path previous-path
        :digest digest
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

(defun gnosis-nodes--read-state ()
  "Read and validate minimal durable synchronization state."
  (let ((file (gnosis-nodes--state-file)))
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
                     (stringp (plist-get state :cursor))
                     (vectorp (plist-get state :bases))
                     (vectorp (plist-get state :outbox))
                     (vectorp (plist-get state :conflicts)))
          (signal 'gnosis-nodes-error '("Invalid Gnosis synchronization state")))
        (gnosis-nodes--normalize-state state)))))

(defun gnosis-nodes--write-state (state)
  "Atomically persist minimal synchronization STATE."
  (let* ((file (gnosis-nodes--state-file))
         (directory (file-name-directory file)))
    (make-directory directory t)
    (let ((temporary (make-temp-file (expand-file-name ".state-" directory))))
      (unwind-protect
          (progn
            (with-temp-file temporary
              (insert
               (json-serialize
                (gnosis-nodes--json-state state)
                :null-object nil :false-object nil)))
            (set-file-modes temporary #o600)
            (rename-file temporary file t))
        (when (file-exists-p temporary)
          (delete-file temporary))))))

(defun gnosis-nodes--json-state (state)
  "Return copy of STATE containing only JSON values."
  (let ((copy (copy-tree state t)))
    (let ((outbox (plist-get copy :outbox)))
      (when (vectorp outbox)
        (dotimes (index (length outbox))
          (let ((entry (aref outbox index)))
            (dolist (key '(:kind :status))
              (when-let* ((value (plist-get entry key))
                          ((keywordp value)))
                (setf (plist-get entry key)
                      (substring (symbol-name value) 1))))))))
    copy))

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
                     :active (not (null
                                   (gnosis-nodes--nullable
                                    (plist-get projection :content_sha256)))))))
    (plist-put state :bases
               (gnosis-nodes--replace-vector-item
                (plist-get state :bases) old base))))

(defun gnosis-nodes--conflict-directory (document-id)
  "Return private sidecar directory for DOCUMENT-ID."
  (expand-file-name
   (concat ".gnosis-sync/conflicts/"
           (secure-hash 'sha256 document-id))
   gnosis-nodes-directory))

(defun gnosis-nodes--record-conflict (state base projection local-bytes remote-bytes)
  "Persist conflict sidecars and return updated STATE.
BASE and PROJECTION identify versions; LOCAL-BYTES remains in its working file.
REMOTE-BYTES contains validated authoritative content, or nil for deletion."
  (let* ((document-id (plist-get projection :document_id))
         (directory (gnosis-nodes--conflict-directory document-id))
         (base-file (expand-file-name "base.org" directory))
         (remote-file (expand-file-name "remote.org" directory))
         (record (list :document-id document-id
                       :logical-path (plist-get base :logical-path)
                       :local-digest (gnosis-nodes--digest local-bytes)
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

(defun gnosis-nodes--apply-document (state projection)
  "Reconcile document PROJECTION into working files and return STATE."
  (let* ((document-id (plist-get projection :document_id))
         (logical-path (plist-get projection :logical_path))
         (remote-digest
          (gnosis-nodes--nullable (plist-get projection :content_sha256)))
         (active (not (null remote-digest)))
         (base (gnosis-nodes--base state document-id logical-path))
         (path (gnosis-nodes--safe-path gnosis-nodes-directory logical-path))
         (local-bytes (and (file-exists-p path) (gnosis-nodes--read-bytes path)))
         (local-digest (and local-bytes (gnosis-nodes--digest local-bytes)))
         (plan (gnosis-nodes--plan-document
                (plist-get base :digest) local-digest remote-digest active)))
    (pcase (plist-get plan :action)
      (:materialize
       (gnosis-nodes--write-document logical-path (gnosis-core-blob remote-digest))
       (gnosis-nodes--record-base state projection))
      (:delete
       (when (file-exists-p path) (delete-file path))
       (gnosis-nodes--record-base state projection))
      (:conflict
       (let ((remote (and remote-digest (gnosis-core-blob remote-digest))))
         (gnosis-nodes--record-conflict state base projection local-bytes remote)))
      (:keep
       (if (equal local-digest remote-digest)
           (gnosis-nodes--record-base state projection)
         state)))))

(defun gnosis-nodes--apply-item (state item)
  "Apply replica ITEM to STATE when it is a document projection."
  (if (equal "document" (plist-get item :entity_kind))
      (gnosis-nodes--apply-document state (plist-get item :projection))
    state))

(defun gnosis-nodes--snapshot (state)
  "Materialize full snapshot into STATE and return it."
  (let ((next nil) page done)
    (while (not done)
      (setq page (gnosis-core-sync-snapshot next))
      (mapc (lambda (item) (setq state (gnosis-nodes--apply-item state item)))
            (append (plist-get page :items) nil))
      (setq next (gnosis-nodes--nullable (plist-get page :next))
            done (null next)))
    (plist-put state :cursor (plist-get page :cursor))))

(defun gnosis-nodes--pull (state)
  "Pull every change after STATE cursor, committing each validated page."
  (let (page done)
    (while (not done)
      (setq page (gnosis-core-sync-changes (plist-get state :cursor)))
      (unless (equal (plist-get state :cursor) (plist-get page :from))
        (signal 'gnosis-nodes-error '("Gnosis change cursor mismatch")))
      (mapc
       (lambda (change)
         (setq state (gnosis-nodes--apply-item state change)))
       (append (plist-get page :items) nil))
      (setq state (plist-put state :cursor (plist-get page :next)))
      (gnosis-nodes--write-state state)
      (setq done (not (eq t (plist-get page :has_more)))))
    state))

(defun gnosis-nodes--outbox-covers-p (state document-id logical-path)
  "Return non-nil when STATE already tracks DOCUMENT-ID or LOGICAL-PATH."
  (seq-some
   (lambda (entry)
     (and (memq (plist-get entry :status) '(:pending :sending :uncertain))
          (or (and document-id
                   (equal document-id (plist-get entry :document-id)))
              (equal logical-path (plist-get entry :logical-path)))))
   (append (plist-get state :outbox) nil)))

(defun gnosis-nodes--enqueue (state entry)
  "Append pending ENTRY to STATE."
  (plist-put state :outbox
             (vconcat (plist-get state :outbox) (vector entry))))

(defun gnosis-nodes--scan (state)
  "Scan working Org files and enqueue local edits in STATE."
  (let ((known-paths
         (mapcar (lambda (base) (plist-get base :logical-path))
                 (append (plist-get state :bases) nil)))
        (consumed-paths nil)
        (files nil))
    (dolist (path (directory-files-recursively
                   gnosis-nodes-directory "\\.org\\'" nil nil t))
      (when (gnosis-nodes--symlink-component-p gnosis-nodes-directory path)
        (signal 'gnosis-nodes-path-error (list path)))
      (let ((logical-path (file-relative-name path gnosis-nodes-directory)))
        (unless (string-prefix-p ".gnosis-sync/" logical-path)
          (push (list :logical-path logical-path
                      :digest (gnosis-nodes--digest
                               (gnosis-nodes--read-bytes path)))
                files))))
    (mapc
     (lambda (base)
       (let* ((document-id (plist-get base :document-id))
              (logical-path (plist-get base :logical-path))
              (path (gnosis-nodes--safe-path
                     gnosis-nodes-directory logical-path))
              (rename-candidates
               (and (not (file-exists-p path))
                    (seq-filter
                     (lambda (file)
                       (and (equal (plist-get file :digest)
                                   (plist-get base :digest))
                            (not (member (plist-get file :logical-path)
                                         consumed-paths))
                            (not (member (plist-get file :logical-path)
                                         known-paths))))
                     files)))
              (rename (and (= (length rename-candidates) 1)
                           (car rename-candidates))))
         (unless (gnosis-nodes--outbox-covers-p
                  state document-id logical-path)
           (cond
            (rename
             (push (plist-get rename :logical-path) consumed-paths)
             (setq state
                   (gnosis-nodes--enqueue
                    state
                    (gnosis-nodes--outbox-entry
                     :rename document-id (plist-get base :revision)
                     (plist-get rename :logical-path)
                     (plist-get rename :digest)))))
            ((file-exists-p path)
               (let ((digest (gnosis-nodes--digest
                              (gnosis-nodes--read-bytes path))))
                 (unless (equal digest (plist-get base :digest))
                   (setq state
                         (gnosis-nodes--enqueue
                          state
                          (gnosis-nodes--outbox-entry
                           :replace document-id (plist-get base :revision)
                           logical-path digest))))))
            ((plist-get base :active)
             (setq state
                   (gnosis-nodes--enqueue
                    state
                    (gnosis-nodes--outbox-entry
                     :delete document-id (plist-get base :revision)
                     logical-path nil))))))))
     (append (plist-get state :bases) nil))
    (dolist (file files)
      (let ((logical-path (plist-get file :logical-path)))
        (unless (or (member logical-path known-paths)
                    (member logical-path consumed-paths)
                    (gnosis-nodes--outbox-covers-p state nil logical-path))
          (setq state
                (gnosis-nodes--enqueue
                 state
                 (gnosis-nodes--outbox-entry
                  :create nil nil logical-path
                  (plist-get file :digest)))))))
    state))

(defun gnosis-nodes--send-entry (entry)
  "Send pending outbox ENTRY once and return mutation response."
  (let ((operation-id (plist-get entry :operation-id))
        (document-id (plist-get entry :document-id))
        (revision (plist-get entry :revision))
        (logical-path (plist-get entry :logical-path)))
    (pcase (plist-get entry :kind)
      (:create
       (gnosis-core-document-create
        operation-id logical-path
        (gnosis-nodes--read-bytes
         (gnosis-nodes--safe-path gnosis-nodes-directory logical-path))))
      (:replace
       (gnosis-core-document-replace
        document-id operation-id revision logical-path
        (gnosis-nodes--read-bytes
         (gnosis-nodes--safe-path gnosis-nodes-directory logical-path))))
      (:rename
       (gnosis-core-document-rename
        document-id operation-id revision logical-path))
      (:delete
       (gnosis-core-document-tombstone document-id operation-id revision))
      (_ (signal 'gnosis-nodes-error '("Invalid Gnosis pending operation"))))))

(defun gnosis-nodes--send-outbox (state)
  "Send pending operations in STATE once; persist unknown delivery explicitly."
  (let ((outbox (plist-get state :outbox)))
    (dotimes (index (length outbox))
      (let ((entry (aref outbox index)))
        (when (eq :pending (plist-get entry :status))
          (aset outbox index (gnosis-nodes--outbox-mark entry :sending))
          (gnosis-nodes--write-state state)
          (condition-case nil
              (let ((result (gnosis-nodes--send-entry entry)))
                (if (equal "applied" (plist-get result :status))
                    (progn
                      (aset outbox index
                            (gnosis-nodes--outbox-mark entry :applied))
                      (setq state
                            (gnosis-nodes--apply-document
                             state (plist-get result :authoritative)))
                      (setq state
                            (plist-put state :cursor
                                       (plist-get result :cursor))))
                  (aset outbox index
                        (gnosis-nodes--outbox-mark entry :rejected))))
            (gnosis-core-mutation-uncertain
             (aset outbox index (gnosis-nodes--outbox-mark entry :uncertain))))
          (gnosis-nodes--write-state state))))
  state))

;;;###autoload
(defun gnosis-nodes-bootstrap ()
  "Bootstrap an empty Org synchronization directory from Gnosis."
  (interactive)
  (when (file-exists-p (gnosis-nodes--state-file))
    (user-error "Gnosis synchronization is already bootstrapped"))
  (make-directory gnosis-nodes-directory t)
  (let* ((identity (gnosis-core-sync-identity))
         (remote (gnosis-core-sync-state))
         (state (list :version 1
                      :server-id (plist-get remote :server_id)
                      :device-id (plist-get identity :device_id)
                      :cursor (plist-get remote :cursor)
                      :bases [] :outbox [] :conflicts [])))
    (setq state (gnosis-nodes--snapshot state))
    (gnosis-nodes--write-state state)
    (message "Gnosis Org synchronization bootstrapped")))

;;;###autoload
(defun gnosis-nodes-sync ()
  "Pull authoritative change pages, then send pending operations once."
  (interactive)
  (let* ((state (gnosis-nodes--read-state))
         (identity (gnosis-core-sync-identity))
         (remote (gnosis-core-sync-state)))
    (unless (and (equal (plist-get state :device-id)
                        (plist-get identity :device_id))
                 (equal (plist-get state :server-id)
                        (plist-get remote :server_id)))
      (signal 'gnosis-nodes-error '("Gnosis synchronization identity changed")))
    (setq state (gnosis-nodes--pull state))
    (setq state (gnosis-nodes--scan state))
    (gnosis-nodes--write-state state)
    (gnosis-nodes--send-outbox state)
    (message "Gnosis Org synchronization complete")))

;;;###autoload
(defun gnosis-nodes-resolve-conflict ()
  "Open first recorded conflict in Ediff without accepting it implicitly."
  (interactive)
  (let* ((state (gnosis-nodes--read-state))
         (record (and (> (length (plist-get state :conflicts)) 0)
                      (aref (plist-get state :conflicts) 0))))
    (unless record (user-error "No Gnosis Org conflicts"))
    (let ((local (gnosis-nodes--safe-path
                  gnosis-nodes-directory (plist-get record :logical-path))))
      (require 'ediff)
      (ediff-files3 local (plist-get record :remote-file)
                    (plist-get record :base-file)))))

;;;###autoload
(defun gnosis-nodes-accept-conflict ()
  "Accept current local merge for first conflict as revisioned replacement.
Refuse when local content changed since conflict capture.  Remote staleness is
guarded by the mutation's exact If-Match revision."
  (interactive)
  (let* ((state (gnosis-nodes--read-state))
         (conflicts (plist-get state :conflicts))
         (record (and (> (length conflicts) 0) (aref conflicts 0))))
    (unless record (user-error "No Gnosis Org conflicts"))
    (let* ((logical-path (plist-get record :logical-path))
           (path (gnosis-nodes--safe-path gnosis-nodes-directory logical-path))
           (digest (and (file-exists-p path)
                        (gnosis-nodes--digest (gnosis-nodes--read-bytes path)))))
      (unless (equal digest (plist-get record :local-digest))
        (user-error "Local conflict content changed; synchronize again"))
      (setq state
            (gnosis-nodes--enqueue
             state
             (gnosis-nodes--outbox-entry
              :replace (plist-get record :document-id)
              (plist-get record :remote-revision) logical-path digest)))
      (plist-put state :conflicts (vconcat (cdr (append conflicts nil))))
      (gnosis-nodes--write-state state)
      (message "Gnosis conflict merge queued"))))

(provide 'gnosis-nodes)
;;; gnosis-nodes.el ends here
