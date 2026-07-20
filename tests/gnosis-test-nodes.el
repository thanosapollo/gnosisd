;;; gnosis-test-nodes.el --- Org synchronization tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-nodes)

(defun gnosis-test-nodes--cursor (server-id position &optional snapshot-id)
  "Return protocol cursor for SERVER-ID, POSITION, and optional SNAPSHOT-ID."
  (let* ((json (json-serialize
                (if snapshot-id
                    `(:version 1 :snapshot_id ,snapshot-id
                      :server_id ,server-id :cut_cursor 9
                      :cut_token "token-9" :after ,position)
                  `(:version 1 :server_id ,server-id :position ,position
                    :token ,(format "token-%s" position)))))
         (encoded (base64-encode-string (encode-coding-string json 'utf-8) t)))
    (replace-regexp-in-string
     "=+\\'" ""
     (replace-regexp-in-string
      "/" "_" (replace-regexp-in-string "+" "-" encoded t t) t t))))

(defun gnosis-test-nodes--encode-cursor-json (json)
  "Return unpadded URL-safe cursor token for JSON bytes."
  (replace-regexp-in-string
   "=+\\'" ""
   (replace-regexp-in-string
    "/" "_"
    (replace-regexp-in-string "+" "-" (base64-encode-string json t) t t)
    t t)))


(defvar gnosis-nodes-max-pages-per-pass)
(defvar gnosis-nodes-max-local-scan-per-pass)
(defvar gnosis-nodes-dir)

(ert-deftest gnosis-nodes-rejects-unsafe-logical-paths ()
  (dolist (path '("/absolute.org" "../escape.org" "dir/../../escape.org"
                  "note.txt" "a//b.org" "./note.org"))
    (should-error (gnosis-nodes--safe-path "/tmp/notes" path)
                  :type 'gnosis-nodes-path-error))
  (should (equal "/tmp/notes/dir/note.org"
                 (gnosis-nodes--safe-path "/tmp/notes" "dir/note.org")))
  (let ((root (make-temp-file "gnosis-root-" t))
        (outside (make-temp-file "gnosis-outside-" t)))
    (unwind-protect
        (progn
          (make-symbolic-link outside (expand-file-name "linked" root))
          (should-error (gnosis-nodes--safe-path root "linked/note.org")
                        :type 'gnosis-nodes-path-error))
      (delete-directory root t)
      (delete-directory outside t))))

(ert-deftest gnosis-nodes-plans-fast-forward-only-for-clean-base ()
  (should
   (equal '(:action :materialize)
          (gnosis-nodes--plan-document "base" "base" "remote" t)))
  (should
   (equal '(:action :keep)
          (gnosis-nodes--plan-document "base" "local" "base" t)))
  (should
   (equal '(:action :conflict)
          (gnosis-nodes--plan-document "base" "local" "remote" t)))
  (should
   (equal '(:action :conflict)
          (gnosis-nodes--plan-document "base" "base" nil nil)))
  (should
   (equal '(:action :conflict)
          (gnosis-nodes--plan-document "base" "local" nil nil))))

(ert-deftest gnosis-nodes-outbox-keeps-stable-operation-id ()
  (let* ((entry (gnosis-nodes--outbox-entry
                 :replace "doc" 3 "note.org" "digest"))
         (operation-id (plist-get entry :operation-id)))
    (should (equal :pending (plist-get entry :status)))
    (should (equal operation-id
                   (plist-get (gnosis-nodes--outbox-mark entry :uncertain)
                              :operation-id)))
    (should (equal :uncertain
                   (plist-get (gnosis-nodes--outbox-mark entry :uncertain)
                              :status)))))

(ert-deftest gnosis-nodes-state-round-trips-minimal-data ()
  (let* ((directory (make-temp-file "gnosis-nodes-state" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "cursor" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (let ((stored (gnosis-nodes--read-state)))
            (should (equal (plist-get stored :notes-root)
                           (file-truename (file-name-as-directory directory))))
            (should (equal (plist-get stored :service-authority)
                           (gnosis-core--validated-base-url)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-refuses-symlink-materialization ()
  (let* ((directory (make-temp-file "gnosis-nodes-root" t))
         (outside (make-temp-file "gnosis-nodes-outside"))
         (gnosis-nodes-dir directory)
         (path (expand-file-name "note.org" directory)))
    (unwind-protect
        (progn
          (make-symbolic-link outside path)
          (should-error (gnosis-nodes--write-document "note.org" "body")
                        :type 'gnosis-nodes-path-error))
      (delete-directory directory t)
      (delete-file outside))))

(ert-deftest gnosis-nodes-delivery-unknown-remains-durable ()
  (let* ((directory (make-temp-file "gnosis-nodes-unknown" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (entry (gnosis-nodes--outbox-entry
                 :replace "doc" 1 "note.org" "digest"))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "cursor" :bases [] :outbox [,entry] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" "body")
          (gnosis-nodes--write-state state)
          (cl-letf (((symbol-function 'gnosis-core-document-replace)
                     (lambda (&rest _)
                       (signal 'gnosis-core-mutation-uncertain '("timeout")))))
            (gnosis-nodes--send-outbox state))
          (should
           (equal :uncertain
                  (plist-get
                   (aref (plist-get (gnosis-nodes--read-state) :outbox) 0)
                   :status))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-scan-only-enqueues-explicitly-tracked-edits ()
  (let* ((directory (make-temp-file "gnosis-nodes-scan" t))
         (gnosis-nodes-dir directory)
         (state
          '(:version 1 :server-id "server" :device-id "device"
            :cursor "cursor"
            :bases [(:document-id "changed-doc" :revision 2
                     :logical-path "changed.org" :digest "old" :active t)
                    (:document-id "deleted-doc" :revision 4
                     :logical-path "deleted.org" :digest "gone" :active t)]
            :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "changed.org" "changed")
          (gnosis-nodes--write-document "fresh.org" "fresh")
          (cl-letf (((symbol-function 'gnosis-core-blob)
                     (lambda (_) "deleted"))
                    ((symbol-function 'gnosis-nodes--record-conflict)
                     (lambda (current &rest _)
                       (plist-put current :conflicts [(:kind :missing-local)]))))
            (setq state (gnosis-nodes--scan state)))
          (should
           (equal '(:replace)
                  (mapcar (lambda (entry) (plist-get entry :kind))
                          (append (plist-get state :outbox) nil))))
          (should (= 1 (length (plist-get state :conflicts)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-scan-never-infers-rename-from-file-layout ()
  (let* ((root (make-temp-file "gnosis-nodes-rename-" t))
         (gnosis-nodes-dir root)
         (bytes (string-to-unibyte "same"))
         (digest (secure-hash 'sha256 bytes))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "cursor"
                  :bases [(:document-id "doc" :revision 4
                           :logical-path "old.org" :digest ,digest :active t)]
                  :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "new.org" bytes)
          (cl-letf (((symbol-function 'gnosis-core-blob) (lambda (_) bytes))
                    ((symbol-function 'gnosis-nodes--record-conflict)
                     (lambda (current &rest _)
                       (plist-put current :conflicts [(:kind :missing-local)]))))
            (setq state (gnosis-nodes--scan state)))
          (should (= 0 (length (plist-get state :outbox))))
          (should (= 1 (length (plist-get state :conflicts)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-outbox-replays-captured-bytes-and-stops-on-uncertain ()
  (let* ((directory (make-temp-file "gnosis-nodes-replay" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (first (gnosis-nodes--outbox-entry
                 :replace "one" 1 "one.org" "old" nil "captured"))
         (second (gnosis-nodes--outbox-entry
                  :replace "two" 1 "two.org" "old" nil "second"))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [,first ,second]
                  :conflicts []))
         calls)
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (cl-letf (((symbol-function 'gnosis-core-document-replace)
                     (lambda (_document _operation _revision _path bytes)
                       (push bytes calls)
                       (signal 'gnosis-core-mutation-uncertain '("timeout")))))
            (gnosis-nodes--send-outbox state))
          (should (equal '("captured") calls))
          (should (equal "feed" (plist-get (gnosis-nodes--read-state) :cursor))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-rejected-mutation-stops-outbox ()
  (let* ((directory (make-temp-file "gnosis-nodes-rejected" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (first (gnosis-nodes--outbox-entry :replace "one" 1 "one.org" "d" nil "one"))
         (second (gnosis-nodes--outbox-entry :replace "two" 1 "two.org" "d" nil "two"))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [,first ,second] :conflicts []))
         (calls 0))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (cl-letf (((symbol-function 'gnosis-core-document-replace)
                     (lambda (&rest _)
                       (cl-incf calls)
                       '(:status "rejected" :cursor "hint"))))
            (gnosis-nodes--send-outbox state))
          (should (= calls 1))
          (should (equal "feed" (plist-get (gnosis-nodes--read-state) :cursor))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-pull-validates-whole-page-before-effects ()
  (let ((state '(:version 1 :server-id "server" :device-id "device"
                 :cursor "from" :bases [] :outbox [] :conflicts []))
        applied)
    (cl-letf (((symbol-function 'gnosis-core-sync-changes)
               (lambda (_)
                 '(:version 1 :from "from" :next "next" :has_more nil
                   :items [(:cursor "wrong" :entity_kind "document")])) )
              ((symbol-function 'gnosis-nodes--apply-item)
               (lambda (&rest _) (setq applied t))))
      (should-error (gnosis-nodes--pull state) :type 'gnosis-nodes-error)
      (should-not applied))))

(ert-deftest gnosis-nodes-protects-modified-visiting-buffer ()
  (let* ((directory (make-temp-file "gnosis-nodes-buffer" t))
         (gnosis-nodes-dir directory)
         (path (expand-file-name "note.org" directory))
         buffer)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" "old")
          (setq buffer (find-file-noselect path))
          (with-current-buffer buffer
            (goto-char (point-max))
            (insert "modified"))
          (should-error (gnosis-nodes--write-document "note.org" "remote")
                        :type 'gnosis-nodes-conflict)
          (should (equal "old" (gnosis-nodes--read-bytes path))))
      (when (buffer-live-p buffer) (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-save-mode-only-queues-enrolled-file ()
  (let* ((directory (make-temp-file "gnosis-nodes-mode" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (path (expand-file-name "note.org" directory))
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [] :conflicts []))
         http-called buffer)
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (gnosis-nodes--write-document "note.org" "body")
          (gnosis-nodes--write-state
           (gnosis-nodes--enroll-file (gnosis-nodes--read-state) path))
          (gnosis-nodes--write-document "note.org" "updated")
          (setq buffer (find-file-noselect path))
          (cl-letf (((symbol-function 'gnosis-core-sync-state)
                     (lambda () (setq http-called t))))
            (with-current-buffer buffer
              (gnosis-nodes-mode 1)
              (run-hooks 'after-save-hook)))
          (should-not http-called)
          (let ((entry (aref (plist-get (gnosis-nodes--read-state) :outbox) 0)))
            (should (= 1 (length (plist-get (gnosis-nodes--read-state) :outbox))))
            (should (equal "updated"
                           (base64-decode-string
                            (plist-get entry :payload))))))
      (when (buffer-live-p buffer) (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-fetches-all-pages-before-materializing ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "cut" :bases [] :outbox [] :conflicts []))
         (calls 0) applied)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-sync-snapshot)
                   (lambda (_next)
                     (cl-incf calls)
                     (if (= calls 1)
                         '(:version 1 :snapshot_id "snapshot" :cursor nil :next "page-2"
                           :items [])
                       '(:version 1 :snapshot_id "wrong" :cursor "cut" :next nil
                         :items []))))
                  ((symbol-function 'gnosis-nodes--apply-item)
                   (lambda (&rest _) (setq applied t))))
          (gnosis-nodes--write-state state)
          (should-error (gnosis-nodes--snapshot state) :type 'gnosis-nodes-error)
          (should-not applied))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-rejection-persists-server-evidence ()
  (let* ((directory (make-temp-file "gnosis-nodes-evidence" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (entry (gnosis-nodes--outbox-entry
                 :replace "one" 1 "one.org" "d" nil "one"))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [,entry] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (cl-letf (((symbol-function 'gnosis-core-document-replace)
                     (lambda (&rest _)
                       '(:status "rejected" :cursor "hint"
                         :rejection (:reason "revision_conflict"
                                     :expected_revision 1
                                     :current_revision 2)))))
            (gnosis-nodes--send-outbox state))
          (let ((stored (aref (plist-get (gnosis-nodes--read-state) :outbox) 0)))
            (should (eq :rejected (plist-get stored :status)))
            (should (equal 2 (plist-get (plist-get (plist-get stored :result)
                                                   :rejection)
                                        :current_revision)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-rejects-equal-revision-projection-collision ()
  (let* ((directory (make-temp-file "gnosis-nodes-revision" t))
         (gnosis-nodes-dir directory)
         (old '(:document_id "doc" :logical_path "old.org" :revision 3
                :content_sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
         (new '(:document_id "doc" :logical_path "new.org" :revision 3
                :content_sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed"
                  :bases [(:document-id "doc" :logical-path "old.org" :revision 3
                           :digest "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                           :fingerprint ,(gnosis-nodes--projection-fingerprint old))]
                  :outbox [] :conflicts [])))
    (unwind-protect
        (should-error (gnosis-nodes--apply-document state new)
                      :type 'gnosis-nodes-error)
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-spool-detects-tampering ()
  (let* ((directory (make-temp-file "gnosis-nodes-spool" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (file (expand-file-name "private/snapshot/page.json" directory)))
    (unwind-protect
        (progn
          (gnosis-nodes--write-bytes file "{}")
          (should-error
           (gnosis-nodes--read-snapshot-page
            (list :file file :sha256 (secure-hash 'sha256 "different")))
           :type 'gnosis-nodes-error))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-sync-all-files-requires-explicit-enrollment ()
  (let* ((directory (make-temp-file "gnosis-nodes-all" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (gnosis-nodes--write-document "a.org" "a")
          (gnosis-nodes--write-document "nested/b.org" "b")
          (gnosis-nodes--write-state
           (gnosis-nodes--enroll-all (gnosis-nodes--read-state)))
          (should (= 2 (length (plist-get (gnosis-nodes--read-state) :outbox)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-dependent-save-waits-for-canonical-feed-base ()
  (let* ((first (gnosis-nodes--outbox-entry
                 :create nil nil "note.org" "old" nil "old"))
         (state `(:bases [] :outbox [,first] :conflicts [])))
    (setf (plist-get first :status) :uncertain)
    (setq state
          (gnosis-nodes--queue-save
           state '(:document-id nil :logical-path "note.org") "new"))
    (let ((dependent (aref (plist-get state :outbox) 1)))
      (should (eq :blocked (plist-get dependent :status)))
      (should (equal (plist-get first :operation-id)
                     (plist-get dependent :depends-on))))))

(ert-deftest gnosis-nodes-conflict-remains-until-feed-confirms-accepted-bytes ()
  (let* ((directory (make-temp-file "gnosis-nodes-conflict" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "merged"))
         (digest (gnosis-nodes--digest bytes))
         (original (string-to-unibyte "before"))
         (merge-file (expand-file-name "merge.org" directory))
         (control-buffer (generate-new-buffer " *gnosis-ediff-control*"))
         (session-id "session")
         (record `(:document-id "doc" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "remote"
                   :status :editing
                   :merge-start-digest ,(gnosis-nodes--digest original)
                   :merge-file ,merge-file :merge-session-id ,session-id))
         (state `(:bases [] :outbox [] :conflicts [,record])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" original)
          (gnosis-nodes--write-bytes merge-file bytes)
          (puthash session-id
                   (list :document-id "doc" :merge-file merge-file
                         :control-buffer control-buffer)
                   gnosis-nodes--ediff-sessions)
          (cl-letf (((symbol-function 'gnosis-nodes--read-state)
                     (lambda () state))
                    ((symbol-function 'gnosis-nodes--conflict-directory)
                     (lambda (_) directory))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value) (setq state value)))
                    ((symbol-function 'gnosis-nodes--remote-conflict-current-p)
                     (lambda (&rest _) t)))
            (gnosis-nodes-accept-conflict))
          (should (= 1 (length (plist-get state :conflicts))))
          (should (equal digest
                         (plist-get (aref (plist-get state :conflicts) 0)
                                    :accepted-digest))))
      (remhash session-id gnosis-nodes--ediff-sessions)
      (when (buffer-live-p control-buffer) (kill-buffer control-buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-accept-conflict-restores-file-when-state-write-fails ()
  (let* ((directory (make-temp-file "gnosis-nodes-conflict-write" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (original (string-to-unibyte "before"))
         (merged (string-to-unibyte "merged"))
         (merge-file (expand-file-name "merge.org" directory))
         (control-buffer (generate-new-buffer " *gnosis-ediff-control*"))
         (session-id "write-failure")
         (record `(:document-id "doc" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "remote"
                   :status :editing
                   :merge-start-digest ,(gnosis-nodes--digest original)
                   :merge-file ,merge-file :merge-session-id ,session-id))
         (state `(:bases [] :outbox [] :conflicts [,record]))
         (writes 0))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" original)
          (gnosis-nodes--write-bytes merge-file merged)
          (puthash session-id
                   (list :document-id "doc" :merge-file merge-file
                         :control-buffer control-buffer)
                   gnosis-nodes--ediff-sessions)
          (cl-letf (((symbol-function 'gnosis-nodes--read-state)
                     (lambda () state))
                    ((symbol-function 'gnosis-nodes--conflict-directory)
                     (lambda (_) directory))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value)
                       (cl-incf writes)
                       (if (= writes 1)
                           (signal 'file-error '("state write failed"))
                         (setq state value))))
                    ((symbol-function 'gnosis-nodes--remote-conflict-current-p)
                     (lambda (&rest _) t)))
            (should-error (gnosis-nodes-accept-conflict) :type 'file-error))
          (should (equal original
                         (gnosis-nodes--managed-bytes
                          directory (expand-file-name "note.org" directory))))
          (should (= writes 2))
          (should (eq :unresolved
                      (plist-get (aref (plist-get state :conflicts) 0) :status))))
      (remhash session-id gnosis-nodes--ediff-sessions)
      (when (buffer-live-p control-buffer) (kill-buffer control-buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-sync-all-files-skips-symlink-escape ()
  (let* ((directory (make-temp-file "gnosis-nodes-all-safe" t))
         (outside (make-temp-file "gnosis-nodes-outside" nil ".org" "outside"))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (make-symbolic-link outside (expand-file-name "linked.org" directory))
          (gnosis-nodes--write-state
           (gnosis-nodes--enroll-all (gnosis-nodes--read-state)))
          (should (= 0 (length (plist-get (gnosis-nodes--read-state) :outbox)))))
      (delete-directory directory t)
      (delete-file outside))))

(ert-deftest gnosis-nodes-idle-sync-refuses-reentry ()
  (let ((gnosis-nodes--busy t)
        called)
    (cl-letf (((symbol-function 'gnosis-nodes-sync)
               (lambda () (setq called t))))
      (gnosis-nodes--idle-sync)
      (should-not called))))

(ert-deftest gnosis-nodes-rejects-stale-delta-and-snapshot-projections ()
  (dolist (operation '("update" "snapshot"))
    (let* ((directory (make-temp-file "gnosis-nodes-stale" t))
           (gnosis-nodes-dir directory)
           (bytes (string-to-unibyte "current"))
           (digest (gnosis-nodes--digest bytes))
           (remote-digest (gnosis-nodes--digest "older"))
           (state `(:version 1 :cursor "feed"
                    :bases [(:document-id "doc" :revision 5
                             :logical-path "note.org" :digest ,digest
                             :active t)]
                    :outbox [] :conflicts [(:document-id "other")]))
           (before (copy-tree state t))
           (projection `(:document_id "doc" :revision 4
                         :logical_path "note.org"
                         :content_sha256 ,remote-digest
                         :operation ,operation))
           (blob-calls 0))
      (unwind-protect
          (progn
            (gnosis-nodes--write-document "note.org" bytes)
            (cl-letf (((symbol-function 'gnosis-core-blob)
                       (lambda (_)
                         (cl-incf blob-calls)
                         "older")))
              (should-error (gnosis-nodes--apply-document state projection)
                            :type 'gnosis-nodes-error))
            (should (= 0 blob-calls))
            (should (equal state before))
            (should (equal bytes
                           (gnosis-nodes--read-bytes
                            (expand-file-name "note.org" directory)))))
        (delete-directory directory t)))))

(ert-deftest gnosis-nodes-rejects-duplicate-logical-path-identity ()
  (let* ((directory (make-temp-file "gnosis-nodes-identity" t))
         (gnosis-nodes-dir directory)
         (bytes (string-to-unibyte "one"))
         (digest (gnosis-nodes--digest bytes))
         (state `(:bases [(:document-id "doc-1" :revision 1
                           :logical-path "same.org" :digest ,digest :active t)]
                  :outbox [] :conflicts []))
         (projection `(:document_id "doc-2" :revision 2
                       :logical_path "same.org"
                       :content_sha256 ,(gnosis-nodes--digest "two")
                       :operation "update"))
         (blob-called nil))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "same.org" bytes)
          (cl-letf (((symbol-function 'gnosis-core-blob)
                     (lambda (_) (setq blob-called t) "two")))
            (should-error (gnosis-nodes--apply-document state projection)
                          :type 'gnosis-nodes-error))
          (should-not blob-called)
          (should (equal bytes
                         (gnosis-nodes--read-bytes
                          (expand-file-name "same.org" directory)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-applies-clean-authoritative-rename ()
  (let* ((directory (make-temp-file "gnosis-nodes-rename" t))
         (gnosis-nodes-dir directory)
         (bytes (string-to-unibyte "body"))
         (digest (gnosis-nodes--digest bytes))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "old.org" :digest ,digest :active t)]
                  :outbox [] :conflicts []))
         (projection `(:document_id "doc" :revision 2
                       :logical_path "new.org" :content_sha256 ,digest
                       :operation "rename")))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "old.org" bytes)
          (cl-letf (((symbol-function 'gnosis-core-blob) (lambda (_) bytes)))
            (setq state (gnosis-nodes--apply-document state projection)))
          (should-not (file-exists-p (expand-file-name "old.org" directory)))
          (should (equal bytes
                         (gnosis-nodes--read-bytes
                          (expand-file-name "new.org" directory))))
          (should (equal "new.org"
                         (plist-get (gnosis-nodes--base state "doc")
                                    :logical-path)))
          (should (= 0 (length (plist-get state :conflicts)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-conflict-status-round-trips-isolated ()
  (let* ((directory (make-temp-file "gnosis-nodes-conflict-state" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox []
                  :conflicts [(:document-id "doc" :logical-path "note.org"
                               :kind :rename :status :queued
                               :operation-id "op")])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (let ((record (aref (plist-get (gnosis-nodes--read-state)
                                         :conflicts)
                              0)))
            (should (eq :queued (plist-get record :status)))
            (should (eq :rename (plist-get record :kind)))
            (should (equal "op" (plist-get record :operation-id)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-pending-local-intent-blocks-remote-materialization ()
  (let* ((directory (make-temp-file "gnosis-nodes-pending" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (bytes (string-to-unibyte "local"))
         (digest (gnosis-nodes--digest bytes))
         (entry (gnosis-nodes--outbox-entry
                 :rename "doc" 1 "moved.org" digest "note.org"))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "note.org" :digest ,digest :active t)]
                  :outbox [,entry] :conflicts []))
         (remote (string-to-unibyte "remote"))
         (projection `(:document_id "doc" :revision 2 :logical_path "note.org"
                       :content_sha256 ,(gnosis-nodes--digest remote)
                       :operation "update")))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" bytes)
          (cl-letf (((symbol-function 'gnosis-core-blob) (lambda (_) remote)))
            (setq state (gnosis-nodes--apply-document state projection)))
          (should (equal bytes (gnosis-nodes--read-bytes
                                (expand-file-name "note.org" directory))))
          (should (= 1 (length (plist-get state :conflicts)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-conflict-acceptance-waits-behind-uncertain-head ()
  (let* ((head (gnosis-nodes--outbox-entry
                :replace "doc" 1 "note.org" "old" nil
                (string-to-unibyte "old")))
         (entry (gnosis-nodes--outbox-entry
                 :replace "doc" 2 "note.org" "merged" nil
                 (string-to-unibyte "merged")))
         (state `(:outbox [,head])))
    (setf (plist-get head :status) :uncertain)
    (setq state (gnosis-nodes--enqueue-after-head state entry "doc" "note.org"))
    (should (= 2 (length (plist-get state :outbox))))
    (let ((blocked (aref (plist-get state :outbox) 1)))
      (should (eq :blocked (plist-get blocked :status)))
      (should (equal (plist-get head :operation-id)
                     (plist-get blocked :depends-on))))))

(ert-deftest gnosis-nodes-confirmed-applied-receipt-is-pruned ()
  (let* ((projection '(:document_id "doc" :revision 2 :logical_path "note.org"
                       :content_sha256 "digest" :operation "update"))
         (entry `(:operation-id "op" :status :applied :payload "large"
                  :result (:status "applied" :authoritative ,projection)))
         (other '(:operation-id "other" :status :uncertain :document-id "other"))
         (state `(:bases [] :outbox [,entry ,other] :conflicts [])))
    (setq state (gnosis-nodes--record-base state projection))
    (should (= 1 (length (plist-get state :outbox))))
    (should (equal "other"
                   (plist-get (aref (plist-get state :outbox) 0)
                              :operation-id)))))

(ert-deftest gnosis-nodes-applied-receipt-promotes-dependent-before-pruning ()
  (let* ((projection '(:document_id "doc" :revision 1
                       :logical_path "note.org" :content_sha256 "digest"
                       :operation "create"))
         (applied `(:operation-id "create-op" :status :applied
                    :result (:status "applied" :authoritative ,projection)))
         (dependent '(:operation-id "replace-op" :kind :replace
                      :logical-path "note.org" :status :blocked
                      :depends-on "create-op"))
         (state `(:bases [] :outbox [,applied ,dependent] :conflicts [])))
    (setq state (gnosis-nodes--record-base state projection))
    (should (= 1 (length (plist-get state :outbox))))
    (let ((promoted (aref (plist-get state :outbox) 0)))
      (should (eq :pending (plist-get promoted :status)))
      (should (equal "doc" (plist-get promoted :document-id)))
      (should (= 1 (plist-get promoted :revision)))
      (should-not (plist-get promoted :depends-on)))))

(ert-deftest gnosis-nodes-explicit-acceptance-supersedes-rejected-head ()
  (let* ((head (gnosis-nodes--outbox-entry
                :replace "doc" 1 "note.org" "old" nil
                (string-to-unibyte "old")))
         (entry (gnosis-nodes--outbox-entry
                 :replace "doc" 2 "note.org" "merged" nil
                 (string-to-unibyte "merged")))
         (state `(:outbox [,head])))
    (setf (plist-get head :status) :rejected)
    (setq state (gnosis-nodes--enqueue-after-head state entry "doc" "note.org"))
    (should (eq :superseded
                (plist-get (aref (plist-get state :outbox) 0) :status)))
    (should (eq :pending
                (plist-get (aref (plist-get state :outbox) 1) :status)))))

(ert-deftest gnosis-nodes-rejects-empty-change-cursor-advance ()
  (let ((from (gnosis-test-nodes--cursor "server" 1))
        (next (gnosis-test-nodes--cursor "server" 2)))
    (should-not
     (gnosis-nodes--change-page-valid-p
      `(:version 1 :from ,from :next ,next :has_more nil :items [])
      from))
    (should
     (gnosis-nodes--change-page-valid-p
      `(:version 1 :from ,from :next ,from :has_more nil :items [])
      from))))

(ert-deftest gnosis-nodes-snapshot-preflights-all-file-effects ()
  (let* ((directory (make-temp-file "gnosis-nodes-preflight" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (old (string-to-unibyte "old"))
         (new (string-to-unibyte "new"))
         (old-digest (gnosis-nodes--digest old))
         (new-digest (gnosis-nodes--digest new))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "before"
                  :bases [(:document-id "b" :revision 1 :logical-path "b.org"
                           :digest ,old-digest :active t)]
                  :outbox [] :conflicts []))
         (page `(:version 1 :snapshot_id "snapshot"
                 :cursor ,(gnosis-test-nodes--cursor "server" 9) :next nil
                 :items [(:entity_kind "document"
                          :projection (:document_id "a" :revision 1
                                       :logical_path "a.org"
                                       :content_sha256 ,new-digest
                                       :blob_url ,(format "/v1/blobs/%s" new-digest)
                                       :operation "create"
                                       :projection (:content_sha256 ,new-digest)))
                         (:entity_kind "document"
                          :projection (:document_id "b" :revision 2
                                       :logical_path "b.org"
                                       :content_sha256 ,new-digest
                                       :blob_url ,(format "/v1/blobs/%s" new-digest)
                                       :operation "update"
                                       :projection (:content_sha256 ,new-digest)))]))
         buffer)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "b.org" old)
          (setq buffer (find-file-noselect (expand-file-name "b.org" directory)))
          (with-current-buffer buffer
            (goto-char (point-max))
            (insert "modified"))
          (cl-letf (((symbol-function 'gnosis-core-sync-snapshot) (lambda (_) page))
                    ((symbol-function 'gnosis-core-blob) (lambda (_) new)))
            (should-error (gnosis-nodes--snapshot state)
                          :type 'gnosis-nodes-conflict))
          (should-not (file-exists-p (expand-file-name "a.org" directory))))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer (set-buffer-modified-p nil))
        (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-pull-stops-at-configured-page-budget ()
  (let* ((zero (gnosis-test-nodes--cursor "server" 0))
         (one (gnosis-test-nodes--cursor "server" 1))
         (state `(:cursor ,zero :bases [] :outbox [] :conflicts []))
         (calls 0))
    (cl-letf (((symbol-value 'gnosis-nodes-max-pages-per-pass) 1)
              ((symbol-function 'gnosis-core-sync-changes)
               (lambda (cursor)
                 (cl-incf calls)
                 `(:version 1 :from ,cursor :next ,one :has_more t
                   :items [(:cursor ,one :entity_kind "settings"
                            :projection (:version 1))])))
              ((symbol-function 'gnosis-nodes--write-state) #'ignore))
      (setq state (gnosis-nodes--pull state))
      (should (= calls 1))
      (should (equal one (plist-get state :cursor)))
      (should-not gnosis-nodes--pull-complete))))

(ert-deftest gnosis-nodes-snapshot-stops-at-configured-page-budget ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot-budget" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:cursor "before" :bases [] :outbox [] :conflicts []))
         (two (gnosis-test-nodes--cursor "server" 2 "snapshot"))
         (calls 0))
    (unwind-protect
        (cl-letf (((symbol-value 'gnosis-nodes-max-pages-per-pass) 1)
                  ((symbol-function 'gnosis-core-sync-snapshot)
                   (lambda (_)
                     (cl-incf calls)
                     `(:version 1 :snapshot_id "snapshot" :cursor nil
                       :next ,two :items []))))
          (setq state (gnosis-nodes--snapshot state))
          (should (= calls 1))
          (should (equal "before" (plist-get state :cursor)))
          (should (equal two (plist-get (plist-get state :snapshot) :next)))
          (should (file-directory-p
                   (expand-file-name "snapshot"
                                     (gnosis-nodes--state-directory)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-conflict-record-keeps-latest-remote-head ()
  (let* ((directory (make-temp-file "gnosis-nodes-latest-conflict" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:conflicts []))
         (base '(:logical-path "note.org" :digest nil)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-nodes--write-bytes) #'ignore))
          (setq state
                (gnosis-nodes--record-conflict
                 state base
                 '(:document_id "doc" :revision 2 :logical_path "note.org"
                   :content_sha256 nil :operation "tombstone")
                 "local" nil))
          (setq state
                (gnosis-nodes--record-conflict
                 state base
                 '(:document_id "doc" :revision 3 :logical_path "note.org"
                   :content_sha256 nil :operation "tombstone")
                 "local" nil))
          (should (= 1 (length (plist-get state :conflicts))))
          (should (= 3 (plist-get (aref (plist-get state :conflicts) 0)
                                  :remote-revision))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-missing-local-conflict-queues-tombstone ()
  (let* ((record '(:document-id "doc" :kind :missing-local
                   :local-path "note.org" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "digest"))
         (state `(:bases [(:document-id "doc" :logical-path "note.org"
                           :revision 1 :digest "old" :active t)]
                  :outbox [] :conflicts [,record])))
    (setq state (gnosis-nodes--keep-local-conflict state record nil))
    (let ((entry (aref (plist-get state :outbox) 0))
          (resolved (aref (plist-get state :conflicts) 0)))
      (should (eq :delete (plist-get entry :kind)))
      (should (= 2 (plist-get entry :revision)))
      (should (eq :queued (plist-get resolved :status)))
      (should (equal (plist-get entry :operation-id)
                     (plist-get resolved :operation-id))))))

(ert-deftest gnosis-nodes-keep-local-conflict-recreates-or-renames-explicitly ()
  (let* ((bytes (string-to-unibyte "local"))
         (tombstone '(:document-id "old" :kind :tombstone
                      :local-path "note.org" :logical-path "note.org"
                      :remote-revision 2 :remote-digest nil))
         (state `(:bases [(:document-id "old" :logical-path "note.org"
                           :revision 1 :digest "old" :active t)]
                  :outbox [] :conflicts [,tombstone])))
    (setq state (gnosis-nodes--keep-local-conflict state tombstone bytes))
    (should (eq :create (plist-get (aref (plist-get state :outbox) 0) :kind)))
    (should (= 0 (length (plist-get state :bases)))))
  (let* ((bytes (string-to-unibyte "edited"))
         (rename '(:document-id "doc" :kind :rename
                   :local-path "old.org" :logical-path "new.org"
                   :remote-revision 2 :remote-digest "remote"))
         (state `(:bases [] :outbox [] :conflicts [,rename])))
    (setq state (gnosis-nodes--keep-local-conflict state rename bytes))
    (should (equal '(:rename :replace)
                   (mapcar (lambda (entry) (plist-get entry :kind))
                           (append (plist-get state :outbox) nil))))
    (should (equal (plist-get (aref (plist-get state :outbox) 0) :operation-id)
                   (plist-get (aref (plist-get state :outbox) 1) :depends-on)))
    (should (equal (plist-get (aref (plist-get state :outbox) 1) :operation-id)
                   (plist-get (aref (plist-get state :conflicts) 0)
                              :operation-id)))))

(ert-deftest gnosis-nodes-accept-remote-tombstone-backs-up-local-bytes ()
  (let* ((directory (make-temp-file "gnosis-nodes-accept-tombstone" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "local"))
         (projection '(:document_id "doc" :revision 2
                       :logical_path "note.org" :content_sha256 nil
                       :blob_url nil :operation "tombstone" :projection nil))
         (record `(:document-id "doc" :kind :tombstone
                   :local-path "note.org" :logical-path "note.org"
                   :local-digest ,(gnosis-nodes--digest bytes)
                   :remote-digest nil :remote-revision 2
                   :projection ,projection))
         (head (gnosis-nodes--outbox-entry
                :delete "doc" 2 "note.org" nil nil))
         (state `(:bases [(:document-id "doc" :logical-path "note.org"
                           :revision 1 :digest "old" :active t)]
                  :outbox [,head] :conflicts [,record])))
    (setf (plist-get head :status) :rejected)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" bytes)
          (setq state (gnosis-nodes--accept-remote-conflict state record))
          (should-not (file-exists-p (expand-file-name "note.org" directory)))
          (should (= 0 (length (plist-get state :conflicts))))
          (should (eq :superseded
                      (plist-get (aref (plist-get state :outbox) 0) :status)))
          (should (equal bytes
                         (gnosis-nodes--read-bytes
                          (expand-file-name
                           "local-backup.org"
                           (gnosis-nodes--conflict-directory "doc"))))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-outbox-stops-at-configured-mutation-budget ()
  (let* ((skipped (gnosis-nodes--outbox-entry
                   :create nil nil "skipped.org"
                   (secure-hash 'sha256 "skipped") nil "skipped"))
         (first (gnosis-nodes--outbox-entry
                 :create nil nil "one.org" (secure-hash 'sha256 "one") nil "one"))
         (second (gnosis-nodes--outbox-entry
                  :create nil nil "two.org" (secure-hash 'sha256 "two") nil "two"))
         (state (list :outbox (vector skipped first second)))
         (calls 0))
    (setf (plist-get skipped :status) :superseded)
    (cl-letf (((symbol-value 'gnosis-nodes-max-mutations-per-pass) 1)
              ((symbol-function 'gnosis-nodes--write-state) #'ignore)
              ((symbol-function 'gnosis-nodes--send-entry)
               (lambda (entry)
                 (cl-incf calls)
                 (list :format_version 1 :status "applied"
                       :operation_id (plist-get entry :operation-id)
                       :cursor "opaque"
                       :authoritative
                       '(:document_id "doc" :revision 1 :logical_path "one.org"
                         :content_sha256 nil :active t)))))
      (setq state (gnosis-nodes--send-outbox state))
      (should (= calls 1))
      (should (eq :superseded
                  (plist-get (aref (plist-get state :outbox) 0) :status)))
      (should (eq :applied
                  (plist-get (aref (plist-get state :outbox) 1) :status)))
      (should (eq :pending
                  (plist-get (aref (plist-get state :outbox) 2) :status))))))

(ert-deftest gnosis-nodes-outbox-scan-eventually-reaches-pending-tail ()
  (let* ((terminal
          (lambda (index)
            (list :operation-id (format "old-%d" index) :status :superseded)))
         (pending (gnosis-nodes--outbox-entry
                   :create nil nil "pending.org"
                   (secure-hash 'sha256 "pending") nil "pending"))
         (state (list :outbox
                      (vconcat (mapcar terminal (number-sequence 0 255))
                               (vector pending))))
         (calls 0))
    (cl-letf (((symbol-function 'gnosis-nodes--write-state) #'ignore)
              ((symbol-function 'gnosis-nodes--send-entry)
               (lambda (entry)
                 (cl-incf calls)
                 (list :format_version 1 :status "applied"
                       :operation_id (plist-get entry :operation-id)
                       :cursor "opaque"
                       :authoritative
                       '(:document_id "doc" :revision 1
                         :logical_path "pending.org"
                         :content_sha256 nil :active t)))))
      (setq state (gnosis-nodes--send-outbox state))
      (should (= calls 0))
      (setq state (gnosis-nodes--send-outbox state))
      (should (= calls 1))
      (should (eq :applied
                  (plist-get (aref (plist-get state :outbox) 256) :status))))))

(ert-deftest gnosis-nodes-accept-remote-supersedes-whole-local-chain ()
  (let* ((first '(:operation-id "first" :status :pending :kind :replace
                 :document-id "doc" :logical-path "note.org"))
         (second '(:operation-id "second" :status :blocked :kind :rename
                  :depends-on "first" :document-id "doc"
                  :logical-path "other.org"))
         (remote (string-to-unibyte "remote"))
         (digest (gnosis-nodes--digest remote))
         (projection `(:document_id "doc" :revision 2
                       :logical_path "note.org" :content_sha256 ,digest
                       :operation "update"))
         (record `(:document-id "doc" :kind :content
                   :local-path "note.org" :logical-path "note.org"
                   :local-digest ,(gnosis-nodes--digest "local")
                   :remote-digest ,digest :remote-revision 2
                   :remote-file "remote-file" :projection ,projection))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "note.org" :digest "old" :active t)]
                  :outbox [,first ,second] :conflicts [,record])))
    (cl-letf (((symbol-function 'gnosis-nodes--safe-path)
               (lambda (&rest _) "/dev/null"))
              ((symbol-function 'gnosis-nodes--working-bytes)
               (lambda (_) (string-to-unibyte "local")))
              ((symbol-function 'gnosis-nodes--managed-bytes)
               (lambda (&rest _) remote))
              ((symbol-function 'gnosis-nodes--write-state) #'ignore)
              ((symbol-function 'gnosis-nodes--write-document) #'ignore))
      (setq state (gnosis-nodes--accept-remote-conflict state record))
      (should (equal '(:superseded :superseded)
                     (mapcar (lambda (entry) (plist-get entry :status))
                             (append (plist-get state :outbox) nil))))
      (should (= 0 (length (plist-get state :conflicts)))))))

(ert-deftest gnosis-nodes-accept-merge-refuses-modified-visiting-buffer ()
  (let* ((directory (make-temp-file "gnosis-accept-modified" t))
         (gnosis-nodes-dir directory)
         (path (expand-file-name "note.org" directory))
         (record '(:document-id "doc" :kind :content
                   :local-path "note.org" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "remote"
                   :status :editing :merge-start-digest "before"))
         (state `(:bases [] :outbox [] :conflicts [,record]))
         buffer written)
    (unwind-protect
        (progn
          (with-temp-file path (insert "saved"))
          (setq buffer (find-file-noselect path))
          (with-current-buffer buffer (goto-char (point-max)) (insert "-merge"))
          (cl-letf (((symbol-function 'gnosis-nodes--state-directory)
                     (lambda () directory))
                    ((symbol-function 'gnosis-nodes--read-state) (lambda () state))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value) (setq written value)))
                    ((symbol-function 'gnosis-nodes--remote-conflict-current-p)
                     (lambda (&rest _) t))
                    ((symbol-function 'lock-file) #'ignore)
                    ((symbol-function 'unlock-file) #'ignore))
            (should-error (gnosis-nodes-accept-conflict) :type 'user-error)
            (should-not written)
            (should (= 0 (length (plist-get state :outbox))))))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer (set-buffer-modified-p nil))
        (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-sync-shares-one-page-budget ()
  (let* ((cursor (gnosis-test-nodes--cursor "server" 1))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor ,cursor :protocol-fingerprint "fingerprint"
                  :bases [] :outbox [] :conflicts []))
        (calls 0)
        (directory (make-temp-file "gnosis-sync-budget" t)))
    (unwind-protect
        (cl-letf (((symbol-value 'gnosis-nodes-max-pages-per-pass) 1)
                  ((symbol-function 'gnosis-nodes--state-directory)
                   (lambda () directory))
                  ((symbol-function 'gnosis-nodes--read-state) (lambda () state))
                  ((symbol-function 'gnosis-nodes--write-state)
                   (lambda (value) (setq state value)))
                  ((symbol-function 'gnosis-nodes--assert-identity) #'ignore)
                  ((symbol-function 'gnosis-core-sync-changes)
                   (lambda (cursor)
                     (cl-incf calls)
                     (list :version 1 :from cursor :next cursor
                           :has_more gnosis-core--json-false :items []))))
          (gnosis-nodes-sync)
          (should (= calls 1)))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-conflict-head-refuses-stale-projection ()
  (let* ((remote (list :document_id "doc" :revision 3
                       :logical_path "note.org" :content_sha256 nil
                       :active gnosis-core--json-false
                       :blob_url nil :operation "tombstone" :projection nil))
         (conflict (list :document-id "doc" :remote-revision 3
                         :logical-path "note.org" :remote-digest nil
                         :projection remote))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "note.org" :digest "base" :active t)]
                  :outbox [] :conflicts [,conflict]))
         (stale (copy-tree remote t))
         (collision (copy-tree remote t)))
    (setf (plist-get stale :revision) 2
          (plist-get collision :logical_path) "other.org")
    (should-error (gnosis-nodes--apply-document state stale)
                  :type 'gnosis-nodes-error)
    (should-error (gnosis-nodes--apply-document state collision)
                  :type 'gnosis-nodes-error)
    (should (= 3 (plist-get (aref (plist-get state :conflicts) 0)
                            :remote-revision)))))

(ert-deftest gnosis-nodes-keep-local-refuses-stale-remote-head ()
  (let* ((record '(:document-id "doc" :kind :missing-local
                   :remote-revision 2 :logical-path "note.org"
                   :remote-digest "digest" :local-digest nil))
         (state `(:bases [] :outbox [] :conflicts [,record]))
         (directory (make-temp-file "gnosis-keep-local-stale" t))
         written)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-nodes--state-directory)
                   (lambda () directory))
                  ((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                  ((symbol-function 'gnosis-nodes--read-state) (lambda () state))
                  ((symbol-function 'gnosis-nodes--write-state)
                   (lambda (value) (setq written value)))
                  ((symbol-function 'gnosis-nodes--remote-conflict-current-p)
                   (lambda (&rest _) nil)))
          (should-error (gnosis-nodes-keep-local-conflict)
                        :type 'user-error)
          (should-not written)
          (should (= 0 (length (plist-get state :outbox)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-reset-resumes-staging-before-publication ()
  (let* ((root (make-temp-file "gnosis-nodes-reset-resume" t))
         (gnosis-nodes-dir (file-name-as-directory
                                  (expand-file-name "notes" root)))
         (user-emacs-directory (file-name-as-directory
                                (expand-file-name "emacs" root)))
         (gnosis-nodes-state-file nil)
         (fingerprint (gnosis-nodes--snapshot-fingerprint
                       (list '(:items []))))
         (trusted `(:version 1 :server-id "server" :device-id "device"
                    :cursor "old" :protocol-fingerprint ,fingerprint
                    :bases [] :outbox [] :conflicts []))
         (calls 0))
    (unwind-protect
        (progn
          (make-directory gnosis-nodes-dir t)
          (gnosis-nodes--write-state trusted)
          (cl-letf (((symbol-value 'gnosis-nodes-max-pages-per-pass) 1)
                    ((symbol-function 'yes-or-no-p) (lambda (&rest _) t))
                    ((symbol-function 'gnosis-core-sync-identity)
                     (lambda () '(:device_id "device")))
                    ((symbol-function 'gnosis-core-sync-state)
                     (lambda () '(:server_id "server" :cursor "cut")))
                    ((symbol-function 'gnosis-core-sync-snapshot)
                     (lambda (continuation)
                       (cl-incf calls)
                       (if continuation
                           `(:version 1 :snapshot_id "snapshot"
                             :cursor ,(gnosis-test-nodes--cursor "server" 9)
                             :next nil :items [])
                         `(:version 1 :snapshot_id "snapshot" :cursor nil
                           :next ,(gnosis-test-nodes--cursor
                                   "server" 2 "snapshot")
                           :items [])))))
            (gnosis-nodes-reset)
            (should (equal fingerprint
                           (plist-get (gnosis-nodes--read-state)
                                      :protocol-fingerprint)))
            (should (file-exists-p
                     (concat (gnosis-nodes--state-file) ".reset")))
            (gnosis-nodes-reset)
            (should (= calls 2))
            (should (equal fingerprint
                           (plist-get (gnosis-nodes--read-state)
                                      :protocol-fingerprint)))
            (should-not (plist-get (gnosis-nodes--read-state) :snapshot))
            (should-not (file-exists-p
                         (concat (gnosis-nodes--state-file) ".reset")))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-rejected-intent-conflicts-with-new-remote-head ()
  (let* ((root (make-temp-file "gnosis-nodes-rejected-" t))
         (gnosis-nodes-dir root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (local (string-to-unibyte "local"))
         (base-digest (gnosis-nodes--digest local))
         (remote (string-to-unibyte "remote"))
         (remote-digest (gnosis-nodes--digest remote))
         (entry (gnosis-nodes--outbox-entry
                 :delete "doc" 1 "note.org" base-digest))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "note.org" :digest ,base-digest
                           :active t)]
                  :outbox [,entry] :conflicts []))
         (projection `(:document_id "doc" :revision 2
                       :logical_path "note.org"
                       :content_sha256 ,remote-digest
                       :operation "update")))
    (setf (plist-get entry :status) :rejected)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" local)
          (cl-letf (((symbol-function 'gnosis-core-blob) (lambda (_) remote)))
            (setq state (gnosis-nodes--apply-document state projection)))
          (should (equal local
                         (gnosis-nodes--read-bytes
                          (expand-file-name "note.org" root))))
          (should (= 1 (length (plist-get state :conflicts)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-keep-local-supersedes-rejected-chain ()
  (let* ((head (gnosis-nodes--outbox-entry
                :replace "doc" 1 "note.org" "old" nil "old"))
         (tail (gnosis-nodes--outbox-entry
                :replace "doc" nil "note.org" "new" nil "new"))
         (record '(:document-id "doc" :kind :content
                   :local-path "note.org" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "remote"))
         (state `(:bases [] :outbox [,head ,tail] :conflicts [,record])))
    (setf (plist-get head :status) :rejected
          (plist-get tail :status) :blocked
          (plist-get tail :depends-on) (plist-get head :operation-id))
    (setq state (gnosis-nodes--keep-local-conflict
                 state record (string-to-unibyte "local")))
    (should (equal '(:superseded :superseded :pending)
                   (mapcar (lambda (entry) (plist-get entry :status))
                           (append (plist-get state :outbox) nil))))))

(ert-deftest gnosis-nodes-local-scan-is-bounded-and-fair ()
  (let* ((root (make-temp-file "gnosis-nodes-scan-budget-" t))
         (gnosis-nodes-dir root)
         (bases (vconcat
                 (mapcar
                  (lambda (index)
                    (list :document-id (format "doc-%d" index) :revision 1
                          :logical-path (format "%d.org" index)
                          :digest (gnosis-nodes--digest "old") :active t))
                  (number-sequence 0 4))))
         (state `(:bases ,bases :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (dotimes (index 5)
            (gnosis-nodes--write-document (format "%d.org" index) "old"))
          (gnosis-nodes--write-document "4.org" "changed")
          (cl-letf (((symbol-value 'gnosis-nodes-max-local-scan-per-pass) 2))
            (dotimes (_ 2)
              (setq state (gnosis-nodes--scan state)))
            (should (= 0 (length (plist-get state :outbox))))
            (setq state (gnosis-nodes--scan state))
            (should (= 1 (length (plist-get state :outbox))))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-ediff-output-is-disposable ()
  (let* ((root (make-temp-file "gnosis-nodes-ediff-" t))
         (gnosis-nodes-dir root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (local (expand-file-name "note.org" root))
         (conflict-directory (gnosis-nodes--conflict-directory "doc"))
         (base (expand-file-name "base.org" conflict-directory))
         (remote (expand-file-name "remote.org" conflict-directory))
         (record `(:document-id "doc" :local-path "note.org"
                   :logical-path "note.org" :base-file ,base
                   :remote-file ,remote))
         (state `(:bases [] :outbox [] :conflicts [,record]))
         output)
    (unwind-protect
        (progn
          (make-directory conflict-directory t)
          (dolist (file (list local base remote))
            (gnosis-nodes--write-bytes file "bytes"))
          (require 'ediff)
          (cl-letf (((symbol-function 'gnosis-nodes--read-state) (lambda () state))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value) (setq state value)))
                    ((symbol-function 'ediff-merge-buffers-with-ancestor)
                     (lambda (&rest args) (setq output (nth 5 args)))))
            (gnosis-nodes-resolve-conflict))
          (should-not (equal output local))
          (should (string-prefix-p (gnosis-nodes--state-directory) output))
          (should (equal output
                         (plist-get (aref (plist-get state :conflicts) 0)
                                    :merge-file)))
          (should (string-match-p
                   "merge buffer"
                   (documentation 'gnosis-nodes-resolve-conflict)))
          (should (string-match-p
                   "merge buffer"
                   (documentation 'gnosis-nodes-accept-conflict))))
      (mapc (lambda (buffer)
              (when (buffer-live-p buffer) (kill-buffer buffer)))
            (mapcar #'get-file-buffer (list local base remote)))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-public-sync-surface-matches-plan ()
  (should (boundp 'gnosis-nodes-dir))
  (should (equal (expand-file-name "~/Notes/") gnosis-nodes-dir))
  (should (fboundp 'gnosis-nodes-auto-sync-mode))
  (should-not (boundp 'gnosis-nodes-directory))
  (should-not (fboundp 'global-gnosis-nodes-mode))
  (should (commandp 'gnosis-nodes-track-file))
  (should (commandp 'gnosis-nodes-list-conflicts))
  (should-not (fboundp 'gnosis-nodes-track-all-files)))

(ert-deftest gnosis-nodes-save-after-uncertain-create-queues-dependent-replace ()
  (let* ((root (make-temp-file "gnosis-nodes-create-save-" t))
         (gnosis-nodes-dir root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (old (string-to-unibyte "old"))
         (new (string-to-unibyte "new"))
         (create (gnosis-nodes--outbox-entry
                  :create nil nil "note.org" (gnosis-nodes--digest old) nil old))
         (state (list :version 1 :server-id "server" :device-id "device"
                      :cursor "feed" :bases [] :outbox (vector create)
                      :conflicts []))
         buffer)
    (setf (plist-get create :status) :uncertain)
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (gnosis-nodes--write-document "note.org" new)
          (setq buffer (find-file-noselect (expand-file-name "note.org" root)))
          (with-current-buffer buffer
            (setq-local gnosis-nodes-mode t)
            (gnosis-nodes--queue-current-buffer))
          (let* ((outbox (plist-get (gnosis-nodes--read-state) :outbox))
                 (dependent (aref outbox 1)))
            (should (= 2 (length outbox)))
            (should (eq :uncertain (plist-get (aref outbox 0) :status)))
            (should (eq :blocked (plist-get dependent :status)))
            (should (equal (plist-get create :operation-id)
                           (plist-get dependent :depends-on)))
            (should (equal new
                           (base64-decode-string
                            (plist-get dependent :payload))))))
      (when (buffer-live-p buffer) (kill-buffer buffer))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-completed-snapshot-removes-private-spool ()
  (let* ((root (make-temp-file "gnosis-nodes-snapshot-cleanup-" t))
         (gnosis-nodes-dir root)
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-state-file nil)
         (state '(:cursor "before" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-sync-snapshot)
                   (lambda (_)
                     `(:version 1 :snapshot_id "snapshot"
                       :cursor ,(gnosis-test-nodes--cursor "server" 9)
                       :next nil :items []))))
          (setq state (gnosis-nodes--snapshot state))
          (should-not (plist-get state :snapshot))
          (should-not (file-exists-p
                       (expand-file-name "snapshot"
                                         (gnosis-nodes--state-directory)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-applied-receipt-does-not-block-conflict-resolution ()
  (let* ((local (string-to-unibyte "local"))
         (remote (string-to-unibyte "remote"))
         (projection
          (list :document_id "doc" :revision 2 :logical_path "note.org"
                :content_sha256 (gnosis-nodes--digest remote) :active t))
         (applied
          (list :operation-id "applied" :kind :replace :document-id "doc"
                :revision 1 :logical-path "note.org" :status :applied
                :result (list :authoritative projection)))
         (record
          (list :kind :content :document-id "doc" :local-path "note.org"
                :logical-path "note.org" :remote-revision 2
                :remote-digest (gnosis-nodes--digest remote)
                :projection projection))
         (state (list :outbox (vector applied) :conflicts (vector record))))
    (setq state (gnosis-nodes--keep-local-conflict state record local))
    (should (eq :superseded
                (plist-get (aref (plist-get state :outbox) 0) :status)))
    (should (eq :pending
                (plist-get (aref (plist-get state :outbox) 1) :status)))))

(ert-deftest gnosis-nodes-rename-replay-recognizes-materialized-destination ()
  (let* ((root (make-temp-file "gnosis-nodes-rename-replay-" t))
         (gnosis-nodes-dir root)
         (bytes (string-to-unibyte "body"))
         (digest (gnosis-nodes--digest bytes))
         (base (list :document-id "doc" :revision 1 :logical-path "old.org"
                     :digest digest :fingerprint "old" :active t))
         (projection
          (list :document_id "doc" :revision 2 :logical_path "new.org"
                :operation "rename" :content_sha256 digest
                :blob_url "/blobs/body" :active t))
         (receipt
          (list :operation-id "rename-op" :status :applied
                :result (list :authoritative projection)))
         (state (list :version 1 :server-id "server" :device-id "device"
                      :cursor nil :bases (vector base)
                      :outbox (vector receipt) :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "new.org" bytes)
          (setq state
                (gnosis-nodes--apply-document
                 state projection (list (cons digest bytes))))
          (should (= 0 (length (plist-get state :conflicts))))
          (let ((updated (gnosis-nodes--base state "doc" "new.org")))
            (should updated)
            (should (= 2 (plist-get updated :revision)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-track-file-enrolls-only-selected-file-offline ()
  (let* ((root (make-temp-file "gnosis-nodes-track-command-" t))
         (user-emacs-directory (file-name-as-directory root))
         (gnosis-nodes-dir (expand-file-name "notes" root))
         (gnosis-nodes-state-file nil)
         (first (expand-file-name "first.org" gnosis-nodes-dir))
         (second (expand-file-name "second.org" gnosis-nodes-dir)))
    (unwind-protect
        (progn
          (make-directory gnosis-nodes-dir t)
          (gnosis-nodes--write-bytes first "first")
          (gnosis-nodes--write-bytes second "second")
          (gnosis-nodes--write-state
           '(:version 1 :server-id "server" :device-id "device"
             :cursor "cursor" :bases [] :outbox [] :conflicts []))
          (gnosis-nodes-track-file first)
          (let ((outbox (plist-get (gnosis-nodes--read-state) :outbox)))
            (should (= 1 (length outbox)))
            (should (equal "first.org" (plist-get (aref outbox 0) :logical-path)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-conflict-selection-can-choose-second-record ()
  (let* ((first '(:document-id "one" :logical-path "one.org" :kind :edit))
         (second '(:document-id "two" :logical-path "two.org" :kind :edit))
         (conflicts (vector first second)))
    (cl-letf (((symbol-function 'completing-read)
               (lambda (&rest _)
                 (gnosis-nodes--conflict-label 1 second))))
      (let ((selection (gnosis-nodes--select-conflict conflicts)))
        (should (= 1 (car selection)))
        (should (eq second (cdr selection)))))))

(ert-deftest gnosis-nodes-corrected-save-supersedes-rejection ()
  (let* ((rejected (gnosis-nodes--outbox-entry
                    :replace "doc" 1 "note.org" "bad" nil "bad"))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "note.org" :digest "old" :active t)]
                  :outbox [,rejected] :conflicts [])))
    (setf (plist-get rejected :status) :rejected
          (plist-get rejected :result)
          '(:status "rejected" :rejection (:reason "invalid_content")))
    (setq state
          (gnosis-nodes--queue-save
           state (aref (plist-get state :bases) 0) "corrected"))
    (let ((replacement (aref (plist-get state :outbox) 1)))
      (should (eq :superseded (plist-get rejected :status)))
      (should (eq :pending (plist-get replacement :status)))
      (should-not (equal (plist-get rejected :operation-id)
                         (plist-get replacement :operation-id)))
      (should (equal "corrected"
                     (base64-decode-string
                      (plist-get replacement :payload)))))))

(ert-deftest gnosis-nodes-rust-projection-confirms-applied-receipt ()
  (let* ((projection '(:document_id "doc" :revision 2
                       :logical_path "note.org"
                       :content_sha256 "digest" :operation "update"))
         (record `(:document-id "doc" :logical-path "note.org"
                   :projection ,projection))
         (entry '(:status :applied
                  :result (:authoritative
                           (:document_id "doc" :revision 2
                            :logical_path "note.org"
                            :content_sha256 "digest" :active t)))))
    (should (gnosis-nodes--entry-confirms-conflict-p entry record))))

(ert-deftest gnosis-nodes-reconciliation-rejects-non-regular-destination ()
  (let* ((directory (make-temp-file "gnosis-nodes-nonregular" t))
         (gnosis-nodes-dir directory)
         (destination (expand-file-name "note.org" directory))
         (projection '(:document_id "doc" :revision 1
                       :logical_path "note.org" :content_sha256 nil
                       :operation "tombstone")))
    (unwind-protect
        (progn
          (make-directory destination)
          (should-error
           (gnosis-nodes--apply-document
            '(:bases [] :outbox [] :conflicts []) projection)
           :type 'gnosis-nodes-path-error)
          (delete-directory destination)
          (should (= 0 (call-process "mkfifo" nil nil nil destination)))
          (should-error
           (gnosis-nodes--apply-document
            '(:bases [] :outbox [] :conflicts []) projection)
           :type 'gnosis-nodes-path-error))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-finalization-is-action-bounded ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot-budget" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (gnosis-nodes-max-snapshot-actions-per-pass 1)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "before" :bases [] :outbox [] :conflicts []))
         (cut (gnosis-test-nodes--cursor "server" 9))
         (page `(:version 1 :snapshot_id "snapshot"
                 :cursor ,cut :next nil
                 :items [(:entity_kind "settings" :projection (:version 1))
                         (:entity_kind "settings" :projection (:version 1))]))
         (original (symbol-function 'gnosis-nodes--apply-item))
         (calls 0))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-sync-snapshot) (lambda (_) page))
                  ((symbol-function 'gnosis-nodes--write-state) #'ignore)
                  ((symbol-function 'gnosis-nodes--apply-item)
                   (lambda (&rest arguments)
                     (cl-incf calls)
                     (apply original arguments))))
          (setq state (gnosis-nodes--snapshot state))
          (dotimes (_ 24)
            (when (plist-get state :snapshot)
              (let ((before calls))
                (setq state (gnosis-nodes--snapshot state))
                (should (<= (- calls before) 1)))))
          (should-not (plist-get state :snapshot))
          (should (equal cut (plist-get state :cursor))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-finalization-resumes-from-durable-state ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot-resume" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (gnosis-core-base-url "http://127.0.0.1:8000")
         (gnosis-nodes-max-snapshot-actions-per-pass 1)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "before" :bases [] :outbox [] :conflicts []))
         (cut (gnosis-test-nodes--cursor "server" 9))
         (page `(:version 1 :snapshot_id "snapshot"
                 :cursor ,cut :next nil
                 :items [(:entity_kind "settings" :projection (:version 1))])))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-sync-snapshot) (lambda (_) page)))
          (setq state (gnosis-nodes--snapshot state))
          (dotimes (_ 20)
            (when (plist-get state :snapshot)
              (setq state (gnosis-nodes--snapshot (gnosis-nodes--read-state)))))
          (should-not (plist-get state :snapshot))
          (should (equal cut (plist-get state :cursor))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-replays-accept-remote-after-state-write-crash ()
  (dolist (case '(("update" "note.org" "note.org" "remote")
                  ("rename" "old.org" "new.org" "remote")
                  ("tombstone" "note.org" "note.org" nil)))
    (let* ((directory (make-temp-file "gnosis-nodes-accept-replay" t))
           (gnosis-nodes-dir directory)
           (user-emacs-directory (file-name-as-directory directory))
           (gnosis-nodes-state-file nil)
           (operation (nth 0 case))
           (local-path (nth 1 case))
           (remote-path (nth 2 case))
           (remote-bytes (and (nth 3 case) (string-to-unibyte (nth 3 case))))
           (remote-digest (and remote-bytes (gnosis-nodes--digest remote-bytes)))
           (projection `(:document_id "doc" :revision 2
                         :logical_path ,remote-path
                         :content_sha256 ,remote-digest
                         :operation ,operation))
           (remote-file (expand-file-name "remote.org" directory))
           (record `(:document-id "doc" :kind ,(intern (concat ":" operation))
                     :local-path ,local-path :logical-path ,remote-path
                     :local-digest ,(gnosis-nodes--digest "local")
                     :remote-digest ,remote-digest :remote-revision 2
                     :remote-file ,remote-file :projection ,projection
                     :resolution :accept-remote))
           (entry '(:operation-id "local" :status :rejected
                    :document-id "doc"))
           (state `(:bases [(:document-id "doc" :revision 1
                             :logical-path ,local-path :digest "old" :active t)]
                    :outbox [,entry] :conflicts [,record])))
      (unwind-protect
          (progn
            (gnosis-nodes--write-document local-path (string-to-unibyte "local"))
            (when remote-bytes
              (gnosis-nodes--write-bytes remote-file remote-bytes))
            (cl-letf (((symbol-function 'gnosis-nodes--conflict-directory)
                       (lambda (_) directory)))
              (setq state (gnosis-nodes--apply-document state projection)))
            (should (= 0 (length (plist-get state :conflicts))))
            (should (eq :superseded
                        (plist-get (aref (plist-get state :outbox) 0) :status)))
            (should (= 2 (plist-get (aref (plist-get state :bases) 0)
                                    :revision))))
        (delete-directory directory t)))))

(ert-deftest gnosis-nodes-reverted-save-reconciles-existing-intent ()
  (let* ((bytes (string-to-unibyte "base"))
         (digest (gnosis-nodes--digest bytes))
         (base `(:document-id "doc" :revision 1 :logical-path "note.org"
                 :digest ,digest :active t))
         (pending (gnosis-nodes--outbox-entry
                   :replace "doc" 1 "note.org" digest nil
                   (string-to-unibyte "changed")))
         (state `(:outbox [,pending])))
    (setq state (gnosis-nodes--queue-save state base bytes))
    (should (eq :superseded (plist-get pending :status)))
    (setf (plist-get pending :status) :applied)
    (setq state (gnosis-nodes--queue-save state base bytes))
    (should (= 2 (length (plist-get state :outbox))))
    (should (eq :blocked
                (plist-get (aref (plist-get state :outbox) 1) :status)))))

(ert-deftest gnosis-nodes-unchanged-save-preserves-lifecycle-intent ()
  (let* ((bytes (string-to-unibyte "base"))
         (digest (gnosis-nodes--digest bytes))
         (base `(:document-id "doc" :revision 1 :logical-path "note.org"
                 :digest ,digest :active t)))
    (dolist (kind '(:rename :delete))
      (let* ((entry (gnosis-nodes--outbox-entry
                     kind "doc" 1 "note.org" digest "old.org"))
             (state `(:outbox [,entry])))
        (setq state (gnosis-nodes--queue-save state base bytes))
        (should (= 1 (length (plist-get state :outbox))))
        (should (eq :pending (plist-get entry :status)))))))

(ert-deftest gnosis-nodes-unchanged-receipt-reconciles-outbox ()
  (let* ((digest (gnosis-nodes--digest "same"))
         (authoritative `(:document_id "doc" :revision 3
                          :logical_path "note.org"
                          :content_sha256 ,digest :active t
                          :operation "update"))
         (base `(:document-id "doc" :revision 3 :logical-path "note.org"
                 :digest ,digest :active t
                 :fingerprint ,(gnosis-nodes--projection-fingerprint authoritative)))
         (entry (gnosis-nodes--outbox-entry
                 :replace "doc" 3 "note.org" digest nil "same"))
         (dependent (gnosis-nodes--outbox-entry
                     :replace "doc" nil "note.org" digest nil "same"))
         (state nil))
    (setf (plist-get dependent :status) :blocked
          (plist-get dependent :depends-on) (plist-get entry :operation-id)
          state `(:bases [,base] :outbox [,entry ,dependent] :conflicts []))
    (cl-letf (((symbol-function 'gnosis-nodes--write-state) #'ignore)
              ((symbol-function 'gnosis-nodes--send-entry)
               (lambda (_)
                 `(:format_version 1 :status "applied"
                   :operation_id ,(plist-get entry :operation-id)
                   :changed ,gnosis-core--json-false :cursor "same"
                   :authoritative ,(copy-tree authoritative)))))
      (setq state (gnosis-nodes--send-outbox state))
      (should (= 1 (length (plist-get state :outbox))))
      (let ((promoted (aref (plist-get state :outbox) 0)))
        (should (eq :pending (plist-get promoted :status)))
        (should (= 3 (plist-get promoted :revision)))
        (should (equal (plist-get promoted :operation-id)
                       (plist-get dependent :operation-id)))))))

(ert-deftest gnosis-nodes-materializes-document-snapshot-projection ()
  (let* ((directory (make-temp-file "gnosis-nodes-document-snapshot" t))
         (gnosis-nodes-dir directory)
         (bytes (string-to-unibyte "snapshot"))
         (digest (gnosis-nodes--digest bytes))
         (projection `(:document_id "doc" :revision 4
                       :logical_path "note.org" :content_sha256 nil
                       :blob_url nil :operation "snapshot"
                       :projection (:document_id "doc" :revision 4
                                    :logical_path "note.org"
                                    :content_sha256 ,digest :active t)))
         (state '(:bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-blob)
                   (lambda (requested)
                     (should (equal digest requested))
                     bytes)))
          (setq state (gnosis-nodes--apply-document state projection))
          (should (equal bytes (gnosis-nodes--read-bytes
                                (expand-file-name "note.org" directory))))
          (should (equal digest
                         (plist-get (aref (plist-get state :bases) 0) :digest))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-validates-document-operation-coupling ()
  (should-not
   (gnosis-nodes--valid-document-p
    '(:document_id "doc" :revision 1 :logical_path "note.org"
      :content_sha256 nil :blob_url nil :operation "create"
      :projection nil)))
  (should-not
   (gnosis-nodes--valid-document-p
    '(:document_id "doc" :revision 1 :logical_path "note.org"
      :content_sha256 "digest" :blob_url "/wrong" :operation "update"
      :projection (:content_sha256 "other")))))

(ert-deftest gnosis-nodes-local-scan-rejects-non-regular-enrolled-path ()
  (let* ((directory (make-temp-file "gnosis-nodes-scan-directory" t))
         (gnosis-nodes-dir directory)
         (path (expand-file-name "note.org" directory))
         (base '(:document-id "doc" :revision 1 :logical-path "note.org"
                 :digest "old" :active t))
         (state `(:bases [,base] :outbox [] :conflicts [] :scan-index 0)))
    (unwind-protect
        (progn
          (make-directory path)
          (should-error (gnosis-nodes--scan state)
                        :type 'gnosis-nodes-path-error))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-budget-charges-empty-pages ()
  (let* ((directory (make-temp-file "gnosis-nodes-empty-pages" t))
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (gnosis-nodes-max-snapshot-actions-per-pass 1)
         paths)
    (unwind-protect
        (progn
          (dotimes (index 3)
            (let* ((file (gnosis-nodes--snapshot-spool-file index))
                   (bytes (encode-coding-string
                           (json-serialize
                            (gnosis-nodes--json-value
                             '(:version 1 :snapshot_id "s" :cursor "cut"
                               :next nil :items [])))
                           'utf-8-unix)))
              (make-directory (file-name-directory file) t)
              (gnosis-nodes--write-bytes file bytes)
              (push (list :file file :sha256 (gnosis-nodes--digest bytes))
                    paths)))
          (let ((state `(:snapshot (:stage "manifest" :pages ,(vconcat (nreverse paths))
                                    :page-index 0 :item-index 0))))
            (cl-letf (((symbol-function 'gnosis-nodes--write-state) #'ignore))
              (setq state (gnosis-nodes--finalize-snapshot state nil)))
            (should (= 1 (plist-get (plist-get state :snapshot) :page-index)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-replays-partial-accepted-rename ()
  (let* ((directory (make-temp-file "gnosis-nodes-partial-rename" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (local (string-to-unibyte "local"))
         (remote (string-to-unibyte "remote"))
         (remote-digest (gnosis-nodes--digest remote))
         (remote-file (expand-file-name
                       "remote" (gnosis-nodes--conflict-directory "doc")))
         (projection `(:document_id "doc" :revision 2 :logical_path "new.org"
                       :content_sha256 ,remote-digest :operation "rename"
                       :projection (:content_sha256 ,remote-digest)))
         (record `(:document-id "doc" :kind :rename :resolution :accept-remote
                   :local-path "old.org" :logical-path "new.org"
                   :local-digest ,(gnosis-nodes--digest local)
                   :remote-digest ,remote-digest :remote-revision 2
                   :remote-file ,remote-file :projection ,projection))
         (state `(:bases [(:document-id "doc" :revision 1
                           :logical-path "old.org" :digest "old" :active t)]
                  :outbox [] :conflicts [,record])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "old.org" local)
          (gnosis-nodes--write-document "new.org" remote)
          (gnosis-nodes--write-bytes remote-file remote)
          (should-error (gnosis-nodes--apply-document state projection)
                        :type 'gnosis-nodes-conflict)
          (should (file-exists-p (expand-file-name "old.org" directory)))
          (should (= 1 (length (plist-get state :conflicts)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-state-preserves-protocol-false-in-receipts ()
  (let* ((directory (make-temp-file "gnosis-nodes-state-false" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (projection `(:document_id "doc" :revision 2 :logical_path "note.org"
                       :content_sha256 nil :active ,gnosis-core--json-false
                       :operation "tombstone"))
         (entry `(:operation-id "op" :status :applied :document-id "doc"
                  :result (:status "applied" :authoritative ,projection)))
         (record `(:document-id "doc" :projection ,projection))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "cursor" :bases [] :outbox [,entry]
                  :conflicts [,record])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (let* ((stored (gnosis-nodes--read-state))
                 (stored-entry (aref (plist-get stored :outbox) 0))
                 (stored-record (aref (plist-get stored :conflicts) 0)))
            (should (eq gnosis-core--json-false
                        (plist-get
                         (plist-get (plist-get stored-entry :result) :authoritative)
                         :active)))
            (should (gnosis-nodes--entry-confirms-conflict-p
                     stored-entry stored-record))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-state-round-trips-unicode-and-rejects-invalid-utf-8 ()
  (let* ((directory (make-temp-file "gnosis-nodes-state-utf8" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "διακομιστής" :device-id "συσκευή"
                  :cursor "cursor" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (should (equal "διακομιστής"
                         (plist-get (gnosis-nodes--read-state) :server-id)))
          (gnosis-nodes--write-bytes
           (gnosis-nodes--state-file)
           (concat (string-to-unibyte "{\"version\":1,\"server-id\":\"")
                   (unibyte-string #xff)
                   (string-to-unibyte "\"}")))
          (should-error (gnosis-nodes--read-state) :type 'gnosis-nodes-error))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-accept-remote-finishes-post-effect-restart ()
  (let* ((directory (make-temp-file "gnosis-nodes-accept-post-effect" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "remote"))
         (digest (gnosis-nodes--digest bytes))
         (projection `(:document_id "doc" :revision 2 :logical_path "note.org"
                       :content_sha256 ,digest :blob_url ,(concat "/v1/blobs/" digest)
                       :operation "update" :projection (:active t)))
         (record `(:document-id "doc" :kind :update :local-path "note.org"
                   :logical-path "note.org" :local-digest "old"
                   :remote-digest ,digest :remote-revision 2
                   :projection ,projection :resolution :accept-remote))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "after" :bases [] :outbox [] :conflicts [,record])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" bytes)
          (gnosis-nodes--write-state state)
          (setq state (gnosis-nodes--read-state)
                record (aref (plist-get state :conflicts) 0)
                state (gnosis-nodes--accept-remote-conflict state record))
          (should (= 0 (length (plist-get state :conflicts))))
          (should (= 2 (plist-get (aref (plist-get state :bases) 0) :revision))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-corrupt-snapshot-restarts-from-first-page ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot-restart" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (first-file (gnosis-nodes--snapshot-spool-file 0))
         (last-file (gnosis-nodes--snapshot-spool-file 1))
         (first-bytes (encode-coding-string
                       (json-serialize '(:version 1 :snapshot_id "s" :cursor nil
                                         :next "two" :items [])) 'utf-8 t))
         (last-bytes (encode-coding-string
                      (json-serialize '(:version 1 :snapshot_id "s" :cursor nil
                                        :next nil :items [])) 'utf-8 t))
         (state `(:version 1 :server-id "server" :device-id "device"
                  :cursor "before" :bases [] :outbox [] :conflicts []
                  :snapshot (:snapshot-id "s" :next nil :cursor nil
                             :seen ["two"]
                             :pages [(:file ,first-file :sha256 "wrong")
                                     (:file ,last-file
                                      :sha256 ,(gnosis-nodes--digest last-bytes))]
                             :blobs [])))
         calls)
    (unwind-protect
        (progn
          (gnosis-nodes--write-bytes first-file first-bytes)
          (gnosis-nodes--write-bytes last-file last-bytes)
          (gnosis-nodes--write-state state)
          (should-error (gnosis-nodes--snapshot (gnosis-nodes--read-state))
                        :type 'gnosis-nodes-error)
          (should-not (plist-get (gnosis-nodes--read-state) :snapshot))
          (cl-letf (((symbol-function 'gnosis-core-sync-snapshot)
                     (lambda (next)
                       (push next calls)
                       `(:version 1 :snapshot_id "fresh"
                         :cursor ,(gnosis-test-nodes--cursor "server" 9)
                         :next nil :items []))))
            (gnosis-nodes--snapshot (gnosis-nodes--read-state)))
          (should (equal '(nil) calls)))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-spools-nested-document-blob ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot-blob" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "snapshot"))
         (digest (gnosis-nodes--digest bytes))
         (item `(:entity_kind "document"
                 :projection (:document_id "doc" :revision 1
                              :logical_path "note.org" :operation "snapshot"
                              :projection (:content_sha256 ,digest :active t))))
         requested)
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-blob)
                   (lambda (sha256)
                     (setq requested sha256)
                     bytes))
                  ((symbol-function 'gnosis-nodes--write-state) #'ignore))
          (let ((blobs
                 (cdr (gnosis-nodes--snapshot-blobs
                       '(:snapshot (:blobs []))
                       (list (list :items (vector item)))))))
            (should (equal digest requested))
            (should (equal digest (caar blobs)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-default-state-path-is-absolute-and-confined ()
  (let ((user-emacs-directory "~/.emacs.d/")
        (gnosis-nodes-state-file nil))
    (should (equal (expand-file-name "~/.emacs.d/gnosis/sync/state.json")
                   (gnosis-nodes--state-file)))))

(ert-deftest gnosis-nodes-unresolved-conflict-preserves-lifecycle-intent ()
  (let* ((directory (make-temp-file "gnosis-nodes-unresolved" t))
         (gnosis-nodes-dir directory)
         (bytes (string-to-unibyte "same"))
         (digest (gnosis-nodes--digest bytes)))
    (unwind-protect
        (dolist (kind '(:rename :tombstone))
          (let* ((operation (if (eq kind :tombstone) "tombstone" "update"))
                 (projection
                  (list :document_id "doc" :revision 2
                        :logical_path "note.org"
                        :content_sha256
                        (unless (eq kind :tombstone) digest)
                        :operation operation))
                 (entry (list :operation-id "lifecycle" :kind kind
                              :document-id "doc" :status :pending))
                 (conflict
                  (list :document-id "doc" :local-path "note.org"
                        :logical-path "note.org" :local-digest digest
                        :remote-digest
                        (unless (eq kind :tombstone) digest)
                        :remote-revision 2 :projection projection))
                 (state (list :bases [] :outbox (vector entry)
                              :conflicts (vector conflict))))
            (if (eq kind :tombstone)
                (when (file-exists-p (expand-file-name "note.org" directory))
                  (delete-file (expand-file-name "note.org" directory)))
              (gnosis-nodes--write-document "note.org" bytes))
            (setq state (gnosis-nodes--apply-document state projection))
            (should (= 1 (length (plist-get state :conflicts))))
            (should (eq :pending (plist-get entry :status)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-newer-remote-head-updates-unresolved-conflict ()
  (let* ((directory (make-temp-file "gnosis-nodes-new-head" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (entry '(:operation-id "rename" :kind :rename
                  :document-id "doc" :status :pending))
         (old '(:document-id "doc" :kind :tombstone
                :resolution :accept-remote
                :local-path "note.org" :logical-path "note.org"
                :local-digest nil :remote-digest nil :remote-revision 2
                :projection (:document_id "doc" :revision 2
                             :logical_path "note.org"
                             :content_sha256 nil :operation "tombstone")))
         (projection '(:document_id "doc" :revision 3
                       :logical_path "note.org"
                       :content_sha256 nil :operation "tombstone"))
         (state `(:bases [] :outbox [,entry] :conflicts [,old])))
    (unwind-protect
        (progn
          (setq state (gnosis-nodes--apply-document state projection))
          (should (= 3 (plist-get (aref (plist-get state :conflicts) 0)
                                  :remote-revision)))
          (should-not (plist-get (aref (plist-get state :conflicts) 0)
                                 :resolution))
          (should (eq :pending (plist-get entry :status))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-managed-bytes-rejects-unsafe-state-paths-before-read ()
  (let* ((root (make-temp-file "gnosis-nodes-managed" t))
         (outside (make-temp-file "gnosis-nodes-outside" nil nil "outside"))
         (regular (expand-file-name "regular" root))
         (directory (expand-file-name "directory" root))
         (link (expand-file-name "link" root))
         read)
    (unwind-protect
        (progn
          (gnosis-nodes--write-bytes regular "inside")
          (make-directory directory)
          (make-symbolic-link outside link)
          (cl-letf (((symbol-function 'gnosis-nodes--read-bytes)
                     (lambda (path)
                       (setq read path)
                       "inside")))
            (should (equal "inside" (gnosis-nodes--managed-bytes root regular)))
            (dolist (path (list outside directory link))
              (setq read nil)
              (should-error (gnosis-nodes--managed-bytes root path)
                            :type 'gnosis-nodes-path-error)
              (should-not read)))
          (let ((gnosis-nodes-state-file directory))
            (should-error (gnosis-nodes--read-state)
                          :type 'gnosis-nodes-path-error)))
      (delete-directory root t)
      (delete-file outside))))

(ert-deftest gnosis-nodes-recovered-snapshot-validation-is-page-bounded ()
  (let* ((gnosis-nodes-max-pages-per-pass 1)
         (records [(:file "one") (:file "two") (:file "three")])
         (state `(:snapshot (:snapshot-id "snapshot" :next nil
                             :seen [] :pages ,records :blobs [])))
         (reads 0))
    (cl-letf (((symbol-function 'gnosis-nodes--read-snapshot-page)
               (lambda (_record)
                 (cl-incf reads)
                 `(:version 1 :snapshot_id "snapshot" :cursor nil
                   :next ,(gnosis-test-nodes--cursor
                           "server" reads "snapshot")
                   :items [])))
              ((symbol-function 'gnosis-nodes--write-state) #'ignore))
      (let ((result (gnosis-nodes--snapshot state)))
        (should (= 1 reads))
        (should (= 1 (plist-get (plist-get result :snapshot)
                                :validated-pages)))))))

(ert-deftest gnosis-nodes-change-page-rejects-foreign-or-regressing-cursor ()
  (let* ((from (gnosis-test-nodes--cursor "server" 4))
         (foreign (gnosis-test-nodes--cursor "other" 5))
         (regressing (gnosis-test-nodes--cursor "server" 3)))
    (dolist (next (list foreign regressing))
      (should-not
       (gnosis-nodes--change-page-valid-p
        `(:version 1 :from ,from :next ,next
          :has_more ,gnosis-core--json-false
          :items [(:cursor ,next :entity_kind "asset" :projection (:id "a"))])
        from)))))

(ert-deftest gnosis-nodes-snapshot-page-rejects-foreign-or-replayed-continuation ()
  (let* ((requested (gnosis-test-nodes--cursor "server" 4 "snapshot"))
         (foreign (gnosis-test-nodes--cursor "other" 5 "snapshot"))
         (replayed (gnosis-test-nodes--cursor "server" 4 "snapshot")))
    (dolist (next (list foreign replayed))
      (should-not
       (gnosis-nodes--snapshot-page-valid-p
        `(:version 1 :snapshot_id "snapshot" :cursor nil
          :next ,next :items [])
        "snapshot" requested)))))

(ert-deftest gnosis-nodes-scan-surfaces-missing-enrolled-file ()
  (let* ((directory (make-temp-file "gnosis-nodes-missing" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "remote"))
         (digest (gnosis-nodes--digest bytes))
         (base `(:document-id "doc" :revision 2 :logical-path "note.org"
                 :digest ,digest :active t))
         (state `(:bases [,base] :outbox [] :conflicts [] :scan-index 0)))
    (unwind-protect
        (cl-letf (((symbol-function 'gnosis-core-blob) (lambda (_) bytes))
                  ((symbol-function 'gnosis-nodes--write-state) #'ignore))
          (setq state (gnosis-nodes--scan state))
          (should (= 1 (length (plist-get state :conflicts))))
          (should (eq :missing-local
                      (plist-get (aref (plist-get state :conflicts) 0) :kind))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-accepted-rename-requires-source-absence ()
  (let* ((directory (make-temp-file "gnosis-nodes-rename-effect" t))
         (gnosis-nodes-dir directory)
         (bytes (string-to-unibyte "remote"))
         (digest (gnosis-nodes--digest bytes))
         (projection `(:document_id "doc" :revision 2 :logical_path "new.org"
                       :content_sha256 ,digest :operation "rename"))
         (record `(:document-id "doc" :local-path "old.org"
                   :logical-path "new.org" :remote-digest ,digest
                   :local-digest ,(gnosis-nodes--digest "local")
                   :resolution :accept-remote)))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "old.org" "local")
          (gnosis-nodes--write-document "new.org" bytes)
          (should-not (gnosis-nodes--accepted-remote-effect-p record projection))
          (should (file-exists-p (expand-file-name "old.org" directory))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-rename-refuses-visited-destination-buffer ()
  (let* ((directory (make-temp-file "gnosis-nodes-visited-target" t))
         (gnosis-nodes-dir directory)
         (destination (expand-file-name "new.org" directory))
         buffer)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "old.org" "old")
          (setq buffer (find-file-noselect destination))
          (should-error (gnosis-nodes--move-document "old.org" "new.org" "remote")
                        :type 'gnosis-nodes-error)
          (should (buffer-live-p buffer))
          (should-not (file-exists-p destination)))
      (when (buffer-live-p buffer) (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-abandoned-ediff-session-removes-only-disposable-state ()
  (let* ((directory (make-temp-file "gnosis-nodes-ediff-quit" t))
         (gnosis-nodes-dir directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (session-id "abandoned")
         (merge-file (expand-file-name
                      "merge" (gnosis-nodes--conflict-directory "doc")))
         (control-buffer (generate-new-buffer " *gnosis-ediff-control*"))
         (remote-buffer (generate-new-buffer " *gnosis-ediff-remote*"))
         (record `(:document-id "doc" :logical-path "note.org"
                   :local-path "note.org" :remote-revision 2
                   :remote-digest "remote" :status :editing
                   :merge-start-digest "start" :merge-file ,merge-file
                   :merge-session-id ,session-id))
         (state `(:bases [] :outbox [] :conflicts [,record]))
         (lock-attempts 0)
         pending)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" "original")
          (gnosis-nodes--write-bytes merge-file "partial merge")
          (puthash session-id
                   (list :document-id "doc" :merge-file merge-file
                         :control-buffer control-buffer
                         :disposable-buffers (list remote-buffer))
                   gnosis-nodes--ediff-sessions)
          (cl-letf (((symbol-function 'gnosis-nodes--read-state)
                     (lambda () state))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value) (setq state value)))
                    ((symbol-function 'lock-file)
                     (lambda (_)
                       (cl-incf lock-attempts)
                       (when (= lock-attempts 1)
                         (signal 'gnosis-nodes-busy '("busy")))))
                    ((symbol-function 'unlock-file) #'ignore)
                    ((symbol-function 'run-at-time)
                     (lambda (_delay _repeat function &rest args)
                       (setq pending (cons function args)))))
            (gnosis-nodes--abandon-ediff-session session-id)
            (should pending)
            (should (gethash session-id gnosis-nodes--ediff-sessions))
            (should (file-exists-p merge-file))
            (should (eq :editing
                        (plist-get (aref (plist-get state :conflicts) 0)
                                   :status)))
            (apply (car pending) (cdr pending))
            (should-error (gnosis-nodes-accept-conflict) :type 'user-error))
          (should-not (file-exists-p merge-file))
          (should-not (buffer-live-p remote-buffer))
          (should (buffer-live-p control-buffer))
          (should (equal "original"
                         (gnosis-nodes--managed-bytes
                          directory (expand-file-name "note.org" directory))))
          (let ((updated (aref (plist-get state :conflicts) 0)))
            (should (eq :unresolved (plist-get updated :status)))
            (should-not (plist-get updated :merge-file))
            (should-not (plist-get updated :merge-session-id))))
      (remhash session-id gnosis-nodes--ediff-sessions)
      (dolist (buffer (list control-buffer remote-buffer))
        (when (buffer-live-p buffer) (kill-buffer buffer)))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-accepted-remote-replay-rejects-visited-destination ()
  (let* ((directory (make-temp-file "gnosis-nodes-accepted-visited" t))
         (gnosis-nodes-dir directory)
         (remote (string-to-unibyte "remote"))
         (digest (gnosis-nodes--digest remote))
         (projection `(:operation "update" :logical_path "new.org"
                       :content_sha256 ,digest))
         (record `(:kind :rename :local-path "old.org"
                   :remote-path "new.org" :remote-digest ,digest))
         buffer)
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "new.org" remote)
          (setq buffer (find-file-noselect
                        (expand-file-name "new.org" directory)))
          (should-not (gnosis-nodes--accepted-remote-effect-p
                       record projection))
          (with-current-buffer buffer (set-buffer-modified-p t))
          (should-not (gnosis-nodes--accepted-remote-effect-p
                       record projection))
          (should (equal remote
                         (gnosis-nodes--managed-bytes
                          directory (expand-file-name "new.org" directory)))))
      (when (buffer-live-p buffer)
        (with-current-buffer buffer (set-buffer-modified-p nil))
        (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-cursor-codec-matches-rust-domain ()
  (let* ((maximum (1- (expt 2 64)))
         (cursor (gnosis-test-nodes--cursor "server" maximum))
         (snapshot (gnosis-test-nodes--cursor "server" maximum "snapshot"))
         (padded (concat cursor "="))
         (overflow (gnosis-test-nodes--encode-cursor-json
                    (format "{\"version\":1,\"server_id\":\"server\",\"position\":%s,\"token\":\"token\"}"
                            (1+ maximum))))
         (float-version (gnosis-test-nodes--encode-cursor-json
                         "{\"version\":1.0,\"server_id\":\"server\",\"position\":1,\"token\":\"token\"}"))
         (float-position (gnosis-test-nodes--encode-cursor-json
                          "{\"version\":1,\"server_id\":\"server\",\"position\":1.0,\"token\":\"token\"}"))
         (unknown (gnosis-test-nodes--encode-cursor-json
                   "{\"version\":1,\"server_id\":\"server\",\"position\":1,\"token\":\"token\",\"extra\":1}"))
         (malformed-utf8 (gnosis-test-nodes--encode-cursor-json
                          (unibyte-string #xff))))
    (should (gnosis-nodes--decode-cursor cursor))
    (should (gnosis-nodes--decode-cursor snapshot t))
    (dolist (invalid (list padded "+" float-version float-position
                           overflow unknown malformed-utf8))
      (should-not (gnosis-nodes--decode-cursor invalid)))))

(provide 'gnosis-test-nodes)
;;; gnosis-test-nodes.el ends here
