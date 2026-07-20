;;; gnosis-test-nodes.el --- Org synchronization tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'ert)
(require 'gnosis-nodes)

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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
          (setq state (gnosis-nodes--scan state))
          (should
           (equal '(:replace)
                  (mapcar (lambda (entry) (plist-get entry :kind))
                          (append (plist-get state :outbox) nil)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-scan-never-infers-rename-from-file-layout ()
  (let* ((root (make-temp-file "gnosis-nodes-rename-" t))
         (gnosis-nodes-directory root)
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
          (should (= 0 (length (plist-get (gnosis-nodes--scan state)
                                          :outbox)))))
      (delete-directory root t))))

(ert-deftest gnosis-nodes-outbox-replays-captured-bytes-and-stops-on-uncertain ()
  (let* ((directory (make-temp-file "gnosis-nodes-replay" t))
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
          (setq buffer (find-file-noselect path))
          (cl-letf (((symbol-function 'gnosis-core-sync-state)
                     (lambda () (setq http-called t))))
            (with-current-buffer buffer
              (gnosis-nodes-mode 1)
              (run-hooks 'after-save-hook)))
          (should-not http-called)
          (should (= 1 (length (plist-get (gnosis-nodes--read-state) :outbox)))))
      (when (buffer-live-p buffer) (kill-buffer buffer))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-snapshot-fetches-all-pages-before-materializing ()
  (let* ((directory (make-temp-file "gnosis-nodes-snapshot" t))
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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

(ert-deftest gnosis-nodes-track-all-files-requires-explicit-enrollment ()
  (let* ((directory (make-temp-file "gnosis-nodes-all" t))
         (gnosis-nodes-directory directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (gnosis-nodes--write-document "a.org" "a")
          (gnosis-nodes--write-document "nested/b.org" "b")
          (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t)))
            (gnosis-nodes-track-all-files))
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
         (gnosis-nodes-directory directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (bytes (string-to-unibyte "merged"))
         (digest (gnosis-nodes--digest bytes))
         (record '(:document-id "doc" :logical-path "note.org"
                   :remote-revision 2 :remote-digest "remote"
                   :status :editing :merge-start-digest "before"))
         (state `(:bases [] :outbox [] :conflicts [,record])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-document "note.org" bytes)
          (cl-letf (((symbol-function 'gnosis-nodes--read-state)
                     (lambda () state))
                    ((symbol-function 'gnosis-nodes--write-state)
                     (lambda (value) (setq state value)))
                    ((symbol-function 'gnosis-nodes--remote-conflict-current-p)
                     (lambda (&rest _) t)))
            (gnosis-nodes-accept-conflict))
          (should (= 1 (length (plist-get state :conflicts))))
          (should (equal digest
                         (plist-get (aref (plist-get state :conflicts) 0)
                                    :accepted-digest))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-track-all-files-skips-symlink-escape ()
  (let* ((directory (make-temp-file "gnosis-nodes-all-safe" t))
         (outside (make-temp-file "gnosis-nodes-outside" nil ".org" "outside"))
         (gnosis-nodes-directory directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (make-symbolic-link outside (expand-file-name "linked.org" directory))
          (cl-letf (((symbol-function 'yes-or-no-p) (lambda (&rest _) t)))
            (gnosis-nodes-track-all-files))
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
           (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
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
         (gnosis-nodes-directory directory)
         (user-emacs-directory (file-name-as-directory directory))
         (gnosis-nodes-state-file nil)
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "feed" :bases [] :outbox []
                  :conflicts [(:document-id "doc" :logical-path "note.org"
                               :status :queued :operation-id "op")])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (let ((record (aref (plist-get (gnosis-nodes--read-state)
                                         :conflicts)
                              0)))
            (should (eq :queued (plist-get record :status)))
            (should (equal "op" (plist-get record :operation-id)))))
      (delete-directory directory t))))

(provide 'gnosis-test-nodes)
;;; gnosis-test-nodes.el ends here
