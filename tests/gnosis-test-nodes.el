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
   (equal '(:action :delete)
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
         (gnosis-nodes-state-file
          (expand-file-name ".gnosis-sync/state.json" directory))
         (state '(:version 1 :server-id "server" :device-id "device"
                  :cursor "cursor" :bases [] :outbox [] :conflicts [])))
    (unwind-protect
        (progn
          (gnosis-nodes--write-state state)
          (should (equal state (gnosis-nodes--read-state))))
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
         (gnosis-nodes-state-file
          (expand-file-name ".gnosis-sync/state.json" directory))
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

(ert-deftest gnosis-nodes-scan-enqueues-replace-delete-and-create ()
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
           (equal '(:replace :delete :create)
                  (mapcar (lambda (entry) (plist-get entry :kind))
                          (append (plist-get state :outbox) nil)))))
      (delete-directory directory t))))

(ert-deftest gnosis-nodes-scan-recognizes-content-preserving-rename ()
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
          (let* ((next (gnosis-nodes--scan state))
                 (outbox (plist-get next :outbox)))
            (should (= 1 (length outbox)))
            (should (eq :rename (plist-get (aref outbox 0) :kind)))
            (should (equal "new.org"
                           (plist-get (aref outbox 0) :logical-path)))))
      (delete-directory root t))))

(provide 'gnosis-test-nodes)
;;; gnosis-test-nodes.el ends here
